use std::sync::Arc;

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::mcp::{GrantError, IssuedGrant};

use super::{
    BrowserState, authenticated_token, malformed_query, storage_unavailable, unauthenticated,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantInput {
    scope: String,
    all_projects: bool,
    #[serde(default)]
    projects: Vec<String>,
}

#[derive(Serialize)]
struct GrantBody {
    configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    all_projects: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projects: Option<Vec<String>>,
}

pub(super) async fn show(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, crate::http::error::ApiError> {
    let user_id = actor(&state, request).await?;
    let grant = crate::mcp::grant_status(state.database.pool(), &user_id)
        .await
        .map_err(grant_error)?;
    Ok(no_store(grant_document(grant)))
}

pub(super) async fn replace(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, crate::http::error::ApiError> {
    let (parts, request_body) = request.into_parts();
    let token = authenticated_token(&state, &parts).await?;
    if parts.uri.query().is_some() {
        return Err(malformed_query());
    }
    let input: GrantInput = read_json(request_body).await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let actor = state
        .sessions
        .authenticate_in_transaction(&mut transaction, &token, Utc::now())
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(unauthenticated)?;
    let user_id = actor.user_id().to_owned();
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let grant = crate::mcp::replace_grant(
        state.database.pool(),
        &user_id,
        &input.scope,
        input.all_projects,
        &input.projects,
        Utc::now(),
    )
    .await
    .map_err(grant_error)?;
    Ok(no_store(grant_document(Some(grant))))
}

pub(super) async fn revoke(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, crate::http::error::ApiError> {
    let user_id = actor(&state, request).await?;
    crate::mcp::revoke_grant(state.database.pool(), &user_id)
        .await
        .map_err(grant_error)?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, "no-store")],
    )
        .into_response())
}

async fn actor(
    state: &BrowserState,
    request: Request,
) -> Result<String, crate::http::error::ApiError> {
    let (parts, _) = request.into_parts();
    let token = authenticated_token(state, &parts).await?;
    if parts.uri.query().is_some() {
        return Err(malformed_query());
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let actor = state
        .sessions
        .authenticate_in_transaction(&mut transaction, &token, Utc::now())
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(unauthenticated)?;
    let user_id = actor.user_id().to_owned();
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    Ok(user_id)
}

fn grant_document(grant: Option<IssuedGrant>) -> GrantBody {
    match grant {
        None => GrantBody {
            configured: false,
            scope: None,
            all_projects: None,
            projects: None,
        },
        Some(grant) => GrantBody {
            configured: true,
            scope: Some(grant.scope),
            all_projects: Some(grant.all_projects),
            projects: Some(grant.projects),
        },
    }
}

fn no_store(body: GrantBody) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn grant_error(error: GrantError) -> crate::http::error::ApiError {
    match error {
        GrantError::Invalid => {
            crate::http::error::ApiError::invalid_field("projects", "The MCP grant is not valid")
        }
        GrantError::ProjectUnavailable => crate::http::error::ApiError::invalid_field(
            "projects",
            "Each project must already include this person",
        ),
        GrantError::Database => storage_unavailable(),
    }
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    body: Body,
) -> Result<T, crate::http::error::ApiError> {
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| {
        crate::http::error::ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request body could not be read",
            false,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        crate::http::error::ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request JSON is malformed or unsupported",
            false,
        )
    })
}
