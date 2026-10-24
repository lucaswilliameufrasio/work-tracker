use super::{
    BrowserState, authenticated_token, canonical_uuid, malformed_query, positive_i64,
    session_error, storage_unavailable, unauthenticated,
};
use crate::{
    access::AccessStoreError,
    http::error::ApiError,
    issues::{
        IssuePriority,
        fields::{IssueFieldError, SetIssuePriority},
    },
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
    priority: String,
}

#[derive(Serialize)]
struct Receipt {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    priority: &'static str,
    result_version: String,
    changed: bool,
}

pub(super) async fn set_priority(
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
    let key = super::create::idempotency_key(&request.headers)?;
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| malformed())?;
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    let expected_version = positive_i64(&input.expected_version, "expected_version")?;
    let priority = parse_priority(&input.priority).ok_or_else(|| {
        ApiError::invalid_field("priority", "Use none, low, medium, high or urgent")
    })?;

    let outcome = state
        .field_writer
        .set_priority(
            &token,
            &project_id,
            issue_number,
            key,
            SetIssuePriority {
                expected_version,
                priority,
            },
            Utc::now(),
        )
        .await
        .map_err(field_error)?;
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
            priority: priority_name(receipt.priority),
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

fn parse_priority(value: &str) -> Option<IssuePriority> {
    match value {
        "none" => Some(IssuePriority::None),
        "low" => Some(IssuePriority::Low),
        "medium" => Some(IssuePriority::Medium),
        "high" => Some(IssuePriority::High),
        "urgent" => Some(IssuePriority::Urgent),
        _ => None,
    }
}

fn priority_name(priority: IssuePriority) -> &'static str {
    match priority {
        IssuePriority::None => "none",
        IssuePriority::Low => "low",
        IssuePriority::Medium => "medium",
        IssuePriority::High => "high",
        IssuePriority::Urgent => "urgent",
    }
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

fn field_error(error: IssueFieldError) -> ApiError {
    match error {
        IssueFieldError::Unauthenticated => unauthenticated(),
        IssueFieldError::Unavailable => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        IssueFieldError::VersionConflict => ApiError::new(
            StatusCode::CONFLICT,
            "issue_version_conflict",
            "Reload the issue before changing its priority",
            false,
        ),
        IssueFieldError::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency key was used for a different request",
            false,
        ),
        IssueFieldError::FingerprintKeyUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "idempotency_unavailable",
            "Original request key unavailable; do not recreate with a new key",
            false,
        ),
        IssueFieldError::VersionExhausted => ApiError::new(
            StatusCode::CONFLICT,
            "issue_capacity_exhausted",
            "Issue version is exhausted",
            false,
        ),
        IssueFieldError::TimeRegression => ApiError::new(
            StatusCode::CONFLICT,
            "issue_clock_conflict",
            "Issue update time is unavailable",
            false,
        ),
        IssueFieldError::InvalidVersion => ApiError::invalid_field(
            "expected_version",
            "Use a positive canonical decimal string",
        ),
        IssueFieldError::InvalidIdempotencyKey => {
            ApiError::invalid_field("idempotency_key", "Use a canonical UUID")
        }
        IssueFieldError::Database(_) | IssueFieldError::Access(AccessStoreError::Database(_)) => {
            storage_unavailable()
        }
        IssueFieldError::Session(error) => session_error(error),
        _ => unexpected(),
    }
}
