use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{HeaderValue, Method, Request, StatusCode, header},
    response::Response,
};
use chrono::{TimeDelta, Utc};
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        SessionToken,
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    projects::ProjectStore,
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

async fn user(database: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Browser user', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')")
        .bind(&id).bind(role).execute(database.pool()).await.unwrap();
    id
}

async fn database(directory: &TempDir) -> Arc<Database> {
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Browser API tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool()).await.unwrap();
    database
}

fn app(directory: &TempDir, database: Arc<Database>) -> Router {
    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    http::router_with_browser_sessions(database, security, policy())
}

fn request(uri: &str, token: Option<&SessionToken>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(uri)
        .header(header::HOST, "tracker.example.test");
    if let Some(token) = token {
        request = request.header(
            header::COOKIE,
            format!("__Host-puxiri_session={}", token.expose()),
        );
    }
    request.body(Body::empty()).unwrap()
}

async fn json(response: Response, status: StatusCode) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY)
    );
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn routes_are_opt_in_and_invalid_origin_cli_fails_before_creating_data() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let response = http::router(database)
        .oneshot(request("/api/v1/projects", None))
        .await
        .unwrap();
    assert_eq!(
        json(response, StatusCode::NOT_FOUND).await["code"],
        "not_found"
    );
    let data = directory.path().join("invalid-origin-must-not-exist");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args(["serve", "--data-dir"])
        .arg(&data)
        .args(["--browser-origin", "http://tracker.example.test"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!data.exists());
}

#[tokio::test]
async fn current_session_is_cookie_derived_no_store_and_returns_csrf_not_the_bearer() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions
        .issue(&actor, Utc::now() - TimeDelta::minutes(10))
        .await
        .unwrap();
    let app = app(&directory, database.clone());
    let result = json(
        app.clone()
            .oneshot(request("/api/v1/auth/session", Some(&issued.token)))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(result["user_id"], actor);
    assert_eq!(result["session_id"], issued.id);
    assert_eq!(result.as_object().unwrap().len(), 3);
    assert_ne!(
        result["csrf_token"].as_str().unwrap(),
        issued.token.expose()
    );
    SessionToken::parse(result["csrf_token"].as_str().unwrap()).unwrap();
    let last_activity: String =
        sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&last_activity).unwrap()
            > Utc::now() - TimeDelta::minutes(1)
    );
    sessions
        .revoke(&actor, &issued.id, Utc::now())
        .await
        .unwrap();
    let error = json(
        app.oneshot(request("/api/v1/auth/session", Some(&issued.token)))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(error["code"], "unauthenticated");
    assert!(error.get("csrf_token").is_none());
    assert!(error["request_id"].as_str().unwrap().len() == 36);
}

