use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    access::{AccessStoreError, ProjectRole},
    auth::{
        SessionToken,
        session::{
            SessionError, SessionListOptions, SessionPolicy, SessionRevocationOutcome, SessionStore,
        },
        time::format_timestamp,
    },
    db::Database,
    projects::{
        ProjectListOptions, ProjectReadError,
        session::{SessionProjectReadError, SessionProjectReader},
    },
};

use super::{
    browser_session::{BrowserSessionError, BrowserSessionSecurity},
    error::ApiError,
};

struct BrowserState {
    database: Arc<Database>,
    security: BrowserSessionSecurity,
    sessions: SessionStore,
    projects: SessionProjectReader,
    issues: crate::issues::session::SessionIssueReader,
    catalogs: crate::projects::catalog::SessionProjectCatalogReader,
    writer: crate::issues::creation::SessionIssueWriter,
    field_writer: crate::issues::fields::SessionIssueFieldWriter,
    text_writer: crate::issues::text::SessionIssueTextWriter,
    mover: crate::issues::movement::SessionIssueMover,
}

mod catalogs;
mod issues;

pub(super) fn routes(
    database: Arc<Database>,
    security: BrowserSessionSecurity,
    policy: SessionPolicy,
) -> Router {
    let budget = Arc::new(super::browser_budget::BrowserBudget::with_security(
        security.clone(),
    ));
    let writer = crate::issues::creation::SessionIssueWriter::new(
        database.pool().clone(),
        policy,
        security.master_key(),
    );
    let state = Arc::new(BrowserState {
        field_writer: crate::issues::fields::SessionIssueFieldWriter::new(
            database.pool().clone(),
            policy,
            security.master_key(),
        ),
        text_writer: crate::issues::text::SessionIssueTextWriter::new(
            database.pool().clone(),
            policy,
            security.master_key(),
        ),
        mover: crate::issues::movement::SessionIssueMover::new(
            database.pool().clone(),
            policy,
            security.master_key(),
        ),
        writer,
        security,
        sessions: SessionStore::new(database.pool().clone(), policy),
        projects: SessionProjectReader::new(database.pool().clone(), policy),
        issues: crate::issues::session::SessionIssueReader::new(database.pool().clone(), policy),
        catalogs: crate::projects::catalog::SessionProjectCatalogReader::new(
            database.pool().clone(),
            policy,
        ),
        database,
    });
    Router::new()
        .route("/api/v1/auth/session", get(current_session).delete(logout))
        .route(
            "/api/v1/auth/sessions",
            get(list_sessions).delete(revoke_all_sessions),
        )
        .route("/api/v1/auth/sessions/{session_id}", delete(revoke_session))
        .route("/api/v1/projects", get(accessible_projects))
        .route(
            "/api/v1/projects/{project_id}/capabilities",
            get(project_capabilities),
        )
        .route("/api/v1/admin/projects", get(operational_projects))
        .merge(issues::routes())
        .merge(catalogs::routes())
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            budget,
            super::browser_budget::admit,
        ))
}

#[derive(Serialize)]
struct ProjectCapabilities {
    project_id: String,
    can_create_issues: bool,
    can_update_issue_fields: bool,
    can_replace_issue_text: bool,
    can_move_issues: bool,
}

async fn project_capabilities(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let Path(project_id) =
        path.map_err(|_| ApiError::invalid_field("project_id", "Use a canonical UUID"))?;
    if uuid::Uuid::parse_str(&project_id)
        .map(|id| id.to_string())
        .ok()
        .as_deref()
        != Some(project_id.as_str())
    {
        return Err(ApiError::invalid_field(
            "project_id",
            "Use a canonical UUID",
        ));
    }
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let can_create_issues = state
        .projects
        .can_create_issues(&token, &project_id, Utc::now())
        .await
        .map_err(project_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Resource not found",
                false,
            )
        })?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(ProjectCapabilities {
            project_id,
            can_create_issues,
            can_update_issue_fields: can_create_issues,
            can_replace_issue_text: can_create_issues,
            can_move_issues: can_create_issues,
        }),
    )
        .into_response())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    cursor: Option<String>,
    page_size: Option<u16>,
    include_archived: Option<bool>,
}

