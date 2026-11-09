use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use chrono::Utc;
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    projects::ProjectStore,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(chrono::TimeDelta::minutes(5)).unwrap()
}

async fn person(database: &Database, name: &str, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', 'now', 'now')",
    )
    .bind(&id)
    .bind(name)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, 'now')",
    )
    .bind(&id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
    id
}

fn browser(
    security: &BrowserSessionSecurity,
    token: &puxiri::auth::SessionToken,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "tracker.example.test")
        .header(header::ORIGIN, "https://tracker.example.test")
        .header(
            header::COOKIE,
            format!("__Host-puxiri_session={}", token.expose()),
        )
        .header("x-csrf-token", security.csrf_token(token).expose());
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder
        .body(Body::from(body.unwrap_or("").to_owned()))
        .unwrap()
}

async fn wait_for(address: std::net::SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("listener did not accept");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn muira_token_identifies_and_puxiri_grant_authorizes_each_tool() {
    let subjects = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = subjects.clone();
    let userinfo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let userinfo_address = userinfo.local_addr().unwrap();
    tokio::spawn(async move {
        let router = Router::new().route(
            "/oauth/userinfo",
            get(|headers: axum::http::HeaderMap| async move {
                let token = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .unwrap_or("");
                recorded.lock().unwrap().push(token.to_owned());
                let subject = match token {
                    "owner-access-token" => "owner-subject",
                    "viewer-access-token" => "viewer-subject",
                    "stranger-access-token" => "stranger-subject",
                    _ => {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid_token"})),
                        )
                            .into_response();
                    }
                };
                (StatusCode::OK, Json(json!({"sub": subject}))).into_response()
            }),
        );
        axum::serve(userinfo, router).await.unwrap();
    });
    wait_for(userinfo_address).await;
    let issuer = format!("http://{userinfo_address}");

    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'MCP', 'pt-BR', 'America/Sao_Paulo', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let owner = person(&database, "Owner", "owner").await;
    let viewer = person(&database, "Viewer", "member").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "MCP", "Trabalho", None, Utc::now())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, 'viewer', 'now')",
    )
    .bind(&project.id)
    .bind(&viewer)
    .execute(database.pool())
    .await
    .unwrap();
    for (user, subject) in [(&owner, "owner-subject"), (&viewer, "viewer-subject")] {
        sqlx::query(
            "INSERT INTO auth_identities (id, user_id, provider, issuer, subject, created_at) \
             VALUES (?, ?, 'muira', ?, ?, 'now')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(user)
        .bind(&issuer)
        .bind(subject)
        .execute(database.pool())
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO installation_login \
         (singleton, source, muira_issuer, muira_client_id, muira_client_secret_envelope) \
         VALUES (1, 'muira', ?, 'puxiri', X'01')",
    )
    .bind(&issuer)
    .execute(database.pool())
    .await
    .unwrap();

    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let owner_session = sessions.issue(&owner, Utc::now()).await.unwrap();
    let viewer_session = sessions.issue(&viewer, Utc::now()).await.unwrap();
    let app = http::router_with_browser_sessions(database.clone(), security.clone(), policy());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    wait_for(app_address).await;
    let mcp_url = format!("http://{app_address}/mcp");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .unwrap();

    let metadata: Value = client
        .get(format!(
            "http://{app_address}/.well-known/oauth-protected-resource/mcp"
        ))
        .header(header::HOST, "attacker.example.invalid")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["resource"], "https://tracker.example.test/mcp");
    assert_eq!(metadata["authorization_servers"], json!([issuer]));
    assert_eq!(
        metadata["scopes_supported"],
        json!(["mcp:read", "mcp:write"])
    );

    let unauthenticated = client
        .post(&mcp_url)
        .header("accept", "application/json, text/event-stream")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert!(
        unauthenticated.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("resource_metadata=\"https://tracker.example.test/.well-known/oauth-protected-resource/mcp\"")
    );

    let needs_grant = client
        .post(&mcp_url)
        .header(header::AUTHORIZATION, "Bearer owner-access-token")
        .header("accept", "application/json, text/event-stream")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(needs_grant.status(), StatusCode::FORBIDDEN);
    assert!(
        needs_grant.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("insufficient_scope")
    );

    let stranger = client
        .post(&mcp_url)
        .header(header::AUTHORIZATION, "Bearer stranger-access-token")
        .header("accept", "application/json, text/event-stream")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(stranger.status(), StatusCode::FORBIDDEN);
    assert!(
        stranger.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("access_denied")
    );
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(accounts, 2);

    let (grant_status, grant_body) = json_response(
        app_oneshot(
            &database,
            &security,
            policy(),
            browser(
                &security,
                &owner_session.token,
                "PUT",
                "/api/v1/mcp/grant",
                Some(r#"{"scope":"write","all_projects":true,"projects":[]}"#),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(grant_status, StatusCode::OK);
    assert_eq!(grant_body["configured"], true);
    assert_eq!(grant_body["scope"], "write");
    assert!(grant_body.get("token").is_none());

    let (viewer_grant_status, _) = json_response(
        app_oneshot(
            &database,
            &security,
            policy(),
            browser(
                &security,
                &viewer_session.token,
                "PUT",
                "/api/v1/mcp/grant",
                Some(&format!(
                    r#"{{"scope":"read","all_projects":false,"projects":["{}"]}}"#,
                    project.id
                )),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(viewer_grant_status, StatusCode::OK);

    let listed = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "list_projects",
        json!({}),
    )
    .await;
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["projects"][0]["id"], project.id);

    let created = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "create_issue",
        json!({"project_id": project.id, "title": "Pelo MCP"}),
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    assert_eq!(created["issue_number"], 1);
    assert_eq!(created["title"], "Pelo MCP");

    let denied = call_tool_raw(
        &client,
        &mcp_url,
        "viewer-access-token",
        "create_issue",
        json!({"project_id": project.id, "title": "Não"}),
    )
    .await;
    let denial = denied["error"]["message"].as_str().unwrap_or("");
    assert!(
        denial.contains("grant") || denial.contains("role"),
        "{denied}"
    );

    let read = call_tool(
        &client,
        &mcp_url,
        "viewer-access-token",
        "get_issue",
        json!({"project_id": project.id, "issue_number": 1}),
    )
    .await;
    let read: Value = serde_json::from_str(&read).unwrap();
    assert_eq!(read["title"], "Pelo MCP");

    let schema = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "get_project_schema",
        json!({"project_id": project.id}),
    )
    .await;
    let schema: Value = serde_json::from_str(&schema).unwrap();
    let statuses = schema["statuses"].as_array().expect("statuses");
    assert!(statuses.len() >= 2, "{schema}");
    let current_status = created["status_id"].as_str().unwrap();
    let next_status = statuses
        .iter()
        .find_map(|status| {
            let id = status["id"].as_str()?;
            let visible = status["board_visible"].as_bool()?;
            if id != current_status && visible {
                Some(id.to_owned())
            } else {
                None
            }
        })
        .expect("another visible status");

    let found = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "search_issues",
        json!({"project_id": project.id, "query": "Pelo"}),
    )
    .await;
    let found: Value = serde_json::from_str(&found).unwrap();
    assert_eq!(found["issues"][0]["issue_number"], 1);

    let updated = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "update_issue",
        json!({
            "project_id": project.id,
            "issue_number": 1,
            "expected_version": created["version"],
            "title": "Pelo MCP revisado",
            "idempotency_key": Uuid::now_v7().to_string(),
        }),
    )
    .await;
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(updated["changed"], true);

    let moved = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "move_issue",
        json!({
            "project_id": project.id,
            "issue_number": 1,
            "expected_version": updated["version"],
            "status_id": next_status,
            "idempotency_key": Uuid::now_v7().to_string(),
        }),
    )
    .await;
    let moved: Value = serde_json::from_str(&moved).unwrap();
    assert_eq!(moved["status_id"], next_status);

    let comment_key = Uuid::now_v7().to_string();
    let comment = call_tool(
        &client,
        &mcp_url,
        "owner-access-token",
        "add_comment",
        json!({
            "project_id": project.id,
            "issue_number": 1,
            "body": "nota",
            "idempotency_key": comment_key,
        }),
    )
    .await;
    let comment: Value = serde_json::from_str(&comment).unwrap();
    assert!(!comment["comment_id"].as_str().unwrap().is_empty());
    assert!(!comment.to_string().contains("nota") || comment.get("body").is_none());

    let database_bytes = std::fs::read(directory.path().join("puxiri.sqlite3")).unwrap();
    assert!(!contains_bytes(&database_bytes, b"owner-access-token"));
    assert!(!contains_bytes(&database_bytes, b"viewer-access-token"));
    assert!(!contains_bytes(&database_bytes, b"stranger-access-token"));
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "puxiri-test", "version": "0"}
        }
    })
}

async fn call_tool(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> String {
    let response = call_tool_raw(client, url, token, name, arguments).await;
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} response: {response}"))
        .to_owned()
}

async fn call_tool_raw(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let opened = client
        .post(url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status(), StatusCode::OK, "{token}");
    let session = opened
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let opened_body = opened.text().await.unwrap();
    assert!(
        opened_body.contains("\"result\""),
        "initialize body: {opened_body}"
    );
    let mut request = client
        .post(url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn app_oneshot(
    database: &Arc<Database>,
    security: &BrowserSessionSecurity,
    policy: SessionPolicy,
    request: Request<Body>,
) -> axum::response::Response {
    let app = http::router_with_browser_sessions(database.clone(), security.clone(), policy);
    app.oneshot(request).await.unwrap()
}

async fn json_response(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
