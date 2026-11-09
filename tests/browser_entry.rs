use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use chrono::{TimeDelta, Utc};
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        SessionToken,
        entry::{
            MuiraCodeExchange, MuiraExchangeFuture, MuiraExchangeRequest, VerifiedMuiraIdentity,
        },
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
        setup::SetupTokenStore,
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct Seen {
    client_secret: String,
    verifier_len: usize,
    nonce_len: usize,
    redirect_uri: String,
    code: String,
    issuer: String,
}

struct ScriptedExchange {
    identity: Mutex<VerifiedMuiraIdentity>,
    calls: Mutex<Vec<Seen>>,
}

impl ScriptedExchange {
    fn new(identity: VerifiedMuiraIdentity) -> Arc<Self> {
        Arc::new(Self {
            identity: Mutex::new(identity),
            calls: Mutex::new(Vec::new()),
        })
    }
}

impl MuiraCodeExchange for ScriptedExchange {
    fn exchange(&self, request: MuiraExchangeRequest) -> MuiraExchangeFuture {
        let identity = self.identity.lock().expect("identity").clone();
        self.calls.lock().expect("calls").push(Seen {
            client_secret: request.client_secret.to_string(),
            verifier_len: request.verifier.len(),
            nonce_len: request.nonce.len(),
            redirect_uri: request.redirect_uri,
            code: request.code,
            issuer: request.issuer,
        });
        Box::pin(async move { Ok(identity) })
    }
}

fn identity(subject: &str, email: &str) -> VerifiedMuiraIdentity {
    VerifiedMuiraIdentity {
        issuer: "https://muira.example.test".to_owned(),
        subject: subject.to_owned(),
        email: Some(email.to_owned()),
        email_verified: true,
    }
}

