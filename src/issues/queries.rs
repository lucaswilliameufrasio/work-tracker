use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::access::{AccessCapability, AccessStore, AccessStoreError, authorize};

use super::{IssuePriority, IssueRecord, IssueStore};

const MAX_PAGE_SIZE: u16 = 100;

#[derive(Debug, Error)]
pub enum IssueReadError {
    #[error("issue or project is unavailable")]
    Unavailable,
    #[error("issue number must be positive")]
    InvalidIssueNumber,
    #[error("issue-list cursor must be a positive issue number")]
    InvalidCursor,
    #[error("issue-list page size must be between 1 and 100")]
    InvalidPageSize,
    #[error("persisted issue or history data is invalid")]
    CorruptData,
    #[error("persisted authorization state is invalid: {0}")]
    AccessState(#[from] AccessStoreError),
    #[error("issue query failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSummary {
    pub id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub issue_type_id: String,
    pub status_id: String,
    pub title: String,
    pub assignee_user_id: Option<String>,
    pub creator_user_id: String,
    pub priority: IssuePriority,
    pub due_date: Option<NaiveDate>,
    pub rank: i64,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueListPage {
    pub items: Vec<IssueSummary>,
    pub next_after_issue_number: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueEventKind {
    Created,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEvent {
    pub id: String,
    pub project_id: String,
    pub issue_id: String,
    pub actor_user_id: String,
    pub kind: IssueEventKind,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDetails {
    pub issue: IssueRecord,
    pub history: Vec<IssueEvent>,
}

impl IssueStore {
    /// Lists non-trashed issues for an explicitly authorized project member.
    /// Pages are ordered by the immutable project-local issue number.
    pub async fn list_project_issues(
        &self,
        actor_user_id: &str,
        project_id: &str,
        after_issue_number: Option<i64>,
        page_size: u16,
    ) -> Result<IssueListPage, IssueReadError> {
        validate_list_options(after_issue_number, page_size)?;
        let mut transaction = self.pool.begin().await?;
        let page = Self::list_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            after_issue_number,
            page_size,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }

    pub(crate) async fn list_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        project_id: &str,
        after_issue_number: Option<i64>,
        page_size: u16,
    ) -> Result<IssueListPage, IssueReadError> {
        validate_list_options(after_issue_number, page_size)?;
        authorize_project_read(transaction, actor_user_id, project_id).await?;

        let rows = sqlx::query(
            "SELECT issues.id, issues.project_id, issues.issue_number, \
                    projects.project_key, issues.issue_type_id, issues.status_id, \
                    issues.title, issues.assignee_user_id, issues.creator_user_id, \
                    issues.priority, issues.due_date, issues.rank, issues.version, \
                    issues.created_at, issues.updated_at \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             WHERE issues.project_id = ? \
               AND issues.trashed_at IS NULL \
               AND issues.issue_number > COALESCE(?, 0) \
             ORDER BY issues.issue_number ASC \
             LIMIT ?",
        )
        .bind(project_id)
        .bind(after_issue_number)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&mut **transaction)
        .await?;

        let has_more = rows.len() > usize::from(page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(page_size)));
        for row in rows.into_iter().take(usize::from(page_size)) {
            items.push(issue_summary_from_row(&row)?);
        }

        let next_after_issue_number = if has_more {
            items.last().map(|issue| issue.issue_number)
        } else {
            None
        };
        Ok(IssueListPage {
            items,
            next_after_issue_number,
        })
    }

    /// Reads an issue, including one in the project trash, and its current
    /// content-free functional history within the same authorization snapshot.
    pub async fn get_project_issue(
        &self,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
    ) -> Result<IssueDetails, IssueReadError> {
        if issue_number <= 0 {
            return Err(IssueReadError::InvalidIssueNumber);
        }

        let mut transaction = self.pool.begin().await?;
        authorize_project_read(&mut transaction, actor_user_id, project_id).await?;

        let row = sqlx::query(
            "SELECT issues.id, issues.project_id, issues.issue_number, \
                    projects.project_key, issues.issue_type_id, issues.status_id, \
                    issues.title, issues.description, issues.assignee_user_id, \
                    issues.creator_user_id, issues.priority, issues.due_date, \
                    issues.rank, issues.version, issues.completed_at, issues.trashed_at, \
                    issues.created_at, issues.updated_at \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             WHERE issues.project_id = ? AND issues.issue_number = ?",
        )
        .bind(project_id)
        .bind(issue_number)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueReadError::Unavailable)?;
        let issue = issue_record_from_row(&row)?;

        let event_rows = sqlx::query(
            "SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at \
             FROM issue_events \
             WHERE project_id = ? AND issue_id = ? \
             ORDER BY occurred_at ASC, id ASC",
        )
        .bind(project_id)
        .bind(&issue.id)
        .fetch_all(&mut *transaction)
        .await?;
        if event_rows.is_empty() {
            return Err(IssueReadError::CorruptData);
        }

        let mut history = Vec::with_capacity(event_rows.len());
        for event_row in event_rows {
            history.push(issue_event_from_row(&event_row)?);
        }

        transaction.commit().await?;
        Ok(IssueDetails { issue, history })
    }
}

fn validate_list_options(cursor: Option<i64>, page_size: u16) -> Result<(), IssueReadError> {
    if !(1..=MAX_PAGE_SIZE).contains(&page_size) {
        return Err(IssueReadError::InvalidPageSize);
    }
    if cursor.is_some_and(|issue_number| issue_number <= 0) {
        return Err(IssueReadError::InvalidCursor);
    }
    Ok(())
}

async fn authorize_project_read(
    transaction: &mut Transaction<'_, Sqlite>,
    actor_user_id: &str,
    project_id: &str,
) -> Result<(), IssueReadError> {
    let Some(context) = AccessStore::load_project_context_in_transaction(
        transaction,
        actor_user_id,
        project_id,
        false,
    )
    .await?
    else {
        return Err(IssueReadError::Unavailable);
    };

    if authorize(&context, AccessCapability::ReadProjectContent).is_err() {
        return Err(IssueReadError::Unavailable);
    }

    Ok(())
}

fn issue_summary_from_row(row: &SqliteRow) -> Result<IssueSummary, IssueReadError> {
    let issue_number: i64 = row.try_get("issue_number")?;
    let project_key: String = row.try_get("project_key")?;
    let project_id: String = row.try_get("project_id")?;
    let title: String = row.try_get("title")?;
    if issue_number <= 0 {
        return Err(IssueReadError::CorruptData);
    }
    if title.trim().is_empty() || title.chars().count() > 240 {
        return Err(IssueReadError::CorruptData);
    }

    Ok(IssueSummary {
        id: row.try_get("id")?,
        project_id,
        issue_number,
        issue_key: format!("{project_key}-{issue_number}"),
        issue_type_id: row.try_get("issue_type_id")?,
        status_id: row.try_get("status_id")?,
        title,
        assignee_user_id: row.try_get("assignee_user_id")?,
        creator_user_id: row.try_get("creator_user_id")?,
        priority: parse_priority(&row.try_get::<String, _>("priority")?)?,
        due_date: parse_optional_date(row.try_get("due_date")?)?,
        rank: row.try_get("rank")?,
        version: parse_version(row.try_get("version")?)?,
        created_at: parse_timestamp(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_timestamp(&row.try_get::<String, _>("updated_at")?)?,
    })
}

fn issue_record_from_row(row: &SqliteRow) -> Result<IssueRecord, IssueReadError> {
    let summary = issue_summary_from_row(row)?;
    let description: Option<String> = row.try_get("description")?;
    if description
        .as_ref()
        .is_some_and(|value| value.len() > 65_536)
    {
        return Err(IssueReadError::CorruptData);
    }
    Ok(IssueRecord {
        id: summary.id,
        project_id: summary.project_id,
        issue_number: summary.issue_number,
        issue_key: summary.issue_key,
        issue_type_id: summary.issue_type_id,
        status_id: summary.status_id,
        title: summary.title,
        description,
        assignee_user_id: summary.assignee_user_id,
        creator_user_id: summary.creator_user_id,
        priority: summary.priority,
        due_date: summary.due_date,
        rank: summary.rank,
        version: summary.version,
        completed_at: parse_optional_timestamp(row.try_get("completed_at")?)?,
        trashed_at: parse_optional_timestamp(row.try_get("trashed_at")?)?,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
    })
}

fn issue_event_from_row(row: &SqliteRow) -> Result<IssueEvent, IssueReadError> {
    let kind = match row.try_get::<String, _>("event_kind")?.as_str() {
        "issue.created" => IssueEventKind::Created,
        _ => return Err(IssueReadError::CorruptData),
    };

    Ok(IssueEvent {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        issue_id: row.try_get("issue_id")?,
        actor_user_id: row.try_get("actor_user_id")?,
        kind,
        occurred_at: parse_timestamp(&row.try_get::<String, _>("occurred_at")?)?,
    })
}

fn parse_priority(value: &str) -> Result<IssuePriority, IssueReadError> {
    match value {
        "none" => Ok(IssuePriority::None),
        "low" => Ok(IssuePriority::Low),
        "medium" => Ok(IssuePriority::Medium),
        "high" => Ok(IssuePriority::High),
        "urgent" => Ok(IssuePriority::Urgent),
        _ => Err(IssueReadError::CorruptData),
    }
}

fn parse_version(value: i64) -> Result<i64, IssueReadError> {
    if value < 1 {
        return Err(IssueReadError::CorruptData);
    }
    Ok(value)
}

fn parse_optional_date(value: Option<String>) -> Result<Option<NaiveDate>, IssueReadError> {
    value
        .map(|date| {
            NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(|_| IssueReadError::CorruptData)
        })
        .transpose()
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, IssueReadError> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| IssueReadError::CorruptData)
}

fn parse_optional_timestamp(
    value: Option<String>,
) -> Result<Option<DateTime<Utc>>, IssueReadError> {
    value.as_deref().map(parse_timestamp).transpose()
}
