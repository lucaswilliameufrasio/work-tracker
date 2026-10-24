use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use sqlx::{Sqlite, Transaction};

use super::{
    IssueReadError, IssueStore, IssueSummary,
    queries::{authorize_project_read, issue_summary_from_row, organization_local_today},
};

const MAX_PAGE_SIZE: u16 = 100;
const MAX_QUERY_BYTES: usize = 512;
const MAX_QUERY_SCALARS: usize = 256;
const MAX_QUERY_TERMS: usize = 16;

/// Opaque keyset boundary for the stable `(issue_number, issue_id)` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSearchCursor {
    issue_number: i64,
    issue_id: String,
}

impl IssueSearchCursor {
    pub fn decode(value: &str) -> Result<Self, IssueReadError> {
        if !(51..=75).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(IssueReadError::InvalidSearchCursor);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| IssueReadError::InvalidSearchCursor)?;
        if URL_SAFE_NO_PAD.encode(&bytes) != value {
            return Err(IssueReadError::InvalidSearchCursor);
        }
        let raw = std::str::from_utf8(&bytes).map_err(|_| IssueReadError::InvalidSearchCursor)?;
        let (number, id) = raw
            .split_once('/')
            .ok_or(IssueReadError::InvalidSearchCursor)?;
        let issue_number =
            canonical_positive_i64(number).ok_or(IssueReadError::InvalidSearchCursor)?;
        let issue_id = uuid::Uuid::parse_str(id)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| value == id)
            .ok_or(IssueReadError::InvalidSearchCursor)?;
        Ok(Self {
            issue_number,
            issue_id,
        })
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}/{}", self.issue_number, self.issue_id))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSearchPage {
    pub items: Vec<IssueSummary>,
    pub next_cursor: Option<IssueSearchCursor>,
}

impl IssueStore {
    pub async fn search_project_issues(
        &self,
        actor_user_id: &str,
        project_id: &str,
        query: &str,
        after: Option<&IssueSearchCursor>,
        page_size: u16,
    ) -> Result<IssueSearchPage, IssueReadError> {
        let mut transaction = self.pool.begin().await?;
        let page = Self::search_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            query,
            after,
            page_size,
            Utc::now(),
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }

