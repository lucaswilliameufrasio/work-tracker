use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::Response,
    routing::get,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    access::{AccessStoreError, ProjectRole},
    http::error::ApiError,
    projects::{
        membership::{ProjectMembershipError, ProjectMembershipRecord},
        session::SessionProjectReadError,
    },
};

use super::{
    BrowserState, Page, authenticated_token, malformed_query, no_store, session_error,
    storage_unavailable, unauthenticated,
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route("/api/v1/projects/{project_id}/members", get(list))
        .route(
            "/api/v1/projects/{project_id}/members/{user_id}",
            get(get_member),
        )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    cursor: Option<String>,
    page_size: Option<u16>,
}

#[derive(Serialize)]
struct Member {
    user_id: String,
    display_name: String,
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_email: Option<String>,
    can_be_assigned: bool,
}

impl TryFrom<ProjectMembershipRecord> for Member {
    type Error = ApiError;

    fn try_from(record: ProjectMembershipRecord) -> Result<Self, Self::Error> {
        if !canonical_uuid(&record.user_id) {
            return Err(unexpected());
        }
        Ok(Self {
            user_id: record.user_id,
            display_name: record.display_name,
            role: match record.role {
                ProjectRole::Manager => "manager",
                ProjectRole::Member => "member",
                ProjectRole::Viewer => "viewer",
            },
            primary_email: record.primary_email,
            can_be_assigned: record.can_be_assigned,
        })
    }
}

async fn list(
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
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    if query
        .cursor
        .as_deref()
        .is_some_and(|cursor| !canonical_uuid(cursor))
    {
        return Err(ApiError::invalid_field("cursor", "Use a canonical UUID"));
    }
    let page = state
        .projects
        .list_project_members(
            &token,
            &project_id,
            query.cursor.as_deref(),
            query.page_size.unwrap_or(50),
            Utc::now(),
        )
        .await
        .map_err(membership_read_error)?
        .ok_or_else(not_found)?;
    let mut data = Vec::with_capacity(page.items.len());
    for record in page.items {
        data.push(Member::try_from(record)?);
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_user_id.is_some(),
        next_cursor: page.next_after_user_id,
    }))
}

async fn get_member(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path((project_id, user_id)) =
        path.map_err(|_| ApiError::invalid_field("path", "Use canonical UUID selectors"))?;
    if !canonical_uuid(&project_id) {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    if !canonical_uuid(&user_id) {
        return Err(ApiError::invalid_field("user_id", "Use a canonical UUID"));
    }
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let member = state
        .projects
        .get_project_member(&token, &project_id, &user_id, Utc::now())
        .await
        .map_err(membership_read_error)?
        .ok_or_else(not_found)?;
    Ok(no_store(Member::try_from(member)?))
}

fn membership_read_error(error: SessionProjectReadError) -> ApiError {
    match error {
        SessionProjectReadError::Unauthenticated => unauthenticated(),
        SessionProjectReadError::Session(error) => session_error(error),
        SessionProjectReadError::Database(_)
        | SessionProjectReadError::Membership(ProjectMembershipError::Database(_))
        | SessionProjectReadError::Membership(ProjectMembershipError::Access(
            AccessStoreError::Database(_),
        )) => storage_unavailable(),
        SessionProjectReadError::Membership(ProjectMembershipError::Forbidden)
        | SessionProjectReadError::Membership(ProjectMembershipError::MembershipNotFound) => {
            not_found()
        }
        SessionProjectReadError::Membership(ProjectMembershipError::InvalidCursor) => {
            ApiError::invalid_field("cursor", "Use a canonical UUID")
        }
        SessionProjectReadError::Membership(ProjectMembershipError::InvalidMemberId) => {
            ApiError::invalid_field("user_id", "Use a canonical UUID")
        }
        SessionProjectReadError::Membership(ProjectMembershipError::InvalidPageSize) => {
            ApiError::invalid_field("page_size", "Use a value between 1 and 100")
        }
        _ => unexpected(),
    }
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::parse_str(value)
            .map(|id| id.to_string() == value)
            .unwrap_or(false)
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "Resource not found",
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
