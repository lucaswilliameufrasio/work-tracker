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

fn logout_request(token: &SessionToken, csrf: &str) -> Request<Body> {
    let mut request = request("/api/v1/auth/session", Some(token));
    *request.method_mut() = Method::DELETE;
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://tracker.example.test"),
    );
    request
        .headers_mut()
        .insert("x-csrf-token", HeaderValue::from_str(csrf).unwrap());
    request
}

async fn csrf(app: &Router, token: &SessionToken) -> String {
    json(
        app.clone()
            .oneshot(request("/api/v1/auth/session", Some(token)))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn revoke_request(uri: &str, token: &SessionToken, csrf: &str) -> Request<Body> {
    let mut request = logout_request(token, csrf);
    *request.uri_mut() = uri.parse().unwrap();
    request
}

#[tokio::test]
async fn browser_session_listing_is_bounded_private_no_store_and_marks_the_current_session() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let stranger = user(&database, "owner").await;
    let store = SessionStore::new(database.pool().clone(), policy());
    let own = store.issue(&actor, Utc::now()).await.unwrap();
    let other = store.issue(&actor, Utc::now()).await.unwrap();
    let foreign = store.issue(&stranger, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let mut cursor = None;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let uri = match &cursor {
            Some(cursor) => format!("/api/v1/auth/sessions?page_size=1&cursor={cursor}"),
            None => String::from("/api/v1/auth/sessions?page_size=1"),
        };
        let response = app
            .clone()
            .oneshot(request(&uri, Some(&own.token)))
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        let page = json(response, StatusCode::OK).await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        let item = &page["data"][0];
        assert_eq!(item.as_object().unwrap().len(), 5);
        let id = item["id"].as_str().unwrap().to_owned();
        assert_eq!(item["is_current"], id == own.id);
        for field in ["created_at", "last_activity_at", "expires_at"] {
            chrono::DateTime::parse_from_rfc3339(item[field].as_str().unwrap()).unwrap();
        }
        assert!(!page.to_string().contains(own.token.expose()));
        assert!(!page.to_string().contains(&foreign.id));
        ids.push(id);
        cursor = page
            .get("next_cursor")
            .map(|value| value.as_str().unwrap().to_owned());
        assert_eq!(page["has_more"], cursor.is_some());
    }
    assert!(cursor.is_none());
    let mut expected = vec![own.id.clone(), other.id.clone()];
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, expected);
    let mut head = request("/api/v1/auth/sessions", Some(&own.token));
    *head.method_mut() = Method::HEAD;
    let head = app.clone().oneshot(head).await.unwrap();
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
    store.revoke(&actor, &other.id, Utc::now()).await.unwrap();
    let page = json(
        app.clone()
            .oneshot(request("/api/v1/auth/sessions", Some(&own.token)))
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    store.revoke(&actor, &own.id, Utc::now()).await.unwrap();
    json(
        app.oneshot(request("/api/v1/auth/sessions", Some(&own.token)))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    json(
        http::router(database)
            .oneshot(request("/api/v1/auth/sessions", None))
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn session_list_rejects_identity_overrides_invalid_pages_and_hostile_transport() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let store = SessionStore::new(database.pool().clone(), policy());
    let own = store.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    json(
        app.clone()
            .oneshot(request("/api/v1/auth/sessions", None))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    for suffix in [
        "user_id=other",
        "include_archived=true",
        "page_size=1&page_size=2",
        "page_size=bad",
        "cursor=a&cursor=b",
    ] {
        json(
            app.clone()
                .oneshot(request(
                    &format!("/api/v1/auth/sessions?{suffix}"),
                    Some(&own.token),
                ))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for (suffix, field) in [
        ("page_size=0", "page_size"),
        ("page_size=101", "page_size"),
        ("cursor=bad", "cursor"),
        ("cursor=", "cursor"),
    ] {
        let error = json(
            app.clone()
                .oneshot(request(
                    &format!("/api/v1/auth/sessions?{suffix}"),
                    Some(&own.token),
                ))
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert!(error["field_errors"][field].is_string());
    }
    let mut req = request("/api/v1/auth/sessions", Some(&own.token));
    req.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://attacker.example.test"),
    );
    json(
        app.clone().oneshot(req).await.unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    let corrupt = store.issue(&actor, Utc::now()).await.unwrap();
    sqlx::query("UPDATE sessions SET id = 'gggggggg-gggg-gggg-gggg-gggggggggggg' WHERE id = ?")
        .bind(&corrupt.id)
        .execute(database.pool())
        .await
        .unwrap();
    let error = json(
        app.clone()
            .oneshot(request("/api/v1/auth/sessions", Some(&own.token)))
            .await
            .unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert_eq!(error["code"], "unexpected_error");
    assert!(!error.to_string().contains("gggggggg"));
    database.pool().close().await;
    let error = json(
        app.oneshot(request("/api/v1/auth/sessions", Some(&own.token)))
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "storage_unavailable");
}

#[tokio::test]
async fn selected_session_revocation_scopes_ownership_and_clears_only_the_current_cookie() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let stranger = user(&database, "owner").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let other = sessions.issue(&actor, Utc::now()).await.unwrap();
    let foreign = sessions.issue(&stranger, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &own.token).await;
    for id in [&foreign.id, &Uuid::now_v7().to_string()] {
        let response = app
            .clone()
            .oneshot(revoke_request(
                &format!("/api/v1/auth/sessions/{id}"),
                &own.token,
                &csrf,
            ))
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(
            json(response, StatusCode::NOT_FOUND).await["code"],
            "not_found"
        );
    }
    let uri = format!("/api/v1/auth/sessions/{}", other.id);
    let response = app
        .clone()
        .oneshot(revoke_request(&uri, &own.token, &csrf))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert!(
        sessions
            .validate(&other.token, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&own.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions
            .validate(&foreign.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    json(
        app.clone()
            .oneshot(revoke_request(&uri, &own.token, &csrf))
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    let response = app
        .oneshot(revoke_request(
            &format!("/api/v1/auth/sessions/{}", own.id),
            &own.token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.session.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audits, 2);
}

#[tokio::test]
async fn session_revocation_routes_reject_hostile_queries_csrf_ids_and_methods() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let other = sessions.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &own.token).await;
    let uri = format!("/api/v1/auth/sessions/{}", other.id);
    for path in ["/api/v1/auth/sessions", uri.as_str()] {
        let mut req = revoke_request(path, &own.token, &csrf);
        req.headers_mut().remove("x-csrf-token");
        json(
            app.clone().oneshot(req).await.unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        let mut req = revoke_request(path, &own.token, &csrf);
        req.headers_mut().remove(header::ORIGIN);
        json(
            app.clone().oneshot(req).await.unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        json(
            app.clone()
                .oneshot(revoke_request(
                    &format!("{path}?user_id={actor}"),
                    &own.token,
                    &csrf,
                ))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
        let methods = if path == "/api/v1/auth/sessions" {
            vec![Method::POST, Method::PATCH, Method::OPTIONS]
        } else {
            vec![Method::GET, Method::HEAD, Method::POST]
        };
        for method in methods {
            let mut req = request(path, Some(&own.token));
            *req.method_mut() = method.clone();
            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(
                response.headers()[header::ALLOW],
                if path == "/api/v1/auth/sessions" {
                    "GET, HEAD, DELETE"
                } else {
                    "DELETE"
                }
            );
        }
    }
    for id in ["bad", "%FF", "00000000000000000000000000000000"] {
        let error = json(
            app.clone()
                .oneshot(revoke_request(
                    &format!("/api/v1/auth/sessions/{id}"),
                    &own.token,
                    &csrf,
                ))
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert!(error["field_errors"]["session_id"].is_string());
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
}

#[tokio::test]
async fn individual_and_global_session_revocation_roll_back_with_their_audits() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let other = sessions.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &own.token).await;
    sqlx::query("CREATE TRIGGER reject_session_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT, 'test audit unavailable'); END")
        .execute(database.pool()).await.unwrap();
    for uri in [
        format!("/api/v1/auth/sessions/{}", other.id),
        String::from("/api/v1/auth/sessions"),
    ] {
        let response = app
            .clone()
            .oneshot(revoke_request(&uri, &own.token, &csrf))
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(
            json(response, StatusCode::SERVICE_UNAVAILABLE).await["code"],
            "storage_unavailable"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
    sqlx::query("DROP TRIGGER reject_session_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let response = app
        .oneshot(revoke_request("/api/v1/auth/sessions", &own.token, &csrf))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key(header::SET_COOKIE));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn concurrent_global_logout_has_one_winner_and_preserves_other_users() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let stranger = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let other = sessions.issue(&actor, Utc::now()).await.unwrap();
    let foreign = sessions.issue(&stranger, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &own.token).await;
    let (a, b) = tokio::join!(
        app.clone()
            .oneshot(revoke_request("/api/v1/auth/sessions", &own.token, &csrf)),
        app.clone()
            .oneshot(revoke_request("/api/v1/auth/sessions", &own.token, &csrf)),
    );
    let mut statuses = vec![a.unwrap().status(), b.unwrap().status()];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED]
    );
    assert!(
        sessions
            .validate(&own.token, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&other.token, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&foreign.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.sessions.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn concurrent_selected_revocation_audits_once_and_default_router_keeps_routes_closed() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let target = sessions.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &own.token).await;
    let uri = format!("/api/v1/auth/sessions/{}", target.id);
    for path in [uri.as_str(), "/api/v1/auth/sessions"] {
        json(
            http::router(database.clone())
                .oneshot(revoke_request(path, &own.token, &csrf))
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let (a, b) = tokio::join!(
        app.clone().oneshot(revoke_request(&uri, &own.token, &csrf)),
        app.clone().oneshot(revoke_request(&uri, &own.token, &csrf)),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert!(!a.headers().contains_key(header::SET_COOKIE));
    assert!(!b.headers().contains_key(header::SET_COOKIE));
    let mut statuses = vec![a.status(), b.status()];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![StatusCode::NO_CONTENT, StatusCode::NOT_FOUND]
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.session.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1);
    assert!(
        sessions
            .validate(&own.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn session_bound_revocation_revalidates_accounts_and_deadlines_without_http_precheck() {
    use puxiri::auth::session::SessionRevocationOutcome;
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let now = Utc::now();
    let own = sessions.issue(&actor, now).await.unwrap();
    let target = sessions.issue(&actor, now).await.unwrap();
    for instant in [
        now - TimeDelta::seconds(1),
        now + TimeDelta::hours(24),
        now + TimeDelta::days(7),
    ] {
        assert!(matches!(
            sessions
                .revoke_owned(&own.token, &target.id, instant)
                .await
                .unwrap(),
            SessionRevocationOutcome::Unauthenticated
        ));
        assert!(
            !sessions
                .revoke_all_owned(&own.token, instant)
                .await
                .unwrap()
        );
    }
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&actor)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        sessions
            .revoke_owned(&own.token, &target.id, now)
            .await
            .unwrap(),
        SessionRevocationOutcome::Unauthenticated
    ));
    assert!(!sessions.revoke_all_owned(&own.token, now).await.unwrap());
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn logout_requires_same_origin_and_the_exact_bearers_csrf_without_side_effects() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, Utc::now()).await.unwrap();
    let other = sessions.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let own_csrf = csrf(&app, &issued.token).await;
    let other_csrf = csrf(&app, &other.token).await;
    let before: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&issued.id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    for scenario in [
        "missing_origin",
        "foreign_origin",
        "missing_csrf",
        "other_session_csrf",
        "duplicate_csrf",
    ] {
        let mut req = logout_request(&issued.token, &own_csrf);
        match scenario {
            "missing_origin" => {
                req.headers_mut().remove(header::ORIGIN);
            }
            "foreign_origin" => {
                req.headers_mut().insert(
                    header::ORIGIN,
                    HeaderValue::from_static("https://attacker.example.test"),
                );
            }
            "missing_csrf" => {
                req.headers_mut().remove("x-csrf-token");
            }
            "other_session_csrf" => {
                req.headers_mut()
                    .insert("x-csrf-token", HeaderValue::from_str(&other_csrf).unwrap());
            }
            _ => {
                req.headers_mut()
                    .append("x-csrf-token", HeaderValue::from_str(&own_csrf).unwrap());
            }
        }
        let response = app.clone().oneshot(req).await.unwrap();
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        json(response, StatusCode::FORBIDDEN).await;
    }
    let after: (String, Option<String>) =
        sqlx::query_as("SELECT last_activity_at, revoked_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(after, (before, None));
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
}

#[tokio::test]
async fn logout_clears_cookie_after_commit_and_revokes_only_its_own_exact_session() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let stranger = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, Utc::now()).await.unwrap();
    let same_user_other_session = sessions.issue(&actor, Utc::now()).await.unwrap();
    let stranger_session = sessions.issue(&stranger, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &issued.token).await;
    let mut overridden = logout_request(&issued.token, &csrf);
    *overridden.uri_mut() = format!("/api/v1/auth/session?session_id={}", stranger_session.id)
        .parse()
        .unwrap();
    json(
        app.clone().oneshot(overridden).await.unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let response = app
        .clone()
        .oneshot(logout_request(&issued.token, &csrf))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[header::SET_COOKIE],
        "__Host-puxiri_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax"
    );
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    assert!(
        sessions
            .validate(&issued.token, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&same_user_other_session.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions
            .validate(&stranger_session.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    let audit: (String, String) = sqlx::query_as("SELECT actor_user_id, target_id FROM audit_events WHERE event_kind = 'auth.session.revoked'")
        .fetch_one(database.pool()).await.unwrap();
    assert_eq!(audit, (actor, issued.id));
    json(
        app.oneshot(logout_request(&issued.token, &csrf))
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

#[tokio::test]
async fn logout_audit_failure_preserves_session_and_cookie_then_concurrent_retry_revokes_once() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, Utc::now()).await.unwrap();
    let app = app(&directory, database.clone());
    let csrf = csrf(&app, &issued.token).await;
    sqlx::query("CREATE TRIGGER reject_logout_audit BEFORE INSERT ON audit_events WHEN NEW.event_kind = 'auth.session.revoked' BEGIN SELECT RAISE(ABORT, 'forced audit failure'); END")
        .execute(database.pool()).await.unwrap();
    let response = app
        .clone()
        .oneshot(logout_request(&issued.token, &csrf))
        .await
        .unwrap();
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    let error = json(response, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(error["retryable"], true);
    assert!(
        sessions
            .validate(&issued.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
    sqlx::query("DROP TRIGGER reject_logout_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        app.clone().oneshot(logout_request(&issued.token, &csrf)),
        app.clone().oneshot(logout_request(&issued.token, &csrf)),
    );
    let statuses = [first.unwrap().status(), second.unwrap().status()];
    assert!(statuses.contains(&StatusCode::NO_CONTENT));
    assert!(statuses.contains(&StatusCode::UNAUTHORIZED));
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.session.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn token_based_revocation_revalidates_account_and_deadline_before_writing() {
    let directory = TempDir::new().unwrap();
    let database = database(&directory).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let now = Utc::now();
    let issued = sessions.issue(&actor, now).await.unwrap();
    assert!(
        !sessions
            .revoke_current(&issued.token, now - TimeDelta::nanoseconds(1))
            .await
            .unwrap()
    );
    assert!(
        !sessions
            .revoke_current(&issued.token, now + TimeDelta::hours(24))
            .await
            .unwrap()
    );
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&actor)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(!sessions.revoke_current(&issued.token, now).await.unwrap());
    let revoked: Option<String> =
        sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(revoked.is_none());
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
}
