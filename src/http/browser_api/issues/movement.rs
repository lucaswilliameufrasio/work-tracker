use super::create::{idempotency_key, present_string};
use super::{
    BrowserState, authenticated_token, canonical_uuid, malformed_query, positive_i64,
    session_error, storage_unavailable, unauthenticated,
};
use crate::{
    access::AccessStoreError,
    http::error::ApiError,
    issues::movement::{IssueMoveError, MoveIssue},
};
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_version: String,
    target_status_id: String,
    #[serde(default, deserialize_with = "present_string")]
    previous_issue_id: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    next_issue_id: Option<String>,
}

#[derive(Serialize)]
struct Receipt {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    status_id: String,
    result_version: String,
    changed: bool,
}

pub(super) async fn move_issue(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project_id, issue_number)) =
        path.map_err(|_| ApiError::invalid_field("path", "Use canonical path selectors"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    let issue_number = positive_i64(&issue_number, "issue_number")?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let key = idempotency_key(&request.headers)?;
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| malformed())?;
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    let expected_version = positive_i64(&input.expected_version, "expected_version")?;
    if !canonical_uuid(&input.target_status_id) {
        return Err(ApiError::invalid_field(
            "target_status_id",
            "Use a canonical UUID",
        ));
    }
    for (field, value) in [
        ("previous_issue_id", input.previous_issue_id.as_deref()),
        ("next_issue_id", input.next_issue_id.as_deref()),
    ] {
        if value.is_some_and(|id| !canonical_uuid(id)) {
            return Err(ApiError::invalid_field(field, "Use a canonical UUID"));
        }
    }

    let outcome = state
        .mover
        .move_issue(
            &token,
            &project_id,
            issue_number,
            key,
            MoveIssue {
                expected_version,
                target_status_id: &input.target_status_id,
                previous_issue_id: input.previous_issue_id.as_deref(),
                next_issue_id: input.next_issue_id.as_deref(),
            },
            Utc::now(),
        )
        .await
        .map_err(move_error)?;
    let receipt = outcome.receipt;
    let location = HeaderValue::from_str(&format!(
        "/api/v1/projects/{}/issues/{}",
        receipt.project_id, receipt.issue_number
    ))
    .map_err(|_| unexpected())?;
    let mut response = (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(Receipt {
            id: receipt.issue_id,
            project_id: receipt.project_id,
            issue_number: receipt.issue_number.to_string(),
            issue_key: receipt.issue_key,
            status_id: receipt.status_id,
            result_version: receipt.result_version.to_string(),
            changed: receipt.changed,
        }),
    )
        .into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(
        "idempotency-replayed",
        HeaderValue::from_static(if outcome.replayed { "true" } else { "false" }),
    );
    Ok(response)
}

fn malformed() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "malformed_request",
        "Request JSON is malformed or unsupported",
        false,
    )
}

fn unexpected() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected_error",
        "Unexpected error",
        false,
    )
}

fn move_error(error: IssueMoveError) -> ApiError {
    match error {
        IssueMoveError::Unauthenticated => unauthenticated(),
        IssueMoveError::Unavailable => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        IssueMoveError::VersionConflict => ApiError::new(
            StatusCode::CONFLICT,
            "issue_version_conflict",
            "Reload the issue before moving it again",
            false,
        ),
        IssueMoveError::OrderConflict => ApiError::new(
            StatusCode::CONFLICT,
            "issue_order_conflict",
            "Reload the board and choose current neighboring issues",
            false,
        ),
        IssueMoveError::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency key was used for a different request",
            false,
        ),
        IssueMoveError::FingerprintKeyUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "idempotency_unavailable",
            "Original request key unavailable; do not recreate with a new key",
            false,
        ),
        IssueMoveError::VersionExhausted | IssueMoveError::RankSpaceExhausted => ApiError::new(
            StatusCode::CONFLICT,
            "issue_capacity_exhausted",
            "Issue ordering capacity is exhausted",
            false,
        ),
        IssueMoveError::TimeRegression => ApiError::new(
            StatusCode::CONFLICT,
            "issue_clock_conflict",
            "Issue update time is unavailable",
            false,
        ),
        IssueMoveError::InvalidVersion => ApiError::invalid_field(
            "expected_version",
            "Use a positive canonical decimal string",
        ),
        IssueMoveError::InvalidIdempotencyKey => {
            ApiError::invalid_field("idempotency_key", "Use a canonical UUID")
        }
        IssueMoveError::Database(_) | IssueMoveError::Access(AccessStoreError::Database(_)) => {
            storage_unavailable()
        }
        IssueMoveError::Session(error) => session_error(error),
        _ => unexpected(),
    }
}
