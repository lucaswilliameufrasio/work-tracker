use std::sync::Arc;

use axum::{
    Json,
    body::to_bytes,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    BrowserState, authenticated_token, canonical_uuid, malformed_query, session_error,
    storage_unavailable, unauthenticated,
};
use crate::{
    access::AccessStoreError,
    http::error::ApiError,
    issues::{
        IssueCreationError, NewIssue,
        creation::{IssueCreationOutcome, SessionIssueCreationError},
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    title: String,
    #[serde(default, deserialize_with = "present_string")]
    description: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    assignee_user_id: Option<String>,
}

fn present_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Serialize)]
struct Receipt {
    id: String,
    project_id: String,
    issue_number: String,
    issue_key: String,
    created_version: String,
}

pub(super) async fn create(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path(project_id) =
        path.map_err(|_| ApiError::invalid_field("project_id", "Use a canonical UUID"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let mut keys = request.headers.get_all("idempotency-key").iter();
    let key = keys
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::invalid_field(
                "idempotency_key",
                "Supply one canonical UUID Idempotency-Key",
            )
        })?;
    if keys.next().is_some() {
        return Err(malformed_query());
    }
    if !canonical_uuid(key) {
        return Err(ApiError::invalid_field(
            "idempotency_key",
            "Supply one canonical UUID Idempotency-Key",
        ));
    }
    // Admission already collected this bounded body before entering the handler.
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request body could not be read",
            false,
        )
    })?;
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request JSON is malformed or unsupported",
            false,
        )
    })?;
    if input
        .assignee_user_id
        .as_deref()
        .is_some_and(|value| !canonical_uuid(value))
    {
        return Err(ApiError::invalid_field(
            "assignee_user_id",
            "Use a canonical UUID",
        ));
    }
    let outcome = state
        .writer
        .create(
            &token,
            &project_id,
            key,
            NewIssue {
                title: &input.title,
                description: input.description.as_deref(),
                assignee_user_id: input.assignee_user_id.as_deref(),
            },
            Utc::now(),
        )
        .await
        .map_err(creation_error)?;
    let replayed = matches!(outcome, IssueCreationOutcome::Replayed(_));
    let receipt = outcome.receipt();
    let location = HeaderValue::from_str(&format!(
        "/api/v1/projects/{}/issues/{}",
        receipt.project_id, receipt.issue_number
    ))
    .map_err(|_| unexpected())?;
    let body = Receipt {
        id: receipt.issue_id.clone(),
        project_id: receipt.project_id.clone(),
        issue_number: receipt.issue_number.to_string(),
        issue_key: receipt.issue_key.clone(),
        created_version: receipt.created_version.to_string(),
    };
    let mut response = (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(
        "idempotency-replayed",
        HeaderValue::from_static(if replayed { "true" } else { "false" }),
    );
    Ok(response)
}

fn unexpected() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected_error",
        "Unexpected error",
        false,
    )
}

fn creation_error(error: SessionIssueCreationError) -> ApiError {
    match error {
        SessionIssueCreationError::Unauthenticated => unauthenticated(),
        SessionIssueCreationError::Unavailable
        | SessionIssueCreationError::Creation(
            IssueCreationError::Forbidden | IssueCreationError::UnknownProjectOrActor,
        ) => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        SessionIssueCreationError::InvalidIdempotencyKey => {
            ApiError::invalid_field("idempotency_key", "Use a canonical UUID")
        }
        SessionIssueCreationError::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency key was used for a different request",
            false,
        ),
        SessionIssueCreationError::FingerprintKeyUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "idempotency_unavailable",
            "The original request key is unavailable; do not recreate this operation with a new key",
            false,
        ),
        SessionIssueCreationError::Creation(IssueCreationError::InvalidTitle) => {
            ApiError::invalid_field("title", "Use a nonempty title with at most 240 characters")
        }
        SessionIssueCreationError::Creation(IssueCreationError::DescriptionTooLong) => {
            ApiError::invalid_field("description", "Use at most 65536 UTF-8 bytes")
        }
        SessionIssueCreationError::Creation(IssueCreationError::AssigneeUnavailable) => {
            ApiError::invalid_field(
                "assignee_user_id",
                "Assignee is unavailable for this project",
            )
        }
        SessionIssueCreationError::Creation(IssueCreationError::DefaultSchemaUnavailable) => {
            ApiError::new(
                StatusCode::CONFLICT,
                "issue_schema_unavailable",
                "Issue defaults are unavailable",
                false,
            )
        }
        SessionIssueCreationError::Creation(
            IssueCreationError::IssueNumberExhausted | IssueCreationError::RankSpaceExhausted,
        ) => ApiError::new(
            StatusCode::CONFLICT,
            "issue_capacity_exhausted",
            "Issue allocation is unavailable",
            false,
        ),
        SessionIssueCreationError::Database(_)
        | SessionIssueCreationError::Creation(IssueCreationError::Database(_))
        | SessionIssueCreationError::Access(AccessStoreError::Database(_))
        | SessionIssueCreationError::Creation(IssueCreationError::AccessState(
            AccessStoreError::Database(_),
        )) => storage_unavailable(),
        SessionIssueCreationError::Session(error) => session_error(error),
        _ => unexpected(),
    }
}
