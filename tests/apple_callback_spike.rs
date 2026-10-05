//! Dev-only callback correlation, NOT Apple token validation or product login.
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{rand::rand_bytes, sha::sha256};
use puxiri::db::Database;
use serde::Deserialize;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use tempfile::TempDir;

const COOKIE: &str = "__Secure-puxiri_apple";
const PATH: &str = "/callbacks/apple";
const NOW: i64 = 1_800_000_000;
struct Endpoint {
    db: Arc<Database>,
    now: AtomicI64,
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}
struct Attempt {
    state: String,
    cookie: String,
    set_cookie: String,
}
fn token() -> String {
    let mut bytes = [0; 32];
    rand_bytes(&mut bytes).unwrap();
    URL_SAFE_NO_PAD.encode(bytes)
}
async fn issue(endpoint: &Endpoint) -> Attempt {
    let state = token();
    let cookie = token();
    let now = endpoint.now.load(Ordering::SeqCst);
    sqlx::query("INSERT INTO apple_attempts(state_hash,cookie_hash,config,created,expires) VALUES(?,?,'apple-v1',?,?)")
        .bind(sha256(state.as_bytes()).to_vec()).bind(sha256(cookie.as_bytes()).to_vec()).bind(now).bind(now+300).execute(endpoint.db.pool()).await.unwrap();
    Attempt {
        state,
        set_cookie: format!(
            "{COOKIE}={cookie}; Path={PATH}; Secure; HttpOnly; SameSite=None; Max-Age=300"
        ),
        cookie,
    }
}
async fn callback(
    State(endpoint): State<Arc<Endpoint>>,
    headers: HeaderMap,
    form: Result<Form<Callback>, axum::extract::rejection::FormRejection>,
) -> StatusCode {
    let Ok(Form(form)) = form else {
        return StatusCode::BAD_REQUEST;
    };
    let valid_token = |value: &str| {
        value.len() == 43
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !valid_token(&form.state) {
        return StatusCode::BAD_REQUEST;
    }
    let outcome = match (&form.code, &form.error) {
        (Some(code), None) if !code.is_empty() && code.len() <= 2048 => "code_received",
        (None, Some(error)) if error == "user_cancelled_authorize" => "cancelled",
        _ => return StatusCode::BAD_REQUEST,
    };
    // Exactly one callback cookie across all Cookie headers. Never fall back to
    // state alone or the site's session cookie on cross-site form POST.
    let mut cookies = Vec::new();
    for header in headers.get_all("cookie") {
        let Ok(header) = header.to_str() else {
            return StatusCode::UNAUTHORIZED;
        };
        for pair in header.split(';') {
            if let Some((name, value)) = pair.trim().split_once('=')
                && name == COOKIE
            {
                cookies.push(value);
            }
        }
    }
    if cookies.len() != 1 || !valid_token(cookies[0]) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(mut tx) = endpoint.db.pool().begin_with("BEGIN IMMEDIATE").await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let now = endpoint.now.load(Ordering::SeqCst);
    let result=sqlx::query("UPDATE apple_attempts SET outcome=? WHERE state_hash=? AND cookie_hash=? AND config='apple-v1' AND outcome IS NULL AND created<=? AND expires>?")
        .bind(outcome).bind(sha256(form.state.as_bytes()).to_vec()).bind(sha256(cookies[0].as_bytes()).to_vec()).bind(now).bind(now).execute(&mut *tx).await;
    let Ok(result) = result else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if result.rows_affected() != 1 {
        return StatusCode::UNAUTHORIZED;
    }
    if tx.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    // The code is discarded. No exchange, identity, login session or redirect
    // is authorized by successful state correlation in this fixture.
    StatusCode::ACCEPTED
}
struct Fixture {
    _directory: TempDir,
    endpoint: Arc<Endpoint>,
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::raw_sql("CREATE TABLE apple_attempts(state_hash BLOB PRIMARY KEY,cookie_hash BLOB NOT NULL,config TEXT NOT NULL,created INTEGER NOT NULL,expires INTEGER NOT NULL,outcome TEXT CHECK(outcome IN ('code_received','cancelled')));
            CREATE TABLE fixture_sessions(id TEXT PRIMARY KEY);")
            .execute(db.pool()).await.unwrap();
        let endpoint = Arc::new(Endpoint {
            db,
            now: AtomicI64::new(NOW),
        });
        let app = Router::new()
            .route(PATH, post(callback))
            .layer(DefaultBodyLimit::max(8192))
            .with_state(endpoint.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _directory: directory,
            endpoint,
            base,
            task,
        }
    }
    async fn post(&self, body: String, cookie: Option<String>) -> u16 {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let mut request = client
            .post(format!("{}{PATH}", self.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body);
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        assert!(response.headers().get("set-cookie").is_none());
        assert!(response.headers().get("location").is_none());
        assert!(response.text().await.unwrap().is_empty());
        status
    }
    async fn used(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM apple_attempts WHERE outcome IS NOT NULL")
            .fetch_one(self.endpoint.db.pool())
            .await
            .unwrap()
    }
}
fn body(attempt: &Attempt) -> String {
    format!("state={}&code=artificial-code", attempt.state)
}
fn cookie(attempt: &Attempt) -> String {
    format!("{COOKIE}={}", attempt.cookie)
}

#[tokio::test]
async fn callback_cookie_is_scoped_and_state_is_single_use_under_concurrent_posts_without_login() {
    let mut fixture = Fixture::new().await;
    let attempt = issue(&fixture.endpoint).await;
    assert!(
        attempt
            .set_cookie
            .contains("Path=/callbacks/apple; Secure; HttpOnly; SameSite=None; Max-Age=300")
    );
    assert!(!attempt.set_cookie.contains("Domain="));
    let (a, b) = tokio::join!(
        fixture.post(body(&attempt), Some(cookie(&attempt))),
        fixture.post(body(&attempt), Some(cookie(&attempt)))
    );
    assert!(matches!((a, b), (202, 401) | (401, 202)));
    assert_eq!(fixture.used().await, 1);
    assert_eq!(
        fixture.post(body(&attempt), Some(cookie(&attempt))).await,
        401
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fixture_sessions")
            .fetch_one(fixture.endpoint.db.pool())
            .await
            .unwrap(),
        0
    );
    let stored: (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT state_hash,cookie_hash FROM apple_attempts")
            .fetch_one(fixture.endpoint.db.pool())
            .await
            .unwrap();
    assert_eq!(stored.0.len(), 32);
    assert_eq!(stored.1.len(), 32);
    assert_ne!(stored.0, attempt.state.as_bytes());
    fixture.endpoint.db.close().await;
    fixture.task.abort();
    let _ = (&mut fixture.task).await;
    let directory = std::mem::replace(&mut fixture._directory, TempDir::new().unwrap());
    drop(fixture);
    let reopened = Database::open(directory.path()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM apple_attempts WHERE outcome='code_received'"
        )
        .fetch_one(reopened.pool())
        .await
        .unwrap(),
        1
    );
    reopened.close().await;
}

#[tokio::test]
async fn missing_wrong_duplicated_cookie_or_crossed_attempt_never_consumes_state() {
    let fixture = Fixture::new().await;
    let a = issue(&fixture.endpoint).await;
    let b = issue(&fixture.endpoint).await;
    for value in [
        None,
        Some("session=normal-lax-site-cookie".into()),
        Some(cookie(&b)),
        Some(format!("{}; {}", cookie(&a), cookie(&a))),
    ] {
        assert_eq!(fixture.post(body(&a), value).await, 401);
    }
    assert_eq!(fixture.used().await, 0);
    assert_eq!(fixture.post(body(&a), Some(cookie(&a))).await, 202);
    fixture.endpoint.db.close().await;
}

#[tokio::test]
async fn expiry_future_creation_config_change_and_bad_form_fail_closed() {
    let fixture = Fixture::new().await;
    let attempt = issue(&fixture.endpoint).await;
    for now in [NOW - 1, NOW + 300] {
        fixture.endpoint.now.store(now, Ordering::SeqCst);
        assert_eq!(
            fixture.post(body(&attempt), Some(cookie(&attempt))).await,
            401
        );
    }
    fixture.endpoint.now.store(NOW, Ordering::SeqCst);
    for extra in [
        "&state=duplicate",
        "&code=duplicate",
        "&error=user_cancelled_authorize",
    ] {
        assert_eq!(
            fixture
                .post(format!("{}{extra}", body(&attempt)), Some(cookie(&attempt)))
                .await,
            400
        );
    }
    assert_eq!(
        fixture
            .post(format!("state={}", attempt.state), Some(cookie(&attempt)))
            .await,
        400
    );
    assert_eq!(
        fixture.post("x".repeat(8193), Some(cookie(&attempt))).await,
        400
    );
    sqlx::query("UPDATE apple_attempts SET config='inactive-revision'")
        .execute(fixture.endpoint.db.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture.post(body(&attempt), Some(cookie(&attempt))).await,
        401
    );
    assert_eq!(fixture.used().await, 0);
    fixture.endpoint.db.close().await;
}

#[tokio::test]
async fn cancellation_is_correlated_once_and_database_failure_does_not_consume_retry() {
    let fixture = Fixture::new().await;
    let attempt = issue(&fixture.endpoint).await;
    sqlx::raw_sql("CREATE TRIGGER fail_claim BEFORE UPDATE ON apple_attempts BEGIN SELECT RAISE(ABORT,'fixture claim failure'); END")
        .execute(fixture.endpoint.db.pool()).await.unwrap();
    assert_eq!(
        fixture.post(body(&attempt), Some(cookie(&attempt))).await,
        503
    );
    assert_eq!(fixture.used().await, 0);
    sqlx::query("DROP TRIGGER fail_claim")
        .execute(fixture.endpoint.db.pool())
        .await
        .unwrap();
    let cancelled = format!("state={}&error=user_cancelled_authorize", attempt.state);
    assert_eq!(
        fixture
            .post(cancelled.clone(), Some(cookie(&attempt)))
            .await,
        202
    );
    assert_eq!(fixture.post(cancelled, Some(cookie(&attempt))).await, 401);
    assert_eq!(fixture.used().await, 1);
    fixture.endpoint.db.close().await;
}