impl ListQuery {
    fn options(&self) -> ProjectListOptions<'_> {
        ProjectListOptions {
            after_project_key: self.cursor.as_deref(),
            page_size: self.page_size.unwrap_or(50),
            include_archived: self.include_archived.unwrap_or(false),
        }
    }
}

#[derive(Serialize)]
struct Page<T> {
    data: Vec<T>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct AccessibleProject {
    id: String,
    project_key: String,
    name: String,
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
}

#[derive(Serialize)]
struct OperationalProject {
    id: String,
    project_key: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
    membership_count: i64,
    issue_count: i64,
}

#[derive(Serialize)]
struct CurrentSession<'a> {
    user_id: &'a str,
    session_id: &'a str,
    csrf_token: &'a str,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionListQuery {
    cursor: Option<String>,
    page_size: Option<u16>,
}

#[derive(Serialize)]
struct ListedSession {
    id: String,
    created_at: String,
    last_activity_at: String,
    expires_at: String,
    is_current: bool,
}

async fn list_sessions(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let query = Query::<SessionListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let page = state
        .sessions
        .list_owned(
            &token,
            SessionListOptions {
                after_session_id: query.cursor.as_deref(),
                page_size: query.page_size.unwrap_or(50),
            },
            Utc::now(),
        )
        .await
        .map_err(session_error)?
        .ok_or_else(unauthenticated)?;
    let mut data = Vec::with_capacity(page.items.len());
    for session in page.items {
        data.push(ListedSession {
            id: session.id,
            created_at: format_timestamp(session.created_at),
            last_activity_at: format_timestamp(session.last_activity_at),
            expires_at: format_timestamp(session.expires_at),
            is_current: session.is_current,
        });
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_session_id.is_some(),
        next_cursor: page.next_after_session_id,
    }))
}

async fn authenticated_token(
    state: &BrowserState,
    request: &axum::http::request::Parts,
) -> Result<SessionToken, ApiError> {
    let token = state
        .security
        .authenticate_request(&request.method, &request.uri, &request.headers)
        .map_err(transport_error)?;
    state
        .sessions
        .validate(&token, Utc::now())
        .await
        .map_err(session_error)?
        .ok_or_else(unauthenticated)?;
    Ok(token)
}

async fn current_session(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
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
        .map_err(session_error)?
        .ok_or_else(unauthenticated)?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let csrf = state.security.csrf_token(&token);
    Ok(no_store(CurrentSession {
        user_id: principal.user_id(),
        session_id: principal.session_id(),
        csrf_token: csrf.expose(),
    }))
}

async fn logout(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    if !state
        .sessions
        .revoke_current(&token, Utc::now())
        .await
        .map_err(session_error)?
    {
        return Err(unauthenticated());
    }
    Ok(revocation_response(true))
}

async fn revoke_session(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let Path(session_id) =
        path.map_err(|_| ApiError::invalid_field("session_id", "Use a canonical UUID"))?;
    if uuid::Uuid::parse_str(&session_id)
        .map(|id| id.to_string() != session_id)
        .unwrap_or(true)
    {
        return Err(ApiError::invalid_field(
            "session_id",
            "Use a canonical UUID",
        ));
    }
    match state
        .sessions
        .revoke_owned(&token, &session_id, Utc::now())
        .await
        .map_err(session_error)?
    {
        SessionRevocationOutcome::Unauthenticated => Err(unauthenticated()),
        SessionRevocationOutcome::NotFound => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        )),
        SessionRevocationOutcome::Revoked { is_current } => Ok(revocation_response(is_current)),
    }
}

async fn revoke_all_sessions(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    if !state
        .sessions
        .revoke_all_owned(&token, Utc::now())
        .await
        .map_err(session_error)?
    {
        return Err(unauthenticated());
    }
    Ok(revocation_response(true))
}

fn revocation_response(clear_cookie: bool) -> Response {
    let mut response = (
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
    )
        .into_response();
    if clear_cookie {
        response.headers_mut().insert(
            header::SET_COOKIE,
            super::browser_session::clear_session_cookie(),
        );
    }
    response
}

async fn accessible_projects(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let page = state
        .projects
        .list_accessible(&token, query.options(), Utc::now())
        .await
        .map_err(project_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for project in page.items {
        data.push(AccessibleProject {
            id: project.id,
            project_key: project.project_key,
            name: project.name,
            role: match project.role {
                ProjectRole::Manager => "manager",
                ProjectRole::Member => "member",
                ProjectRole::Viewer => "viewer",
            },
            archived_at: project.archived_at.map(format_timestamp),
        });
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_project_key.is_some(),
        next_cursor: page.next_after_project_key,
    }))
}

