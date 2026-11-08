use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    organization_timezone::OrganizationTimezone,
};

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
    #[error("issue history cursor is invalid")]
    InvalidHistoryCursor,
    #[error("board cursor is invalid")]
    InvalidBoardCursor,
    #[error("issue-search cursor is invalid")]
    InvalidSearchCursor,
    #[error("issue-search query is invalid")]
    InvalidSearchQuery,
    #[error("issue-search type filter is invalid")]
    InvalidIssueType,
    #[error("issue-search status filter is invalid")]
    InvalidIssueStatus,
    #[error("issue-search assignee filter is invalid")]
    InvalidIssueAssignee,
    #[error("issue-list page size must be between 1 and 100")]
    InvalidPageSize,
    #[error("persisted issue or history data is invalid")]
    CorruptData,
    #[error("organization timezone is unavailable")]
    OrganizationTimezoneUnavailable,
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
    pub is_overdue: bool,
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
    TitleUpdated,
    DescriptionUpdated,
    TextUpdated,
    Moved,
    FieldUpdated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueFieldChange {
    Priority {
        before: super::IssuePriority,
        after: super::IssuePriority,
    },
    DueDate {
        before: Option<NaiveDate>,
        after: Option<NaiveDate>,
    },
    Assignee {
        before: Option<String>,
        after: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEvent {
    pub id: String,
    pub project_id: String,
    pub issue_id: String,
    pub actor_user_id: String,
    pub kind: IssueEventKind,
    pub change: Option<IssueFieldChange>,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDetails {
    pub issue: IssueRecord,
    pub history: Vec<IssueEvent>,
}

#[derive(Clone)]
pub struct IssueHistoryCursor {
    occurred_at: String,
    event_id: String,
}

impl IssueHistoryCursor {
    pub fn decode(value: &str) -> Result<Self, IssueReadError> {
        if value.len() != 90 {
            return Err(IssueReadError::InvalidHistoryCursor);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| IssueReadError::InvalidHistoryCursor)?;
        if bytes.len() != 67 || URL_SAFE_NO_PAD.encode(&bytes) != value {
            return Err(IssueReadError::InvalidHistoryCursor);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| IssueReadError::InvalidHistoryCursor)?;
        let (time, id) = text
            .split_once('/')
            .ok_or(IssueReadError::InvalidHistoryCursor)?;
        let timestamp = parse_timestamp(time).map_err(|_| IssueReadError::InvalidHistoryCursor)?;
        if time.len() != 30
            || crate::auth::time::format_timestamp(timestamp) != time
            || uuid::Uuid::parse_str(id)
                .map(|id| id.to_string())
                .ok()
                .as_deref()
                != Some(id)
        {
            return Err(IssueReadError::InvalidHistoryCursor);
        }
        Ok(Self {
            occurred_at: time.to_owned(),
            event_id: id.to_owned(),
        })
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}/{}", self.occurred_at, self.event_id))
    }
}

pub struct IssueHistoryPage {
    pub items: Vec<IssueEvent>,
    pub next_cursor: Option<IssueHistoryCursor>,
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
        let now = Utc::now();
        let page = Self::list_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            after_issue_number,
            page_size,
            now,
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
        now: DateTime<Utc>,
    ) -> Result<IssueListPage, IssueReadError> {
        validate_list_options(after_issue_number, page_size)?;
        authorize_project_read(transaction, actor_user_id, project_id).await?;
        let local_today = organization_local_today(transaction, now).await?;

        let rows = sqlx::query(
            "SELECT issues.id, issues.project_id, issues.issue_number, \
                    projects.project_key, issues.issue_type_id, issues.status_id, \
                    issues.title, issues.assignee_user_id, issues.creator_user_id, \
                    issues.priority, issues.due_date, statuses.category AS status_category, \
                    issues.rank, issues.version, \
                    issues.created_at, issues.updated_at \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             JOIN statuses ON statuses.project_id = issues.project_id \
                          AND statuses.id = issues.status_id \
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
            items.push(issue_summary_from_row(&row, local_today)?);
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
        let issue = Self::record_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            issue_number,
            Utc::now(),
        )
        .await?;
        let history = sqlx::query("SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at, field_key, before_value_json, after_value_json FROM issue_events WHERE project_id = ? AND issue_id = ? ORDER BY occurred_at ASC, id ASC")
            .bind(project_id).bind(&issue.id).fetch_all(&mut *transaction).await?;
        let mut events = Vec::with_capacity(history.len());
        for row in history {
            events.push(issue_event_from_row(&row)?);
        }
        transaction.commit().await?;
        Ok(IssueDetails {
            issue,
            history: events,
        })
    }

    pub(crate) async fn record_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
        now: DateTime<Utc>,
    ) -> Result<IssueRecord, IssueReadError> {
        if issue_number <= 0 {
            return Err(IssueReadError::InvalidIssueNumber);
        }
        authorize_project_read(transaction, actor_user_id, project_id).await?;
        let local_today = organization_local_today(transaction, now).await?;
        let row = sqlx::query(
            "SELECT issues.id, issues.project_id, issues.issue_number, \
                    projects.project_key, issues.issue_type_id, issues.status_id, \
                    issues.title, issues.description, issues.assignee_user_id, \
                    issues.creator_user_id, issues.priority, issues.due_date, \
                    statuses.category AS status_category, \
                    issues.rank, issues.version, issues.completed_at, issues.trashed_at, \
                    issues.created_at, issues.updated_at \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             JOIN statuses ON statuses.project_id = issues.project_id \
                          AND statuses.id = issues.status_id \
             WHERE issues.project_id = ? AND issues.issue_number = ?",
        )
        .bind(project_id)
        .bind(issue_number)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(IssueReadError::Unavailable)?;
        let issue = issue_record_from_row(&row, local_today)?;

        let creation = sqlx::query(
            "SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
                    field_key, before_value_json, after_value_json \
             FROM issue_events \
              WHERE project_id = ? AND issue_id = ? AND event_kind = 'issue.created' \
              ORDER BY occurred_at ASC, id ASC LIMIT 1",
        )
        .bind(project_id)
        .bind(&issue.id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(IssueReadError::CorruptData)?;
        issue_event_from_row(&creation)?;
        Ok(issue)
    }

    pub(crate) async fn history_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
        after: Option<&IssueHistoryCursor>,
        page_size: u16,
        now: DateTime<Utc>,
    ) -> Result<IssueHistoryPage, IssueReadError> {
        validate_list_options(None, page_size)?;
        let issue =
            Self::record_in_transaction(transaction, actor_user_id, project_id, issue_number, now)
                .await?;
        let rows = sqlx::query("SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at, field_key, before_value_json, after_value_json FROM issue_events WHERE project_id = ? AND issue_id = ? AND (occurred_at, id) > (?, ?) ORDER BY occurred_at ASC, id ASC LIMIT ?")
            .bind(project_id).bind(&issue.id)
            .bind(after.map(|cursor| cursor.occurred_at.as_str()).unwrap_or(""))
            .bind(after.map(|cursor| cursor.event_id.as_str()).unwrap_or(""))
            .bind(i64::from(page_size) + 1).fetch_all(&mut **transaction).await?;
        let has_more = rows.len() > usize::from(page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(page_size)));
        let mut last = None;
        for row in rows.into_iter().take(usize::from(page_size)) {
            let event = issue_event_from_row(&row)?;
            let canonical = crate::auth::time::format_timestamp(event.occurred_at);
            if row.try_get::<String, _>("occurred_at")? != canonical
                || uuid::Uuid::parse_str(&event.id)
                    .map(|id| id.to_string())
                    .ok()
                    .as_deref()
                    != Some(event.id.as_str())
            {
                return Err(IssueReadError::CorruptData);
            }
            last = Some(IssueHistoryCursor {
                occurred_at: canonical,
                event_id: event.id.clone(),
            });
            items.push(event);
        }
        Ok(IssueHistoryPage {
            items,
            next_cursor: if has_more { last } else { None },
        })
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

pub(super) async fn authorize_project_read(
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

pub(super) fn issue_summary_from_row(
    row: &SqliteRow,
    local_today: NaiveDate,
) -> Result<IssueSummary, IssueReadError> {
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

    let due_date = parse_optional_date(row.try_get("due_date")?)?;
    let status_category: String = row.try_get("status_category")?;
    let is_overdue = super::deadlines::is_overdue(due_date, &status_category, local_today)
        .ok_or(IssueReadError::CorruptData)?;

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
        due_date,
        is_overdue,
        rank: row.try_get("rank")?,
        version: parse_version(row.try_get("version")?)?,
        created_at: parse_timestamp(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_timestamp(&row.try_get::<String, _>("updated_at")?)?,
    })
}

fn issue_record_from_row(
    row: &SqliteRow,
    local_today: NaiveDate,
) -> Result<IssueRecord, IssueReadError> {
    let summary = issue_summary_from_row(row, local_today)?;
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
        is_overdue: summary.is_overdue,
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
        "issue.title_updated" => IssueEventKind::TitleUpdated,
        "issue.description_updated" => IssueEventKind::DescriptionUpdated,
        "issue.text_updated" => IssueEventKind::TextUpdated,
        "issue.moved" => IssueEventKind::Moved,
        "issue.field_updated" => IssueEventKind::FieldUpdated,
        _ => return Err(IssueReadError::CorruptData),
    };

    let field_key: Option<String> = row.try_get("field_key")?;
    let before_value_json: Option<String> = row.try_get("before_value_json")?;
    let after_value_json: Option<String> = row.try_get("after_value_json")?;
    let change = match (kind, field_key, before_value_json, after_value_json) {
        (IssueEventKind::FieldUpdated, Some(field_key), Some(before_value), Some(after_value))
            if field_key == "priority" =>
        {
            let before = parse_priority(
                &serde_json::from_str::<String>(&before_value)
                    .map_err(|_| IssueReadError::CorruptData)?,
            )?;
            let after = parse_priority(
                &serde_json::from_str::<String>(&after_value)
                    .map_err(|_| IssueReadError::CorruptData)?,
            )?;
            if before == after {
                return Err(IssueReadError::CorruptData);
            }
            Some(IssueFieldChange::Priority { before, after })
        }
        (IssueEventKind::FieldUpdated, Some(field_key), Some(before_value), Some(after_value))
            if field_key == "due_date" =>
        {
            let before = parse_json_due_date(&before_value)?;
            let after = parse_json_due_date(&after_value)?;
            if before == after {
                return Err(IssueReadError::CorruptData);
            }
            Some(IssueFieldChange::DueDate { before, after })
        }
        (IssueEventKind::FieldUpdated, Some(field_key), Some(before_value), Some(after_value))
            if field_key == "assignee_user_id" =>
        {
            let before = parse_json_assignee(&before_value)?;
            let after = parse_json_assignee(&after_value)?;
            if before == after {
                return Err(IssueReadError::CorruptData);
            }
            Some(IssueFieldChange::Assignee { before, after })
        }
        (IssueEventKind::FieldUpdated, _, _, _) => return Err(IssueReadError::CorruptData),
        (_, None, None, None) => None,
        _ => return Err(IssueReadError::CorruptData),
    };

    Ok(IssueEvent {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        issue_id: row.try_get("issue_id")?,
        actor_user_id: row.try_get("actor_user_id")?,
        kind,
        change,
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
        .as_deref()
        .map(parse_date)
        .transpose()
        .map_err(|_| IssueReadError::CorruptData)
}

fn parse_date(value: &str) -> Result<NaiveDate, IssueReadError> {
    super::deadlines::parse_date(value).ok_or(IssueReadError::CorruptData)
}

fn parse_json_due_date(value: &str) -> Result<Option<NaiveDate>, IssueReadError> {
    let date: Option<String> =
        serde_json::from_str(value).map_err(|_| IssueReadError::CorruptData)?;
    date.as_deref().map(parse_date).transpose()
}

fn parse_json_assignee(value: &str) -> Result<Option<String>, IssueReadError> {
    let user_id: Option<String> =
        serde_json::from_str(value).map_err(|_| IssueReadError::CorruptData)?;
    if user_id.as_deref().is_some_and(|user_id| {
        user_id.len() != 36
            || uuid::Uuid::parse_str(user_id)
                .map(|parsed| parsed.to_string() != user_id)
                .unwrap_or(true)
    }) {
        return Err(IssueReadError::CorruptData);
    }
    Ok(user_id)
}

pub(super) async fn organization_local_today(
    transaction: &mut Transaction<'_, Sqlite>,
    now: DateTime<Utc>,
) -> Result<NaiveDate, IssueReadError> {
    let timezone: Option<String> =
        sqlx::query_scalar("SELECT timezone FROM organizations WHERE id = 1")
            .fetch_optional(&mut **transaction)
            .await?;
    let timezone = timezone.ok_or(IssueReadError::OrganizationTimezoneUnavailable)?;
    let timezone = OrganizationTimezone::parse(&timezone)
        .map_err(|_| IssueReadError::OrganizationTimezoneUnavailable)?;
    Ok(timezone.local_date(now))
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
