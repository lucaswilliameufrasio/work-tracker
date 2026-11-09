use std::sync::Arc;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::{
    BrowserState, authenticated_token, canonical_uuid, create::idempotency_key, malformed_query,
    positive_i64, storage_unavailable, unauthenticated,
};
use crate::{
    http::error::ApiError,
    issues::comments::{CommentError, CommentPage as ListedComments, CommentReceipt},
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/issues/{issue_number}/comments",
            get(list).post(create),
        )
        .route(
            "/api/v1/projects/{project_id}/issues/{issue_number}/comments/{comment_id}",
            put(edit).delete(remove),
        )
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BodyInput {
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    body: String,
    expected_version: String,
}

#[derive(Serialize)]
struct CommentBody {
    id: String,
    author_user_id: String,
    author_display_name: String,
    body: String,
    version: String,
    can_edit: bool,
    can_delete: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize)]
struct EventBody {
    id: String,
    comment_id: String,
    actor_user_id: String,
    event_kind: String,
    occurred_at: String,
}

#[derive(Serialize)]
struct PageBody {
    comments: Vec<CommentBody>,
    events: Vec<EventBody>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct ReceiptBody {
    id: String,
    project_id: String,
    issue_number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_version: Option<String>,
    changed: bool,
}

async fn list(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    Query(query): Query<ListQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let (project_id, issue_number) = selectors(path)?;
    if query
        .cursor
        .as_deref()
        .is_some_and(|cursor| !canonical_uuid(cursor))
    {
        return Err(ApiError::invalid_field(
            "cursor",
            "Use a canonical UUID comment id",
        ));
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let principal = state
        .sessions
        .authenticate_in_transaction(&mut transaction, &token, Utc::now())
        .await
        .map_err(super::session_error)?
        .ok_or_else(unauthenticated)?;
    let actor = principal.user_id().to_owned();
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let page = state
        .comments
        .list(&actor, &project_id, issue_number, query.cursor.as_deref())
        .await
        .map_err(comment_error)?;
    Ok(page_response(page))
}

async fn create(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let (project_id, issue_number) = selectors(path)?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let key = idempotency_key(&request.headers)?;
    let input: BodyInput = read_json(body).await?;
    let actor = actor(&state, &token).await?;
    let receipt = state
        .comments
        .create(
            &actor,
            &project_id,
            issue_number,
            key,
            &input.body,
            Utc::now(),
        )
        .await
        .map_err(comment_error)?;
    created(receipt)
}

async fn edit(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project_id, number, comment_id)) =
        path.map_err(|_| ApiError::invalid_field("comment_id", "Use canonical path selectors"))?;
    let (project_id, issue_number) = selectors(Ok(Path((project_id, number))))?;
    if !canonical_uuid(&comment_id) {
        return Err(ApiError::invalid_field(
            "comment_id",
            "Use a canonical UUID",
        ));
    }
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let key = idempotency_key(&request.headers)?;
    let input: EditInput = read_json(body).await?;
    let expected_version = positive_i64(&input.expected_version, "expected_version")?;
    let actor = actor(&state, &token).await?;
    let receipt = state
        .comments
        .edit(
            crate::issues::comments::CommentEdit {
                actor_user_id: &actor,
                project_id: &project_id,
                issue_number,
                comment_id: &comment_id,
                idempotency_key: key,
                expected_version,
                body: &input.body,
            },
            Utc::now(),
        )
        .await
        .map_err(comment_error)?;
    Ok(receipt_response(receipt, StatusCode::OK, None))
}

async fn remove(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project_id, number, comment_id)) =
        path.map_err(|_| ApiError::invalid_field("comment_id", "Use canonical path selectors"))?;
    let (project_id, issue_number) = selectors(Ok(Path((project_id, number))))?;
    if !canonical_uuid(&comment_id) {
        return Err(ApiError::invalid_field(
            "comment_id",
            "Use a canonical UUID",
        ));
    }
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let key = idempotency_key(&request.headers)?;
    let bytes = to_bytes(body, 1).await.map_err(|_| malformed())?;
    if !bytes.is_empty() {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "Request body is not supported",
            false,
        ));
    }
    let actor = actor(&state, &token).await?;
    let receipt = state
        .comments
        .remove(
            &actor,
            &project_id,
            issue_number,
            &comment_id,
            key,
            Utc::now(),
        )
        .await
        .map_err(comment_error)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::HeaderName::from_static("idempotency-replayed"),
        HeaderValue::from_static(if receipt.replayed { "true" } else { "false" }),
    );
    Ok(response)
}