async fn open(exchange: Arc<ScriptedExchange>) -> (TempDir, Arc<Database>, axum::Router) {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let router = http::router_with_entry(
        database.clone(),
        security,
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
        exchange,
    );
    (directory, database, router)
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::HOST, "tracker.example.test")
        .header(header::ORIGIN, "https://tracker.example.test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

fn cookie_value(cookies: &[String], name: &str) -> Option<String> {
    for cookie in cookies {
        let Some(rest) = cookie.strip_prefix(&format!("{name}=")) else {
            continue;
        };
        let value = rest.split(';').next().unwrap_or("");
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

fn query_value(url: &str, name: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix(&format!("{name}=")) {
            return value.to_owned();
        }
    }
    panic!("missing {name}");
}

async fn body(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn setup_body(token: &str, email: &str) -> Value {
    json!({
        "setup_token": token,
        "organization_name": "Puxiri",
        "locale": "pt-BR",
        "timezone": "America/Sao_Paulo",
        "display_name": "Ada",
        "email": email,
        "issuer": "https://muira.example.test",
        "client_id": "puxiri",
        "client_secret": "muira-test-secret"
    })
}

#[tokio::test]
async fn muira_callback_issues_a_session_only_for_a_member_and_decrypts_the_client_secret() {
    let exchange = ScriptedExchange::new(identity("owner-subject", "ada@example.test"));
    let (directory, database, app) = open(exchange.clone()).await;
    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup/status")
                .header(header::HOST, "tracker.example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body(status).await, json!({"setup_required": true}));

    let token = SetupTokenStore::new(database.pool().clone())
        .issue(Utc::now())
        .await
        .unwrap();
    let started = app
        .clone()
        .oneshot(post_json(
            "/api/v1/setup/muira",
            setup_body(token.expose(), "ada@example.test"),
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let started_cookies = cookies(&started);
    let started_body = body(started).await;
    let authorization_url = started_body["authorization_url"].as_str().unwrap();
    let state = query_value(authorization_url, "state");
    assert_eq!(
        cookie_value(&started_cookies, "__Host-puxiri_muira").as_deref(),
        Some(state.as_str())
    );
    assert!(!authorization_url.contains("muira-test-secret"));
    assert!(
        started_cookies
            .iter()
            .all(|cookie| !cookie.contains("__Host-puxiri_session="))
    );

    let callback = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/auth/muira/callback?code=one-time-code&state={state}"
                ))
                .header(header::HOST, "tracker.example.test")
                .header("sec-fetch-site", "cross-site")
                .header(header::COOKIE, format!("__Host-puxiri_muira={state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        callback.headers()[header::LOCATION],
        "https://tracker.example.test/projects"
    );
    let session = cookie_value(&cookies(&callback), "__Host-puxiri_session").unwrap();
    assert_eq!(session.len(), 43);
    let seen = exchange.calls.lock().expect("calls").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].client_secret, "muira-test-secret");
    assert!(seen[0].verifier_len > 0);
    assert!(seen[0].nonce_len > 0);
    assert_eq!(seen[0].code, "one-time-code");
    assert_eq!(seen[0].issuer, "https://muira.example.test");
    assert_eq!(
        seen[0].redirect_uri,
        "https://tracker.example.test/api/v1/auth/muira/callback"
    );

    *exchange.identity.lock().expect("identity") = identity("stranger", "stranger@example.test");
    let login = app
        .clone()
        .oneshot(post_json("/api/v1/auth/muira/login", json!({})))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let login_body = body(login).await;
    let login_state = query_value(login_body["authorization_url"].as_str().unwrap(), "state");
    let refused = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/auth/muira/callback?code=another-code&state={login_state}"
                ))
                .header(header::HOST, "tracker.example.test")
                .header("sec-fetch-site", "cross-site")
                .header(header::COOKIE, format!("__Host-puxiri_muira={login_state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        refused.headers()[header::LOCATION],
        "https://tracker.example.test/convite"
    );
    assert!(cookie_value(&cookies(&refused), "__Host-puxiri_session").is_none());
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(users, 1);

    let csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/session")
                .header(header::HOST, "tracker.example.test")
                .header(header::COOKIE, format!("__Host-puxiri_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let csrf = body(csrf).await["csrf_token"].as_str().unwrap().to_owned();
    let mut create = post_json(
        "/api/v1/projects",
        json!({"project_key": "ENT", "name": "Entrada"}),
    );
    create.headers_mut().insert(
        header::COOKIE,
        format!("__Host-puxiri_session={session}").parse().unwrap(),
    );
    create
        .headers_mut()
        .insert("x-csrf-token", csrf.parse().unwrap());
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = body(created).await;
    assert_eq!(created_body["project_key"], "ENT");
    assert_eq!(created_body["default_view"], "board");
    assert!(created_body.get("description").is_none());

    let member = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Member', 'active', 'now', 'now')")
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'member', 'now')")
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    let member_session = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    )
    .issue(&member, Utc::now())
    .await
    .unwrap();
    let member_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/session")
                .header(header::HOST, "tracker.example.test")
                .header(
                    header::COOKIE,
                    format!("__Host-puxiri_session={}", member_session.token.expose()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let member_csrf = body(member_csrf).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut denied = post_json(
        "/api/v1/projects",
        json!({"project_key": "NOPE", "name": "Denied"}),
    );
    denied.headers_mut().insert(
        header::COOKIE,
        format!("__Host-puxiri_session={}", member_session.token.expose())
            .parse()
            .unwrap(),
    );
    denied
        .headers_mut()
        .insert("x-csrf-token", member_csrf.parse().unwrap());
    let denied = app.clone().oneshot(denied).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    drop(directory);
}

#[tokio::test]
async fn muira_callback_rejects_head_a_mismatched_state_and_the_wrong_email_before_access() {
    let exchange = ScriptedExchange::new(identity("owner-subject", "other@example.test"));
    let (_directory, database, app) = open(exchange.clone()).await;
    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri("/api/v1/auth/muira/callback?code=unused&state=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .header(header::HOST, "tracker.example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(exchange.calls.lock().expect("calls").is_empty());

    let token = SetupTokenStore::new(database.pool().clone())
        .issue(Utc::now())
        .await
        .unwrap();
    let started = app
        .clone()
        .oneshot(post_json(
            "/api/v1/setup/muira",
            setup_body(token.expose(), "ada@example.test"),
        ))
        .await
        .unwrap();
    let started_body = body(started).await;
    let state = query_value(started_body["authorization_url"].as_str().unwrap(), "state");
    let other = SessionToken::generate().unwrap();
    let mismatched = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/auth/muira/callback?code=one-time-code&state={state}"
                ))
                .header(header::HOST, "tracker.example.test")
                .header("sec-fetch-site", "cross-site")
                .header(
                    header::COOKIE,
                    format!("__Host-puxiri_muira={}", other.expose()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mismatched.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        mismatched.headers()[header::LOCATION],
        "https://tracker.example.test/?erro=identidade"
    );
    assert!(exchange.calls.lock().expect("calls").is_empty());

    let rejected = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/auth/muira/callback?code=one-time-code&state={state}"
                ))
                .header(header::HOST, "tracker.example.test")
                .header("sec-fetch-site", "cross-site")
                .header(header::COOKIE, format!("__Host-puxiri_muira={state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        rejected.headers()[header::LOCATION],
        "https://tracker.example.test/?erro=identidade"
    );
    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup/status")
                .header(header::HOST, "tracker.example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body(status).await["setup_required"], true);
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[tokio::test]
async fn anonymous_entry_rejects_a_missing_origin_and_malformed_login_json() {
    let exchange = ScriptedExchange::new(identity("owner-subject", "ada@example.test"));
    let (_directory, _database, app) = open(exchange).await;
    let mut missing_origin = post_json("/api/v1/setup/muira", json!({}));
    missing_origin.headers_mut().remove(header::ORIGIN);
    let missing_origin = app.clone().oneshot(missing_origin).await.unwrap();
    assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);
    let malformed = app
        .clone()
        .oneshot(post_json("/api/v1/auth/puxiri/login", json!({})))
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(malformed).await["code"], "malformed_request");
    let duplicate = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/muira/callback?code=one&code=two&state=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .header(header::HOST, "tracker.example.test")
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(duplicate).await["code"], "malformed_request");
}