    pub(crate) async fn search_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        actor_user_id: &str,
        project_id: &str,
        query: &str,
        after: Option<&IssueSearchCursor>,
        page_size: u16,
        now: DateTime<Utc>,
    ) -> Result<IssueSearchPage, IssueReadError> {
        if !(1..=MAX_PAGE_SIZE).contains(&page_size) {
            return Err(IssueReadError::InvalidPageSize);
        }
        let terms = search_terms(query)?;
        authorize_project_read(transaction, actor_user_id, project_id).await?;
        let local_today = organization_local_today(transaction, now).await?;

        let rows = if is_issue_key_shaped(query) {
            let Some((project_key, issue_number)) = exact_issue_key(query) else {
                return Ok(IssueSearchPage {
                    items: Vec::new(),
                    next_cursor: None,
                });
            };
            sqlx::query(
                "SELECT issues.id, issues.project_id, issues.issue_number, \
                        projects.project_key, issues.issue_type_id, issues.status_id, \
                        issues.title, issues.assignee_user_id, issues.creator_user_id, \
                        issues.priority, issues.due_date, statuses.category AS status_category, \
                        issues.rank, issues.version, issues.created_at, issues.updated_at \
                 FROM issues \
                 JOIN projects ON projects.id = issues.project_id \
                 JOIN statuses ON statuses.project_id = issues.project_id \
                              AND statuses.id = issues.status_id \
                 WHERE issues.project_id = ? AND projects.project_key = ? \
                   AND issues.issue_number = ? AND issues.trashed_at IS NULL \
                   AND (issues.issue_number, issues.id) > (?, ?) \
                 ORDER BY issues.issue_number ASC, issues.id ASC LIMIT ?",
            )
            .bind(project_id)
            .bind(project_key)
            .bind(issue_number)
            .bind(after.map(|cursor| cursor.issue_number).unwrap_or(0))
            .bind(after.map(|cursor| cursor.issue_id.as_str()).unwrap_or(""))
            .bind(i64::from(page_size) + 1)
            .fetch_all(&mut **transaction)
            .await?
        } else {
            let expression = fts_expression(&terms);
            sqlx::query(
                "SELECT issues.id, issues.project_id, issues.issue_number, \
                        projects.project_key, issues.issue_type_id, issues.status_id, \
                        issues.title, issues.assignee_user_id, issues.creator_user_id, \
                        issues.priority, issues.due_date, statuses.category AS status_category, \
                        issues.rank, issues.version, issues.created_at, issues.updated_at \
                 FROM issues_fts \
                 JOIN issue_search_documents ON issue_search_documents.id = issues_fts.rowid \
                 JOIN issues ON issues.id = issue_search_documents.issue_id \
                 JOIN projects ON projects.id = issues.project_id \
                 JOIN statuses ON statuses.project_id = issues.project_id \
                              AND statuses.id = issues.status_id \
                 WHERE issues_fts MATCH ? AND issues.project_id = ? \
                   AND issues.trashed_at IS NULL \
                   AND (issues.issue_number, issues.id) > (?, ?) \
                 ORDER BY issues.issue_number ASC, issues.id ASC LIMIT ?",
            )
            .bind(expression)
            .bind(project_id)
            .bind(after.map(|cursor| cursor.issue_number).unwrap_or(0))
            .bind(after.map(|cursor| cursor.issue_id.as_str()).unwrap_or(""))
            .bind(i64::from(page_size) + 1)
            .fetch_all(&mut **transaction)
            .await?
        };

        let has_more = rows.len() > usize::from(page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(page_size)));
        for row in rows.into_iter().take(usize::from(page_size)) {
            items.push(issue_summary_from_row(&row, local_today)?);
        }
        let next_cursor = if has_more {
            items.last().map(|issue| IssueSearchCursor {
                issue_number: issue.issue_number,
                issue_id: issue.id.clone(),
            })
        } else {
            None
        };
        Ok(IssueSearchPage { items, next_cursor })
    }
}

fn search_terms(query: &str) -> Result<Vec<&str>, IssueReadError> {
    if query.len() > MAX_QUERY_BYTES
        || query.chars().count() > MAX_QUERY_SCALARS
        || query.chars().any(char::is_control)
    {
        return Err(IssueReadError::InvalidSearchQuery);
    }
    let terms = query.split_whitespace().collect::<Vec<_>>();
    if terms.is_empty() || terms.len() > MAX_QUERY_TERMS {
        return Err(IssueReadError::InvalidSearchQuery);
    }
    Ok(terms)
}

fn fts_expression(terms: &[&str]) -> String {
    let mut expression = String::new();
    for (index, term) in terms.iter().enumerate() {
        if index > 0 {
            expression.push_str(" AND ");
        }
        expression.push('"');
        for character in term.chars() {
            if character == '"' {
                expression.push('"');
            }
            expression.push(character);
        }
        expression.push('"');
    }
    expression
}

fn exact_issue_key(query: &str) -> Option<(String, i64)> {
    let (project_key, issue_number) = query.split_once('-')?;
    if !(2..=6).contains(&project_key.len())
        || !project_key.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return None;
    }
    Some((
        project_key.to_ascii_uppercase(),
        canonical_positive_i64(issue_number)?,
    ))
}

fn is_issue_key_shaped(query: &str) -> bool {
    let Some((project_key, issue_number)) = query.split_once('-') else {
        return false;
    };
    (2..=6).contains(&project_key.len())
        && project_key.bytes().all(|byte| byte.is_ascii_alphabetic())
        && !issue_number.is_empty()
        && issue_number.bytes().all(|byte| byte.is_ascii_digit())
}

fn canonical_positive_i64(value: &str) -> Option<i64> {
    if value.is_empty()
        || value.len() > 19
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse::<i64>().ok().filter(|number| *number > 0)
}