fn selectors(
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<(String, i64), ApiError> {
    let Path((project_id, number)) =
        path.map_err(|_| ApiError::invalid_field("issue_number", "Use canonical path selectors"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    Ok((project_id, positive_i64(&number, "issue_number")?))
}

async fn actor(
    state: &BrowserState,
    token: &crate::auth::SessionToken,
) -> Result<String, ApiError> {
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let principal = state
        .sessions
        .authenticate_in_transaction(&mut transaction, token, Utc::now())
        .await
        .map_err(super::session_error)?
        .ok_or_else(unauthenticated)?;
    let actor = principal.user_id().to_owned();
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    Ok(actor)
}

fn page_response(page: ListedComments) -> Response {
    let mut comments = Vec::with_capacity(page.comments.len());
    for comment in page.comments {
        comments.push(CommentBody {
            id: comment.id,
            author_user_id: comment.author_user_id,
            author_display_name: comment.author_display_name,
            body: comment.body,
            version: comment.version.to_string(),
            can_edit: comment.can_edit,
            can_delete: comment.can_delete,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        });
    }
    let mut events = Vec::with_capacity(page.events.len());
    for event in page.events {
        events.push(EventBody {
            id: event.id,
            comment_id: event.comment_id,
            actor_user_id: event.actor_user_id,
            event_kind: event.event_kind,
            occurred_at: event.occurred_at,
        });
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(PageBody {
            comments,
            events,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        }),
    )
        .into_response()
}

fn created(receipt: CommentReceipt) -> Result<Response, ApiError> {
    let location = format!(
        "/api/v1/projects/{}/issues/{}/comments/{}",
        receipt.project_id, receipt.issue_number, receipt.comment_id
    );
    Ok(receipt_response(
        receipt,
        StatusCode::CREATED,
        Some(location),
    ))
}

fn receipt_response(
    receipt: CommentReceipt,
    status: StatusCode,
    location: Option<String>,
) -> Response {
    let mut response = (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(ReceiptBody {
            id: receipt.comment_id,
            project_id: receipt.project_id,
            issue_number: receipt.issue_number.to_string(),
            result_version: receipt.result_version.map(|version| version.to_string()),
            changed: receipt.changed,
        }),
    )
        .into_response();
    if let Some(location) = location.and_then(|value| HeaderValue::from_str(&value).ok()) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response.headers_mut().insert(
        header::HeaderName::from_static("idempotency-replayed"),
        HeaderValue::from_static(if receipt.replayed { "true" } else { "false" }),
    );
    response
}

async fn read_json<T: for<'de> Deserialize<'de>>(body: axum::body::Body) -> Result<T, ApiError> {
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| malformed())?;
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

fn malformed() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "malformed_request",
        "Request JSON is malformed or unsupported",
        false,
    )
}

fn comment_error(error: CommentError) -> ApiError {
    match error {
        CommentError::Unavailable => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        CommentError::Forbidden => ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "This comment cannot be changed",
            false,
        ),
        CommentError::InvalidBody => ApiError::invalid_field("body", "Use a comment up to 64 KiB"),
        CommentError::VersionConflict => ApiError::new(
            StatusCode::CONFLICT,
            "issue_version_conflict",
            "Reload the comment and preserve your draft before editing again",
            false,
        ),
        CommentError::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency key was used for a different request",
            false,
        ),
        CommentError::InvalidIdempotencyKey => ApiError::invalid_field(
            "idempotency_key",
            "Supply one canonical UUID Idempotency-Key",
        ),
        CommentError::Access(crate::access::AccessStoreError::Database(_))
        | CommentError::Database(_) => storage_unavailable(),
        CommentError::Access(crate::access::AccessStoreError::CorruptState) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}
