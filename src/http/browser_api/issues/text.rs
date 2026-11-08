use super::create::{idempotency_key, present_string};
use super::{
    BrowserState, authenticated_token, canonical_uuid, malformed_query, session_error,
    storage_unavailable, unauthenticated,
};
use crate::{
    access::AccessStoreError,
    http::error::ApiError,
    issues::{
        IssueCreationError,
        text::{IssueTextError, ReplaceIssueText},
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
    title: String,
    expected_version: String,
    #[serde(default, deserialize_with = "present_string")]
    description: Option<String>,
}

#[derive(Serialize)]
struct Receipt {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    result_version: String,
    changed: bool,
}

fn positive(value: &str, field: &'static str) -> Result<i64, ApiError> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::invalid_field(
            field,
            "Use a positive canonical i64 decimal string",
        ));
    }
    value.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
        ApiError::invalid_field(field, "Use a positive canonical i64 decimal string")
    })
}

pub(super) async fn replace(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project, number)) =
        path.map_err(|_| ApiError::invalid_field("issue_number", "Use canonical path selectors"))?;
    if !canonical_uuid(&project) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    let number = positive(&number, "issue_number")?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let key = idempotency_key(&request.headers)?;
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| malformed())?;
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    let expected_version = positive(&input.expected_version, "expected_version")?;
    let outcome = state
        .text_writer
        .replace_by_number(
            &token,
            &project,
            number,
            key,
            ReplaceIssueText {
                expected_version,
                title: &input.title,
                description: input.description.as_deref(),
            },
            Utc::now(),
        )
        .await
        .map_err(text_error)?;
    let receipt = outcome.receipt;
    let location = HeaderValue::from_str(&format!(
        "/api/v1/projects/{}/issues/{}",
        receipt.project_id, receipt.issue_number
    ))
    .map_err(|_| unexpected())?;
    let mut response = (
        [(header::CACHE_CONTROL, "no-store")],
        Json(Receipt {
            id: receipt.issue_id,
            project_id: receipt.project_id,
            issue_number: receipt.issue_number.to_string(),
            issue_key: receipt.issue_key,
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
fn text_error(error: IssueTextError) -> ApiError {
    match error {
        IssueTextError::Unauthenticated => unauthenticated(),
        IssueTextError::Unavailable => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        IssueTextError::VersionConflict => ApiError::new(
            StatusCode::CONFLICT,
            "issue_version_conflict",
            "Reload the issue and preserve your draft before editing again",
            false,
        ),
        IssueTextError::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency key was used for a different request",
            false,
        ),
        IssueTextError::FingerprintKeyUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "idempotency_unavailable",
            "Original request key unavailable; do not recreate with a new key",
            false,
        ),
        IssueTextError::VersionExhausted => ApiError::new(
            StatusCode::CONFLICT,
            "issue_capacity_exhausted",
            "Issue version is exhausted",
            false,
        ),
        IssueTextError::TimeRegression => ApiError::new(
            StatusCode::CONFLICT,
            "issue_clock_conflict",
            "Issue update time is unavailable",
            false,
        ),
        IssueTextError::InvalidVersion => ApiError::invalid_field(
            "expected_version",
            "Use a positive canonical decimal string",
        ),
        IssueTextError::InvalidIdempotencyKey => {
            ApiError::invalid_field("idempotency_key", "Use a canonical UUID")
        }
        IssueTextError::Validation(IssueCreationError::InvalidTitle) => {
            ApiError::invalid_field("title", "Use a nonempty title with at most240 characters")
        }
        IssueTextError::Validation(IssueCreationError::DescriptionTooLong) => {
            ApiError::invalid_field("description", "Use at most65536 UTF-8 bytes")
        }
        IssueTextError::Database(_) | IssueTextError::Access(AccessStoreError::Database(_)) => {
            storage_unavailable()
        }
        IssueTextError::Session(error) => session_error(error),
        _ => unexpected(),
    }
}