async fn operational_projects(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    let query = Query::<ListQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let page = state
        .projects
        .list_operational_catalog(&token, query.options(), Utc::now())
        .await
        .map_err(project_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for project in page.items {
        data.push(OperationalProject {
            id: project.id,
            project_key: project.project_key,
            name: project.name,
            archived_at: project.archived_at.map(format_timestamp),
            membership_count: project.membership_count,
            issue_count: project.issue_count,
        });
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_project_key.is_some(),
        next_cursor: page.next_after_project_key,
    }))
}

async fn method_not_allowed(request: Request) -> Response {
    let mut response = ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Method not supported for this resource",
        false,
    )
    .into_response();
    response.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static(
            if request
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .is_some_and(|path| path.as_str() == "/api/v1/projects/{project_id}/issues")
            {
                "GET, HEAD, POST"
            } else if request
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .is_some_and(|path| {
                    matches!(
                        path.as_str(),
                        "/api/v1/projects/{project_id}/issues/{issue_number}/text"
                            | "/api/v1/projects/{project_id}/issues/{issue_number}/priority"
                            | "/api/v1/projects/{project_id}/issues/{issue_number}/due-date"
                    )
                })
            {
                "PUT"
            } else if request
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .is_some_and(|path| {
                    path.as_str() == "/api/v1/projects/{project_id}/issues/{issue_number}/move"
                })
            {
                "POST"
            } else if request.uri().path().starts_with("/api/v1/auth/sessions/") {
                "DELETE"
            } else if matches!(
                request.uri().path(),
                "/api/v1/auth/session" | "/api/v1/auth/sessions"
            ) {
                "GET, HEAD, DELETE"
            } else {
                "GET, HEAD"
            },
        ),
    );
    response
}

fn no_store<T: Serialize>(body: T) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "An active session is required",
        false,
    )
}

fn storage_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "storage_unavailable",
        "Service temporarily unavailable",
        true,
    )
}

fn malformed_query() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "malformed_request",
        "Query parameters are malformed or unsupported",
        false,
    )
}

pub(super) fn transport_error(error: BrowserSessionError) -> ApiError {
    match error {
        BrowserSessionError::InvalidSessionCookie | BrowserSessionError::AmbiguousCredentials => {
            unauthenticated()
        }
        BrowserSessionError::RequestOriginDenied => ApiError::new(
            StatusCode::FORBIDDEN,
            "request_origin_denied",
            "Request origin is not allowed",
            false,
        ),
        BrowserSessionError::InvalidCsrfToken => ApiError::new(
            StatusCode::FORBIDDEN,
            "csrf_required",
            "A valid CSRF token is required",
            false,
        ),
        _ => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}

fn session_error(error: SessionError) -> ApiError {
    match error {
        SessionError::InvalidCursor => ApiError::invalid_field("cursor", "Use a canonical UUID"),
        SessionError::InvalidPageSize => {
            ApiError::invalid_field("page_size", "Use a value between 1 and 100")
        }
        SessionError::Database(_) | SessionError::Audit(_) => storage_unavailable(),
        _ => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}

fn project_error(error: SessionProjectReadError) -> ApiError {
    match error {
        SessionProjectReadError::Unauthenticated => unauthenticated(),
        SessionProjectReadError::Database(_)
        | SessionProjectReadError::Project(ProjectReadError::Database(_))
        | SessionProjectReadError::Project(ProjectReadError::AccessState(
            AccessStoreError::Database(_),
        )) => storage_unavailable(),
        SessionProjectReadError::Session(error) => session_error(error),
        SessionProjectReadError::Project(ProjectReadError::Unavailable) => ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "Permission denied",
            false,
        ),
        SessionProjectReadError::Project(ProjectReadError::InvalidCursor) => {
            ApiError::invalid_field("cursor", "Use a valid project key")
        }
        SessionProjectReadError::Project(ProjectReadError::InvalidPageSize) => {
            ApiError::invalid_field("page_size", "Use a value between 1 and 100")
        }
        _ => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}
