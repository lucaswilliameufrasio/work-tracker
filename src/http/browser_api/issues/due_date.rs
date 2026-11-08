use super::{
    BrowserState, authenticated_token, canonical_uuid, malformed_query, positive_i64,
    priority::field_error,
};
use crate::{
    http::error::ApiError,
    issues::{deadlines, fields::SetIssueDueDate},
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
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_version: String,
    due_date: Value,
}

#[derive(Serialize)]
struct Receipt {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    due_date: Option<String>,
    result_version: String,
    changed: bool,
}

pub(super) async fn set_due_date(
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
    let due_date = match input.due_date {
        Value::Null => None,
        Value::String(value) => Some(deadlines::parse_date(&value).ok_or_else(|| {
            ApiError::invalid_field("due_date", "Use a real canonical YYYY-MM-DD date or null")
        })?),
        _ => {
            return Err(ApiError::invalid_field(
                "due_date",
                "Use a real canonical YYYY-MM-DD date or null",
            ));
        }
    };

    let outcome = state
        .field_writer
        .set_due_date(
            &token,
            &project_id,
            issue_number,
            key,
            SetIssueDueDate {
                expected_version,
                due_date,
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
            due_date: receipt
                .due_date
                .map(|date| date.format("%Y-%m-%d").to_string()),
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
