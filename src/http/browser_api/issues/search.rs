use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    response::Response,
    routing::get,
};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;

use super::{
    BrowserState, ListedIssue, Page, authenticated_token, canonical_uuid, malformed_query,
    no_store, read_error,
};
use crate::{
    http::error::ApiError,
    issues::{IssuePriority, IssueSearchCursor, IssueSearchOptions},
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new().route("/api/v1/projects/{project_id}/issues/search", get(search))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: Option<String>,
    priority: Option<String>,
    due_date: Option<String>,
    cursor: Option<String>,
    page_size: Option<u16>,
}

async fn search(
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
    let query = Query::<SearchQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let priority = query.priority.as_deref().map(parse_priority).transpose()?;
    let due_date = query.due_date.as_deref().map(parse_due_date).transpose()?;
    if query.q.is_none() && priority.is_none() && due_date.is_none() {
        return Err(ApiError::invalid_field(
            "q",
            "Provide a search query or at least one structured filter",
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(IssueSearchCursor::decode)
        .transpose()
        .map_err(|error| read_error(error.into()))?;
    let page = state
        .issues
        .search_project_issues(
            &token,
            &project_id,
            IssueSearchOptions {
                query: query.q.as_deref(),
                priority,
                due_date,
                after: cursor.as_ref(),
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
        has_more: page.next_cursor.is_some(),
        next_cursor: page.next_cursor.map(|cursor| cursor.encode()),
    }))
}

fn parse_priority(value: &str) -> Result<IssuePriority, ApiError> {
    match value {
        "none" => Ok(IssuePriority::None),
        "low" => Ok(IssuePriority::Low),
        "medium" => Ok(IssuePriority::Medium),
        "high" => Ok(IssuePriority::High),
        "urgent" => Ok(IssuePriority::Urgent),
        _ => Err(ApiError::invalid_field(
            "priority",
            "Use none, low, medium, high, or urgent",
        )),
    }
}

fn parse_due_date(value: &str) -> Result<NaiveDate, ApiError> {
    if value.len() != 10 {
        return Err(ApiError::invalid_field(
            "due_date",
            "Use a valid calendar date in YYYY-MM-DD format",
        ));
    }
    crate::issues::deadlines::parse_date(value).ok_or_else(|| {
        ApiError::invalid_field("due_date", "Use a valid calendar date in YYYY-MM-DD format")
    })
}
