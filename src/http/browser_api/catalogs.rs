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

use super::{
    BrowserState, Page, authenticated_token, malformed_query, no_store, session_error,
    storage_unavailable, unauthenticated,
};
use crate::{
    access::AccessStoreError,
    auth::time::format_timestamp,
    http::error::ApiError,
    projects::catalog::{
        CatalogFields, CatalogKind, CatalogOptions, CatalogReadError, canonical_uuid,
    },
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route("/api/v1/projects/{project_id}/statuses", get(statuses))
        .route(
            "/api/v1/projects/{project_id}/issue-types",
            get(issue_types),
        )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogQuery {
    cursor: Option<String>,
    page_size: Option<u16>,
    include_archived: Option<bool>,
}

#[derive(Serialize)]
struct CatalogItem {
    id: String,
    project_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
    #[serde(flatten)]
    fields: ItemFields,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ItemFields {
    Status {
        category: String,
        position: String,
        board_visible: bool,
    },
    IssueType {
        #[serde(skip_serializing_if = "Option::is_none")]
        icon_key: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        color_key: Option<String>,
    },
}

async fn statuses(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    list(state, path, request, CatalogKind::Statuses).await
}

async fn issue_types(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    list(state, path, request, CatalogKind::IssueTypes).await
}

async fn list(
    state: Arc<BrowserState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    kind: CatalogKind,
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
    let query = Query::<CatalogQuery>::try_from_uri(&request.uri)
        .map_err(|_| malformed_query())?
        .0;
    let page = state
        .catalogs
        .list(
            &token,
            &project_id,
            kind,
            CatalogOptions {
                after_id: query.cursor.as_deref(),
                page_size: query.page_size.unwrap_or(50),
                include_archived: query.include_archived.unwrap_or(false),
            },
            Utc::now(),
        )
        .await
        .map_err(catalog_error)?;
    let mut data = Vec::with_capacity(page.items.len());
    for record in page.items {
        let fields = match record.fields {
            CatalogFields::Status {
                category,
                position,
                board_visible,
            } => ItemFields::Status {
                category,
                position: position.to_string(),
                board_visible,
            },
            CatalogFields::IssueType {
                icon_key,
                color_key,
            } => ItemFields::IssueType {
                icon_key,
                color_key,
            },
        };
        data.push(CatalogItem {
            id: record.id,
            project_id: record.project_id,
            name: record.name,
            system_key: record.system_key,
            archived_at: record.archived_at.map(format_timestamp),
            fields,
        });
    }
    Ok(no_store(Page {
        data,
        has_more: page.next_after_id.is_some(),
        next_cursor: page.next_after_id,
    }))
}

fn catalog_error(error: CatalogReadError) -> ApiError {
    match error {
        CatalogReadError::Unauthenticated => unauthenticated(),
        CatalogReadError::Unavailable => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        ),
        CatalogReadError::InvalidCursor => {
            ApiError::invalid_field("cursor", "Use a canonical UUID")
        }
        CatalogReadError::InvalidPageSize => {
            ApiError::invalid_field("page_size", "Use a value between 1 and 100")
        }
        CatalogReadError::Database(_) | CatalogReadError::Access(AccessStoreError::Database(_)) => {
            storage_unavailable()
        }
        CatalogReadError::Session(error) => session_error(error),
        _ => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        ),
    }
}