#[tokio::test]
async fn project_api_applies_current_access_pagination_archives_and_operational_separation() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let owner = user(&database, "owner").await;
    let admin = user(&database, "admin").await;
    let member = user(&database, "guest").await;
    let projects = ProjectStore::new(database.pool().clone());
    let aa = projects
        .create(
            &owner,
            "AA",
            "First",
            Some("Private description"),
            Utc::now(),
        )
        .await
        .unwrap();
    let bb = projects
        .create(&owner, "BB", "Second", None, Utc::now())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')")
        .bind(&aa.id).bind(&member).execute(database.pool()).await.unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let owner_session = sessions.issue(&owner, Utc::now()).await.unwrap();
    let admin_session = sessions.issue(&admin, Utc::now()).await.unwrap();
    let member_session = sessions.issue(&member, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let first = json(
        app.clone()
            .oneshot(request(
                "/api/v1/projects?page_size=1",
                Some(&owner_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["data"][0]["id"], aa.id);
    assert_eq!(first["next_cursor"], "AA");
    assert_eq!(first["has_more"], true);
    assert!(first["data"][0].get("description").is_none());
    assert!(first["data"][0].get("archived_at").is_none());
    let last = json(
        app.clone()
            .oneshot(request(
                "/api/v1/projects?cursor=AA&page_size=1",
                Some(&owner_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(last["data"][0]["id"], bb.id);
    assert_eq!(last["has_more"], false);
    assert!(last.get("next_cursor").is_none());
    projects
        .set_archived(&owner, &aa.id, true, Utc::now())
        .await
        .unwrap();
    let hidden = json(
        app.clone()
            .oneshot(request("/api/v1/projects", Some(&member_session.token)))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(hidden["data"].as_array().unwrap().is_empty());
    let archived = json(
        app.clone()
            .oneshot(request(
                "/api/v1/projects?include_archived=true",
                Some(&member_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(archived["data"].as_array().unwrap().len(), 1);
    assert_eq!(archived["data"][0]["role"], "viewer");
    assert!(archived["data"][0]["archived_at"].is_string());
    let admin_navigation = json(
        app.clone()
            .oneshot(request("/api/v1/projects", Some(&admin_session.token)))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(admin_navigation["data"].as_array().unwrap().is_empty());
    let operational = json(
        app.clone()
            .oneshot(request(
                "/api/v1/admin/projects?include_archived=true",
                Some(&admin_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(operational["data"].as_array().unwrap().len(), 2);
    assert_eq!(operational["data"][0]["membership_count"], 2);
    assert_eq!(operational["data"][0]["issue_count"], 0);
    assert!(operational["data"][0].get("role").is_none());
    json(
        app.clone()
            .oneshot(request(
                "/api/v1/admin/projects",
                Some(&member_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    sqlx::query("UPDATE organization_members SET role = 'member' WHERE user_id = ?")
        .bind(&admin)
        .execute(database.pool())
        .await
        .unwrap();
    json(
        app.clone()
            .oneshot(request(
                "/api/v1/admin/projects",
                Some(&admin_session.token),
            ))
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&aa.id)
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    let removed = json(
        app.oneshot(request(
            "/api/v1/projects?include_archived=true",
            Some(&member_session.token),
        ))
        .await
        .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(removed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn hostile_transport_and_unknown_or_duplicate_query_fields_are_rejected_without_reflection() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let issued = SessionStore::new(database.pool().clone(), policy())
        .issue(&actor, Utc::now())
        .await
        .unwrap();
    let app = app(&directory, database);
    json(
        app.clone()
            .oneshot(request("/api/v1/projects", None))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    for (name, value, status) in [
        ("host", "attacker.example.test", StatusCode::FORBIDDEN),
        (
            "origin",
            "https://attacker.example.test",
            StatusCode::FORBIDDEN,
        ),
        (
            "authorization",
            "Bearer test-only-not-accepted",
            StatusCode::UNAUTHORIZED,
        ),
        ("sec-fetch-site", "same-site", StatusCode::FORBIDDEN),
    ] {
        let mut req = request("/api/v1/projects", Some(&issued.token));
        req.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        let error = json(app.clone().oneshot(req).await.unwrap(), status).await;
        assert!(!error.to_string().contains(value));
    }
    let mut duplicate = request("/api/v1/projects", Some(&issued.token));
    duplicate.headers_mut().append(
        header::COOKIE,
        HeaderValue::from_str(&format!("__Host-puxiri_session={}", issued.token.expose())).unwrap(),
    );
    json(
        app.clone().oneshot(duplicate).await.unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    for uri in [
        "/api/v1/projects?actor_user_id=somebody",
        "/api/v1/projects?page_size=1&page_size=2",
        "/api/v1/projects?include_archived=invalid",
        "/api/v1/projects?session_token=not-accepted",
        "/api/v1/auth/session?user_id=somebody",
    ] {
        let error = json(
            app.clone()
                .oneshot(request(uri, Some(&issued.token)))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(error["code"], "malformed_request");
    }
    for (uri, field) in [
        ("/api/v1/projects?page_size=0", "page_size"),
        ("/api/v1/projects?page_size=101", "page_size"),
        ("/api/v1/projects?cursor=bad", "cursor"),
    ] {
        let error = json(
            app.clone()
                .oneshot(request(uri, Some(&issued.token)))
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert!(error["field_errors"][field].is_string());
    }
}

#[tokio::test]
async fn routes_are_read_only_head_is_empty_and_login_or_signup_stay_closed() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let issued = SessionStore::new(database.pool().clone(), policy())
        .issue(&actor, Utc::now())
        .await
        .unwrap();
    let app = app(&directory, database);
    for method in [Method::POST, Method::PATCH, Method::DELETE, Method::OPTIONS] {
        let mut req = request("/api/v1/projects", Some(&issued.token));
        *req.method_mut() = method;
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
        json(response, StatusCode::METHOD_NOT_ALLOWED).await;
    }
    for path in [
        "/auth/login",
        "/api/v1/auth/login",
        "/api/v1/auth/signup",
        "/setup",
        "/mcp",
    ] {
        json(
            app.clone()
                .oneshot(request(path, Some(&issued.token)))
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let mut head = request("/api/v1/projects", Some(&issued.token));
    *head.method_mut() = Method::HEAD;
    let head = app.oneshot(head).await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
}

#[tokio::test]
async fn ban_expiry_storage_failure_and_corrupt_rows_do_not_become_successful_or_anonymous_responses()
 {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "owner").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, Utc::now()).await.unwrap();
    let expired = sessions
        .issue(&actor, Utc::now() - TimeDelta::days(8))
        .await
        .unwrap();
    ProjectStore::new(database.pool().clone())
        .create(&actor, "BAD", "Bad archive", None, Utc::now())
        .await
        .unwrap();
    let app = app(&directory, database.clone());
    json(
        app.clone()
            .oneshot(request("/api/v1/projects", Some(&expired.token)))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    sqlx::query("UPDATE projects SET archived_at = 'malformed'")
        .execute(database.pool())
        .await
        .unwrap();
    let corrupted = json(
        app.clone()
            .oneshot(request(
                "/api/v1/projects?include_archived=true",
                Some(&issued.token),
            ))
            .await
            .unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert_eq!(corrupted["code"], "unexpected_error");
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&actor)
        .execute(database.pool())
        .await
        .unwrap();
    json(
        app.clone()
            .oneshot(request("/api/v1/projects", Some(&issued.token)))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    database.pool().close().await;
    let error = json(
        app.oneshot(request("/api/v1/projects", Some(&issued.token)))
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["retryable"], true);
    assert_eq!(error["code"], "storage_unavailable");
    assert!(!error.to_string().contains("SELECT"));
}
