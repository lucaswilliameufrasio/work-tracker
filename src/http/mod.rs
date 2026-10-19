use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use serde::Serialize;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::db::Database;

mod browser_api;
mod browser_budget;
pub mod browser_session;
mod error;
pub mod openapi;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

pub fn router(database: Arc<Database>) -> Router {
    security_headers(base_router(database))
}

pub fn router_with_browser_sessions(
    database: Arc<Database>,
    security: browser_session::BrowserSessionSecurity,
    policy: crate::auth::session::SessionPolicy,
) -> Router {
    security_headers(
        base_router(database.clone()).merge(browser_api::routes(database, security, policy)),
    )
}

fn base_router(database: Arc<Database>) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .fallback(frontend)
        .with_state(database)
}

fn security_headers(router: Router) -> Router {
    router.layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn live() -> Json<Health> {
    Json(Health { status: "live" })
}

async fn ready(State(database): State<Arc<Database>>) -> Response {
    let (code, status) = match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(database.pool())
        .await
    {
        Ok(1) => (StatusCode::OK, "ready"),
        _ => {
            tracing::warn!("database readiness probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
        }
    };
    (
        code,
        [(header::CACHE_CONTROL, "no-store")],
        Json(Health { status }),
    )
        .into_response()
}

async fn frontend(request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    if path.split('/').next().is_some_and(|prefix| {
        matches!(
            prefix,
            "api" | "auth" | "admin" | "setup" | "mcp" | "health"
        )
    }) {
        return error::ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            false,
        )
        .into_response();
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }
    if path.contains("..") || path.contains('%') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (name, cache) = if path.is_empty()
        || path == "index.html"
        || !path.contains('.') && !path.starts_with("assets/")
    {
        ("index.html", "no-cache")
    } else {
        (path, "public, max-age=31536000, immutable")
    };
    let Some(asset) = Assets::get(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let body = if request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.data.into_owned())
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}
