use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
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
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(chrono::TimeDelta::minutes(5)).unwrap()
}

async fn owner(database: &Database) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Owner', 'active', 'now', 'now')",
    )
    .bind(&id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, 'owner', 'now')",
    )
    .bind(&id)
    .execute(database.pool())
    .await
    .unwrap();
    id
}

struct Capture {
    path: String,
    authorization: String,
    idempotency_key: String,
    body: String,
}

fn mutation(
    security: &BrowserSessionSecurity,
    token: &puxiri::auth::SessionToken,
    method: &str,
    path: &str,
    body: &str,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "tracker.example.test")
        .header(header::ORIGIN, "https://tracker.example.test")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::COOKIE,
            format!("__Host-puxiri_session={}", token.expose()),
        )
        .header("x-csrf-token", security.csrf_token(token).expose())
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn json_of(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, payload)
}

#[tokio::test]
async fn invitation_issues_before_delivery_and_sabia_never_receives_the_token() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Invites', 'pt-BR', 'America/Sao_Paulo', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let owner = owner(&database).await;
    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let session = sessions.issue(&owner, Utc::now()).await.unwrap();
    let app = http::router_with_browser_sessions(database.clone(), security.clone(), policy());

    let (guest_status, _) = json_of(
        app.clone()
            .oneshot(mutation(
                &security,
                &session.token,
                "POST",
                "/api/v1/invitations",
                r#"{"email":"guest@example.test","role":"guest","projects":[]}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(guest_status, StatusCode::UNPROCESSABLE_ENTITY);

    let (plain_status, plain) = json_of(
        app.clone()
            .oneshot(mutation(
                &security,
                &session.token,
                "POST",
                "/api/v1/invitations",
                r#"{"email":"member@example.test","role":"member","projects":[]}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(plain_status, StatusCode::CREATED);
    assert_eq!(plain["delivery"], "unconfigured");
    assert!(plain.get("provider_message_id").is_none());
    let plain_token = plain["token"].as_str().unwrap();
    assert_eq!(plain_token.len(), 43);
    assert_eq!(plain["invite_url"], "https://tracker.example.test/");
    assert!(!plain["invite_url"].as_str().unwrap().contains(plain_token));

    let (denied_status, _) = json_of(
        app.clone()
            .oneshot(mutation(
                &security,
                &session.token,
                "PUT",
                "/api/v1/mail/sender",
                r#"{"provider":"sabia","api_key":"sabia-test-key","template":"organization-invite","origin":"http://127.0.0.1:9"}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(denied_status, StatusCode::FORBIDDEN);

    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();

    let captured = Arc::new(Mutex::new(Vec::<Capture>::new()));
    let captured_state = captured.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let router = Router::new()
            .route(
                "/v1/messages",
                post(
                    |State(captured): State<Arc<Mutex<Vec<Capture>>>>,
                     headers: HeaderMap,
                     body: axum::body::Bytes| async move {
                        let text = String::from_utf8(body.to_vec()).unwrap_or_default();
                        captured.lock().unwrap().push(Capture {
                            path: "/v1/messages".to_owned(),
                            authorization: headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_owned(),
                            idempotency_key: headers
                                .get("idempotency-key")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_owned(),
                            body: text.clone(),
                        });
                        if text.contains("reject@example.test") {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "rejected"})))
                                .into_response();
                        }
                        (
                            StatusCode::ACCEPTED,
                            Json(json!({"delivery_id": "delivery-1", "status": "queued"})),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(captured_state);
        axum::serve(listener, router).await.unwrap();
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("sabia listener did not accept");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let origin = format!("http://{address}");
    let configure = format!(
        r#"{{"provider":"sabia","api_key":"sabia-test-key","template":"organization-invite","origin":"{origin}"}}"#
    );
    let (configured_status, configured) = json_of(
        app.clone()
            .oneshot(mutation(
                &security,
                &session.token,
                "PUT",
                "/api/v1/mail/sender",
                &configure,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(configured_status, StatusCode::OK);
    assert_eq!(configured["configured"], true);
    assert_eq!(configured["provider"], "sabia");
    assert!(configured.get("api_key").is_none());
    assert!(!configured.to_string().contains("sabia-test-key"));

    let (queued_status, queued) = json_of(
        app.clone()
            .oneshot(mutation(
                &security,
                &session.token,
                "POST",
                "/api/v1/invitations",
                r#"{"email":"queued@example.test","role":"member","projects":[]}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(queued_status, StatusCode::CREATED);
    assert_eq!(queued["delivery"], "queued");
    assert_eq!(queued["provider_message_id"], "delivery-1");
    let token = queued["token"].as_str().unwrap();
    assert_eq!(queued["invite_url"], "https://tracker.example.test/");
    assert!(!queued["invite_url"].as_str().unwrap().contains(token));
    {
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/v1/messages");
        assert_eq!(calls[0].authorization, "Bearer sabia-test-key");
        assert_eq!(calls[0].idempotency_key, queued["id"].as_str().unwrap());
        assert!(!calls[0].body.contains(token));
        assert!(!calls[0].body.contains("\"html\""));
        assert!(calls[0].body.contains("https://tracker.example.test/"));
        assert!(
            calls[0]
                .body
                .contains("\"template\":\"organization-invite\"")
        );
    }

    let (rejected_status, rejected) = json_of(
        app.oneshot(mutation(
            &security,
            &session.token,
            "POST",
            "/api/v1/invitations",
            r#"{"email":"reject@example.test","role":"member","projects":[]}"#,
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(rejected_status, StatusCode::CREATED);
    assert_eq!(rejected["delivery"], "rejected");
    assert_eq!(rejected["token"].as_str().unwrap().len(), 43);
    assert!(rejected.get("provider_message_id").is_none());
}
