use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    auth::time::format_timestamp,
};

const INITIAL_RANK_STRIDE: i64 = 1_024;
const MAX_DESCRIPTION_BYTES: usize = 65_536;
const MAX_TITLE_CHARS: usize = 240;

mod queries;
pub use queries::{
    IssueDetails, IssueEvent, IssueEventKind, IssueListPage, IssueReadError, IssueSummary,
};

#[derive(Debug, Error)]
pub enum IssueCreationError {
    #[error("issue title must contain 1 to 240 characters")]
    InvalidTitle,
    #[error("issue description exceeds 64 KiB")]
    DescriptionTooLong,
    #[error("project or actor does not exist")]
    UnknownProjectOrActor,
    #[error("actor is not allowed to create issues in this project")]
    Forbidden,
    #[error("assignee must be an active project manager or member")]
    AssigneeUnavailable,
    #[error("the project's initial task type or todo status is unavailable")]
    DefaultSchemaUnavailable,
    #[error("project issue number space is exhausted")]
    IssueNumberExhausted,
    #[error("issue rank space is exhausted")]
    RankSpaceExhausted,
    #[error("persisted authorization state is invalid: {0}")]
    AccessState(#[from] AccessStoreError),
    #[error("issue persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuePriority {
    None,
    Low,
    Medium,
    High,
    Urgent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRecord {
    pub id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub issue_type_id: String,
    pub status_id: String,
    pub title: String,
    pub description: Option<String>,
    pub assignee_user_id: Option<String>,
    pub creator_user_id: String,
    pub priority: IssuePriority,
    pub due_date: Option<NaiveDate>,
    pub rank: i64,
    pub version: i64,
    pub completed_at: Option<DateTime<Utc>>,
    pub trashed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewIssue<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub assignee_user_id: Option<&'a str>,
}

#[derive(Clone)]
pub struct IssueStore {
    pub(super) pool: SqlitePool,
}

impl IssueStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Creates an issue for an already-authenticated actor. The use case
    /// reloads current project access in the write transaction; callers must
    /// not treat an arbitrary user ID as an authenticated principal.
    pub async fn create(
        &self,
        actor_user_id: &str,
        project_id: &str,
        input: NewIssue<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueRecord, IssueCreationError> {
        let title = input.title.trim();
        let title_chars = title.chars().count();
        if !(1..=MAX_TITLE_CHARS).contains(&title_chars) {
            return Err(IssueCreationError::InvalidTitle);
        }
        if input
            .description
            .is_some_and(|description| description.len() > MAX_DESCRIPTION_BYTES)
        {
            return Err(IssueCreationError::DescriptionTooLong);
        }

        let timestamp = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            false,
        )
        .await?;
        let Some(context) = context else {
            transaction.rollback().await?;
            return Err(IssueCreationError::UnknownProjectOrActor);
        };
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            transaction.rollback().await?;
            return Err(IssueCreationError::Forbidden);
        }

        let issue_type_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM issue_types \
             WHERE project_id = ? AND system_key = 'task' AND archived_at IS NULL",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueCreationError::DefaultSchemaUnavailable)?;

        let status_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM statuses \
             WHERE project_id = ? AND system_key = 'todo' AND archived_at IS NULL",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueCreationError::DefaultSchemaUnavailable)?;

        if let Some(assignee_user_id) = input.assignee_user_id {
            let eligible_assignee: i64 = sqlx::query_scalar(
                "SELECT EXISTS ( \
                     SELECT 1 \
                     FROM project_members \
                     JOIN users ON users.id = project_members.user_id \
                     WHERE project_members.project_id = ? \
                       AND project_members.user_id = ? \
                       AND project_members.role IN ('manager', 'member') \
                       AND users.state = 'active' \
                 )",
            )
            .bind(project_id)
            .bind(assignee_user_id)
            .fetch_one(&mut *transaction)
            .await?;
            if eligible_assignee != 1 {
                transaction.rollback().await?;
                return Err(IssueCreationError::AssigneeUnavailable);
            }
        }

        let project_row = sqlx::query(
            "SELECT project_key, next_issue_number \
             FROM projects \
             WHERE id = ? AND archived_at IS NULL",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(project_row) = project_row else {
            transaction.rollback().await?;
            return Err(IssueCreationError::Forbidden);
        };
        let project_key: String = project_row.try_get("project_key")?;
        let issue_number: i64 = project_row.try_get("next_issue_number")?;
        let next_issue_number = issue_number
            .checked_add(1)
            .ok_or(IssueCreationError::IssueNumberExhausted)?;

        let current_max_rank = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(rank) FROM issues \
             WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL",
        )
        .bind(project_id)
        .bind(&status_id)
        .fetch_one(&mut *transaction)
        .await?;
        let rank = current_max_rank
            .unwrap_or(0)
            .checked_add(INITIAL_RANK_STRIDE)
            .ok_or(IssueCreationError::RankSpaceExhausted)?;

        let issue_id = Uuid::now_v7().to_string();
        let updated = sqlx::query(
            "UPDATE projects SET next_issue_number = ? \
             WHERE id = ? AND next_issue_number = ? AND archived_at IS NULL",
        )
        .bind(next_issue_number)
        .bind(project_id)
        .bind(issue_number)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(IssueCreationError::Forbidden);
        }

        sqlx::query(
            "INSERT INTO issues \
             (id, project_id, issue_number, issue_type_id, status_id, title, \
              description, assignee_user_id, creator_user_id, priority, due_date, \
              rank, version, completed_at, trashed_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'none', NULL, ?, 1, NULL, NULL, ?, ?)",
        )
        .bind(&issue_id)
        .bind(project_id)
        .bind(issue_number)
        .bind(&issue_type_id)
        .bind(&status_id)
        .bind(title)
        .bind(input.description)
        .bind(input.assignee_user_id)
        .bind(actor_user_id)
        .bind(rank)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            "INSERT INTO issue_events \
             (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) \
             VALUES (?, ?, ?, ?, 'issue.created', ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(project_id)
        .bind(&issue_id)
        .bind(actor_user_id)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        Ok(IssueRecord {
            id: issue_id,
            project_id: project_id.to_owned(),
            issue_number,
            issue_key: format!("{project_key}-{issue_number}"),
            issue_type_id,
            status_id,
            title: title.to_owned(),
            description: input.description.map(str::to_owned),
            assignee_user_id: input.assignee_user_id.map(str::to_owned),
            creator_user_id: actor_user_id.to_owned(),
            priority: IssuePriority::None,
            due_date: None,
            rank,
            version: 1,
            completed_at: None,
            trashed_at: None,
            created_at: now,
            updated_at: now,
        })
    }
}
