use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, Sqlite, Transaction};

use super::{
    IssueReadError, IssueStore, IssueSummary,
    queries::{issue_summary_from_row, organization_local_today},
};

const MAX_PAGE_SIZE: u16 = 100;

/// Keyset for due date ascending, missing dates last, then newest update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedIssueCursor {
    due_is_null: bool,
    due_date: Option<NaiveDate>,
    updated_at: String,
    id: String,
}

impl AssignedIssueCursor {
    pub fn decode(value: &str) -> Result<Self, IssueReadError> {
        if value.len() < 40
            || value.len() > 120
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(IssueReadError::InvalidAssignedCursor);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| IssueReadError::InvalidAssignedCursor)?;
        if URL_SAFE_NO_PAD.encode(&bytes) != value {
            return Err(IssueReadError::InvalidAssignedCursor);
        }
        let raw = std::str::from_utf8(&bytes).map_err(|_| IssueReadError::InvalidAssignedCursor)?;
        let mut parts = raw.split('/');
        let flag = parts.next().ok_or(IssueReadError::InvalidAssignedCursor)?;
        let due = parts.next().ok_or(IssueReadError::InvalidAssignedCursor)?;
        let updated_at = parts.next().ok_or(IssueReadError::InvalidAssignedCursor)?;
        let id = parts.next().ok_or(IssueReadError::InvalidAssignedCursor)?;
        if parts.next().is_some() {
            return Err(IssueReadError::InvalidAssignedCursor);
        }
        let due_is_null = match flag {
            "0" => false,
            "1" => true,
            _ => return Err(IssueReadError::InvalidAssignedCursor),
        };
        let due_date = if due_is_null {
            if due != "-" {
                return Err(IssueReadError::InvalidAssignedCursor);
            }
            None
        } else {
            Some(
                NaiveDate::parse_from_str(due, "%Y-%m-%d")
                    .ok()
                    .filter(|date| date.format("%Y-%m-%d").to_string() == due)
                    .ok_or(IssueReadError::InvalidAssignedCursor)?,
            )
        };
        if updated_at.len() != 30 || !updated_at.is_ascii() {
            return Err(IssueReadError::InvalidAssignedCursor);
        }
        let parsed_id = uuid::Uuid::parse_str(id)
            .ok()
            .filter(|value| value.to_string() == id)
            .ok_or(IssueReadError::InvalidAssignedCursor)?;
        Ok(Self {
            due_is_null,
            due_date,
            updated_at: updated_at.to_owned(),
            id: parsed_id.to_string(),
        })
    }

    pub fn encode(&self) -> String {
        let due = self
            .due_date
            .map(|date| date.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "-".to_owned());
        let flag = if self.due_is_null { "1" } else { "0" };
        URL_SAFE_NO_PAD.encode(format!(
            "{flag}/{due}/{}/{id}",
            self.updated_at,
            id = self.id
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedIssuePage {
    pub items: Vec<IssueSummary>,
    pub next_cursor: Option<AssignedIssueCursor>,
}

impl IssueStore {
    pub(crate) async fn assigned_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        cursor: Option<&str>,
        page_size: u16,
        now: DateTime<Utc>,
    ) -> Result<AssignedIssuePage, IssueReadError> {
        if !(1..=MAX_PAGE_SIZE).contains(&page_size) {
            return Err(IssueReadError::InvalidPageSize);
        }
        if uuid::Uuid::parse_str(actor_user_id)
            .ok()
            .filter(|value| value.to_string() == actor_user_id)
            .is_none()
        {
            return Err(IssueReadError::Unavailable);
        }
        let cursor = cursor.map(AssignedIssueCursor::decode).transpose()?;
        let local_today = organization_local_today(transaction, now).await?;
        let null_flag = i64::from(cursor.as_ref().is_some_and(|value| value.due_is_null));
        let has_cursor = i64::from(cursor.is_some());
        let due = cursor
            .as_ref()
            .and_then(|value| value.due_date)
            .map(|date| date.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let updated_at = cursor
            .as_ref()
            .map(|value| value.updated_at.clone())
            .unwrap_or_default();
        let cursor_id = cursor
            .as_ref()
            .map(|value| value.id.clone())
            .unwrap_or_default();
        let rows = sqlx::query(
            "SELECT issues.id, issues.project_id, issues.issue_number, \
                    projects.project_key, issues.issue_type_id, issues.status_id, \
                    issues.title, issues.assignee_user_id, issues.creator_user_id, \
                    issues.priority, issues.due_date, statuses.category AS status_category, \
                    issues.rank, issues.version, issues.created_at, issues.updated_at \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             JOIN project_members \
               ON project_members.project_id = issues.project_id \
              AND project_members.organization_id = projects.organization_id \
              AND project_members.user_id = ? \
             JOIN statuses ON statuses.project_id = issues.project_id \
                          AND statuses.id = issues.status_id \
             WHERE issues.assignee_user_id = ? \
               AND issues.trashed_at IS NULL \
               AND projects.archived_at IS NULL \
               AND (? = 0 OR ( \
                    (issues.due_date IS NULL) > ? \
                    OR ( \
                      (issues.due_date IS NULL) = ? \
                      AND ( \
                        (? = 0 AND issues.due_date > ?) \
                        OR ( \
                          ((? = 0 AND issues.due_date = ?) OR ? = 1) \
                          AND ( \
                            issues.updated_at < ? \
                            OR (issues.updated_at = ? AND issues.id < ?) \
                          ) \
                        ) \
                      ) \
                    ) \
               )) \
             ORDER BY (issues.due_date IS NULL), issues.due_date, \
                      issues.updated_at DESC, issues.id DESC \
             LIMIT ?",
        )
        .bind(actor_user_id)
        .bind(actor_user_id)
        .bind(has_cursor)
        .bind(null_flag)
        .bind(null_flag)
        .bind(null_flag)
        .bind(&due)
        .bind(null_flag)
        .bind(&due)
        .bind(null_flag)
        .bind(&updated_at)
        .bind(&updated_at)
        .bind(&cursor_id)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&mut **transaction)
        .await?;
        let has_more = rows.len() > usize::from(page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(page_size)));
        for row in rows.iter().take(usize::from(page_size)) {
            let summary = issue_summary_from_row(row, local_today)?;
            if summary.assignee_user_id.as_deref() != Some(actor_user_id) {
                return Err(IssueReadError::CorruptData);
            }
            items.push(summary);
        }
        let next_cursor = if has_more {
            let last = items.last().ok_or(IssueReadError::CorruptData)?;
            let boundary = rows
                .get(items.len() - 1)
                .ok_or(IssueReadError::CorruptData)?;
            let updated_at: String = boundary.try_get("updated_at")?;
            if updated_at.len() != 30 {
                return Err(IssueReadError::CorruptData);
            }
            Some(AssignedIssueCursor {
                due_is_null: last.due_date.is_none(),
                due_date: last.due_date,
                updated_at,
                id: last.id.clone(),
            })
        } else {
            None
        };
        Ok(AssignedIssuePage { items, next_cursor })
    }
}
