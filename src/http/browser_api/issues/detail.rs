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
    BrowserState, ListQuery, ListedIssue, Page, authenticated_token, canonical_uuid,
    malformed_query, no_store, positive_i64, read_error,
};
use crate::{
    auth::time::format_timestamp,
    http::error::ApiError,
    issues::{IssueEventKind, IssueHistoryCursor, IssueReadError, IssueRecord, IssueSummary},
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/issues/{issue_number}",
            get(detail),
        )
        .route(
            "/api/v1/projects/{project_id}/issues/{issue_number}/history",
            get(history),
        )
}

#[derive(Serialize)]
struct IssueDetail {
    #[serde(flatten)]
    summary: ListedIssue,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<String>,
}

impl TryFrom<IssueRecord> for IssueDetail {
    type Error = IssueReadError;

    fn try_from(issue: IssueRecord) -> Result<Self, Self::Error> {
        let summary = IssueSummary {
            id: issue.id,
            project_id: issue.project_id,
            issue_number: issue.issue_number,
            issue_key: issue.issue_key,
            issue_type_id: issue.issue_type_id,
            status_id: issue.status_id,
            title: issue.title,
            assignee_user_id: issue.assignee_user_id,
            creator_user_id: issue.creator_user_id,
            priority: issue.priority,
            due_date: issue.due_date,
            rank: issue.rank,
            version: issue.version,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
        };
        Ok(Self {
            summary: ListedIssue::try_from(summary)?,
            description: issue.description,
            completed_at: issue.completed_at.map(format_timestamp),
            trashed_at: issue.trashed_at.map(format_timestamp),
        })
    }
}

#[derive(Serialize)]
struct HistoryEvent {
    id: String,
    project_id: String,
    issue_id: String,
    actor_user_id: String,
    event_kind: &'static str,
    occurred_at: String,
}

fn issue_path(
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<(String, i64), ApiError> {
    let Path((project_id, number)) =
        path.map_err(|_| ApiError::invalid_field("path", "Use valid project and issue selectors"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    let number = positive_i64(&number, "issue_number")?;
    Ok((project_id, number))
}

async fn detail(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let (project_id, number) = issue_path(path)?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let issue = state
        .issues
        .get_project_issue(&token, &project_id, number, Utc::now())
        .await
        .map_err(read_error)?;
    Ok(no_store(
        IssueDetail::try_from(issue).map_err(|error| read_error(error.into()))?,
    ))
}

async fn history(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let (project_id, number) = issue_path(path)?;
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let cursor = query
        .cursor
        .as_deref()
        .map(IssueHistoryCursor::decode)
        .transpose()
        .map_err(|error| read_error(error.into()))?;
    let page = state
        .issues
        .list_history(
            &token,
            &project_id,
            number,
            cursor.as_ref(),
            query.page_size.unwrap_or(50),
            Utc::now(),
        )
        .await
        .map_err(read_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for event in page.items {
        for id in [
            &event.id,
            &event.project_id,
            &event.issue_id,
            &event.actor_user_id,
        ] {
            if !canonical_uuid(id) {
                return Err(read_error(IssueReadError::CorruptData.into()));
            }
        }
        data.push(HistoryEvent {
            id: event.id,
            project_id: event.project_id,
            issue_id: event.issue_id,
            actor_user_id: event.actor_user_id,
            event_kind: match event.kind {
                IssueEventKind::Created => "issue.created",
                IssueEventKind::TitleUpdated => "issue.title_updated",
                IssueEventKind::DescriptionUpdated => "issue.description_updated",
                IssueEventKind::TextUpdated => "issue.text_updated",
                IssueEventKind::Moved => "issue.moved",
            },
            occurred_at: format_timestamp(event.occurred_at),
        });
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_cursor.is_some(),
        next_cursor: page.next_cursor.map(|cursor| cursor.encode()),
    }))
}
