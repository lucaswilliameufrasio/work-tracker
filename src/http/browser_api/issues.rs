use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::Response,
    routing::{get, put},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    access::AccessStoreError,
    auth::time::format_timestamp,
    http::error::ApiError,
    issues::{
        IssuePriority, IssueReadError, IssueSummary,
        session::{IssueListOptions, SessionIssueReadError},
    },
};

use super::{
    BrowserState, Page, authenticated_token, malformed_query, no_store, session_error,
    storage_unavailable, unauthenticated,
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/issues",
            get(list).post(create::create),
        )
        .merge(detail::routes())
        .merge(board::routes())
        .route(
            "/api/v1/projects/{project_id}/issues/{issue_number}/text",
            put(text::replace),
        )
}

mod board;
mod create;
mod detail;
mod text;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    cursor: Option<String>,
    page_size: Option<u16>,
}

#[derive(Serialize)]
struct ListedIssue {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    issue_type_id: String,
    status_id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee_user_id: Option<String>,
    creator_user_id: String,
    priority: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date: Option<String>,
    version: String,
    created_at: String,
    updated_at: String,
}

impl TryFrom<IssueSummary> for ListedIssue {
    type Error = IssueReadError;

    fn try_from(issue: IssueSummary) -> Result<Self, Self::Error> {
        for id in [
            &issue.id,
            &issue.project_id,
            &issue.issue_type_id,
            &issue.status_id,
            &issue.creator_user_id,
        ] {
            if !canonical_uuid(id) {
                return Err(IssueReadError::CorruptData);
            }
        }
        if issue
            .assignee_user_id
            .as_deref()
            .is_some_and(|id| !canonical_uuid(id))
        {
            return Err(IssueReadError::CorruptData);
        }
        Ok(Self {
            id: issue.id,
            project_id: issue.project_id,
            issue_number: issue.issue_number.to_string(),
            issue_key: issue.issue_key,
            issue_type_id: issue.issue_type_id,
            status_id: issue.status_id,
            title: issue.title,
            assignee_user_id: issue.assignee_user_id,
            creator_user_id: issue.creator_user_id,
            priority: match issue.priority {
                IssuePriority::None => "none",
                IssuePriority::Low => "low",
                IssuePriority::Medium => "medium",
                IssuePriority::High => "high",
                IssuePriority::Urgent => "urgent",
            },
            due_date: issue
                .due_date
                .map(|date| date.format("%Y-%m-%d").to_string()),
            version: issue.version.to_string(),
            created_at: format_timestamp(issue.created_at),
            updated_at: format_timestamp(issue.updated_at),
        })
    }
}

async fn list(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path(project_id) =
        path.map_err(|_| ApiError::invalid_field("project_id", "Use a canonical UUID"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let cursor = query
        .cursor
        .as_deref()
        .map(|value| positive_i64(value, "cursor"))
        .transpose()?;
    let page = state
        .issues
        .list_project_issues(
            &token,
            &project_id,
            IssueListOptions {
                after_issue_number: cursor,
                page_size: query.page_size.unwrap_or(50),
            },
            Utc::now(),
        )
        .await
        .map_err(read_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for issue in page.items {
        data.push(ListedIssue::try_from(issue).map_err(|error| read_error(error.into()))?);
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_issue_number.is_some(),
        next_cursor: page
            .next_after_issue_number
            .map(|number| number.to_string()),
    }))
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::parse_str(value)
            .map(|id| id.to_string() == value)
            .unwrap_or(false)
}

fn positive_i64(value: &str, field: &'static str) -> Result<i64, ApiError> {
    if value.is_empty()
        || value.len() > 19
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ApiError::invalid_field(
            field,
            "Use a positive canonical decimal i64",
        ));
    }
    value
        .parse()
        .map_err(|_| ApiError::invalid_field(field, "Use a positive canonical decimal i64"))
}

fn read_error(error: SessionIssueReadError) -> ApiError {
    match error {
        SessionIssueReadError::Unauthenticated => unauthenticated(),
        SessionIssueReadError::Session(error) => session_error(error),
        SessionIssueReadError::Database(_)
        | SessionIssueReadError::Issue(IssueReadError::Database(_))
        | SessionIssueReadError::Issue(IssueReadError::AccessState(AccessStoreError::Database(
            _,
        ))) => storage_unavailable(),
        SessionIssueReadError::Issue(IssueReadError::Unavailable) => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        SessionIssueReadError::Issue(IssueReadError::InvalidPageSize) => {
            ApiError::invalid_field("page_size", "Use a value between 1 and 100")
        }
        SessionIssueReadError::Issue(IssueReadError::InvalidCursor) => {
            ApiError::invalid_field("cursor", "Use a positive canonical decimal i64")
        }
        SessionIssueReadError::Issue(IssueReadError::InvalidIssueNumber) => {
            ApiError::invalid_field("issue_number", "Use a positive canonical decimal i64")
        }
        SessionIssueReadError::Issue(IssueReadError::InvalidHistoryCursor) => {
            ApiError::invalid_field("cursor", "Use a valid issue-history cursor")
        }
        SessionIssueReadError::Issue(IssueReadError::InvalidBoardCursor) => {
            ApiError::invalid_field("cursor", "Use a valid board cursor")
        }
        _ => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}
