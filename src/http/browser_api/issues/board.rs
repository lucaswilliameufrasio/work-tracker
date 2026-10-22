use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    response::Response,
    routing::get,
};
use chrono::Utc;
use serde::Serialize;

use super::{
    BrowserState, ListQuery, ListedIssue, authenticated_token, canonical_uuid, malformed_query,
    no_store, read_error,
};
use crate::{
    http::error::ApiError,
    issues::{BoardCursor, BoardOptions},
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new().route("/api/v1/projects/{project_id}/board/{status_id}", get(read))
}

#[derive(Serialize)]
struct Card {
    #[serde(flatten)]
    summary: ListedIssue,
    rank: String,
}

#[derive(Serialize)]
struct ColumnPage {
    data: Vec<Card>,
    total_count: String,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

async fn read(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project_id, status_id)) =
        path.map_err(|_| ApiError::invalid_field("path", "Use canonical UUID selectors"))?;
    for (field, id) in [("project_id", &project_id), ("status_id", &status_id)] {
        if !canonical_uuid(id) {
            return Err(ApiError::invalid_field(field, "Use a canonical UUID"));
        }
    }
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let cursor = query
        .cursor
        .as_deref()
        .map(BoardCursor::decode)
        .transpose()
        .map_err(|error| read_error(error.into()))?;
    let page = state
        .issues
        .read_board_column(
            &token,
            &project_id,
            &status_id,
            BoardOptions {
                after: cursor.as_ref(),
                page_size: query.page_size.unwrap_or(20),
            },
            Utc::now(),
        )
        .await
        .map_err(read_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for issue in page.items {
        let rank = issue.rank.to_string();
        data.push(Card {
            summary: ListedIssue::try_from(issue).map_err(|error| read_error(error.into()))?,
            rank,
        });
    }
    Ok(no_store(ColumnPage {
        data,
        total_count: page.total_count.to_string(),
        has_more: page.next_cursor.is_some(),
        next_cursor: page.next_cursor.map(|cursor| cursor.encode()),
    }))
}
