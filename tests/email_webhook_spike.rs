//! Authenticated Resend-style ingress fixture, NOT production suppression/auth.
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use puxiri::db::Database;
use serde::Deserialize;
use standardwebhooks::Webhook;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

struct Endpoint {
    verifier: Webhook,
    database: Arc<Database>,
}
struct Fixture {
    _directory: TempDir,
    database: Arc<Database>,
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
        let database = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::query("CREATE TABLE webhook_inbox (endpoint TEXT NOT NULL, id TEXT NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(endpoint,id))")
            .execute(database.pool()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/callbacks/resend", post(receive))
            .layer(DefaultBodyLimit::max(65536))
            .with_state(Arc::new(Endpoint {
                verifier: Webhook::from_bytes(vec![42; 32]).unwrap(),
                database: database.clone(),
            }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _directory: directory,
            database,
            base,
            task,
        }
    }
    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM webhook_inbox")
            .fetch_one(self.database.pool())
            .await
            .unwrap()
    }
    async fn post(&self, body: Vec<u8>, headers: HeaderMap) -> u16 {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .post(format!("{}/callbacks/resend", self.base))
            .headers(headers)
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn signed(body: &[u8], id: &str, timestamp: i64, key: u8) -> HeaderMap {
    let verifier = Webhook::from_bytes(vec![key; 32]).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("svix-id", id.parse().unwrap());
    headers.insert("svix-timestamp", timestamp.to_string().parse().unwrap());
    headers.insert(
        "svix-signature",
        verifier.sign(id, timestamp, body).unwrap().parse().unwrap(),
    );
    headers
}
async fn receive(
    State(endpoint): State<Arc<Endpoint>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // Translate only unambiguous branded names; do not let aliases shadow them.
    let mut canonical = HeaderMap::new();
    for (svix, standard) in [
        ("svix-id", "webhook-id"),
        ("svix-timestamp", "webhook-timestamp"),
        ("svix-signature", "webhook-signature"),
    ] {
        if headers.get_all(svix).iter().count() != 1 || headers.contains_key(standard) {
            return StatusCode::UNAUTHORIZED;
        }
        canonical.insert(standard, headers[svix].clone());
    }
    // Guard extreme integers before library timestamp subtraction; the library
    // still enforces its own five-minute freshness window and cryptography.
    let timestamp = canonical["webhook-timestamp"]
        .to_str()
        .ok()
        .and_then(|v| v.parse::<i64>().ok());
    if !timestamp.is_some_and(|timestamp| timestamp >= 0 && timestamp <= now().saturating_add(601))
        || endpoint.verifier.verify(&body, &canonical).is_err()
    {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(id) = canonical["webhook-id"].to_str() else {
        return StatusCode::UNAUTHORIZED;
    };
    if id.is_empty() || id.len() > 256 {
        return StatusCode::BAD_REQUEST;
    }
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        email_id: String,
    }
    let Ok(event) = serde_json::from_slice::<Event>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if !event.kind.starts_with("email.") || event.data.email_id.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    // Durable authenticated inbox only. No address suppression, permission or
    // user mutation based on payload; binding to sent-message/config awaits F1/F3.
    let Ok(mut transaction) = endpoint.database.pool().begin_with("BEGIN IMMEDIATE").await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if sqlx::query("INSERT INTO webhook_inbox(endpoint,id,payload) VALUES('fixture-config-v1',?,?) ON CONFLICT DO NOTHING")
        .bind(id).bind(body.as_ref()).execute(&mut *transaction).await.is_err() {return StatusCode::SERVICE_UNAVAILABLE;}
    let stored: Result<Vec<u8>, _> = sqlx::query_scalar(
        "SELECT payload FROM webhook_inbox WHERE endpoint='fixture-config-v1' AND id=?",
    )
    .bind(id)
    .fetch_one(&mut *transaction)
    .await;
    let Ok(stored) = stored else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if stored != body.as_ref() {
        return StatusCode::CONFLICT;
    }
    if transaction.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::NO_CONTENT
}

const BODY:&[u8]=br#"{ "type": "email.complained", "data": { "email_id": "fixture-email-id", "to": ["recipient@example.invalid"] } }"#;

#[tokio::test]
async fn raw_body_authentication_precedes_durable_deduplication_under_concurrent_replay() {
    let fixture = Fixture::new().await;
    let headers = signed(BODY, "fixture-event", now(), 42);
    let (first, second) = tokio::join!(
        fixture.post(BODY.to_vec(), headers.clone()),
        fixture.post(BODY.to_vec(), headers.clone())
    );
    assert_eq!((first, second), (204, 204));
    assert_eq!(fixture.count().await, 1);
    let stored: Vec<u8> = sqlx::query_scalar("SELECT payload FROM webhook_inbox")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(stored, BODY);
    // Even an existing dedup id must authenticate again, before touching DB.
    assert_eq!(
        fixture
            .post(BODY.to_vec(), signed(BODY, "fixture-event", now(), 99))
            .await,
        401
    );
    let reserialized =
        serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(BODY).unwrap()).unwrap();
    assert_eq!(fixture.post(reserialized.clone(), headers).await, 401);
    // Legitimately signed id reuse with changed bytes is not silently accepted.
    assert_eq!(
        fixture
            .post(
                reserialized.clone(),
                signed(&reserialized, "fixture-event", now(), 42)
            )
            .await,
        409
    );
    assert_eq!(fixture.count().await, 1);
    fixture.database.close().await;
}

#[tokio::test]
async fn missing_duplicate_stale_future_or_extreme_headers_and_bad_schema_have_no_effect() {
    let fixture = Fixture::new().await;
    let mut duplicate = signed(BODY, "event", now(), 42);
    duplicate.append("svix-id", "shadow-id".parse().unwrap());
    let mut aliases = signed(BODY, "event", now(), 42);
    aliases.insert("webhook-id", "shadow-id".parse().unwrap());
    for headers in [
        HeaderMap::new(),
        duplicate,
        aliases,
        signed(BODY, "event", now() - 600, 42),
        signed(BODY, "event", now() + 600, 42),
    ] {
        assert_eq!(fixture.post(BODY.to_vec(), headers).await, 401);
    }
    for timestamp in [i64::MIN, i64::MAX] {
        assert_eq!(
            fixture
                .post(BODY.to_vec(), signed(BODY, "extreme", timestamp, 42))
                .await,
            401
        );
    }
    let malformed = b"not-json";
    assert_eq!(
        fixture
            .post(malformed.to_vec(), signed(malformed, "bad", now(), 42))
            .await,
        400
    );
    let oversized = vec![b'x'; 65537];
    assert_eq!(
        fixture
            .post(oversized.clone(), signed(&oversized, "large", now(), 42))
            .await,
        413
    );
    assert_eq!(fixture.count().await, 0);
    fixture.database.close().await;
}

#[tokio::test]
async fn inbox_failure_is_retryable_without_acknowledging_or_losing_the_event() {
    let fixture = Fixture::new().await;
    sqlx::raw_sql("CREATE TRIGGER fail_inbox BEFORE INSERT ON webhook_inbox BEGIN SELECT RAISE(ABORT,'fixture disk/write failure'); END")
        .execute(fixture.database.pool()).await.unwrap();
    let headers = signed(BODY, "retryable", now(), 42);
    assert_eq!(fixture.post(BODY.to_vec(), headers.clone()).await, 503);
    assert_eq!(fixture.count().await, 0);
    sqlx::query("DROP TRIGGER fail_inbox")
        .execute(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(fixture.post(BODY.to_vec(), headers).await, 204);
    assert_eq!(fixture.count().await, 1);
    fixture.database.close().await;
}

#[test]
fn published_standard_webhooks_vector_matches_svix_wire_signature() {
    let webhook = Webhook::new("whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD").unwrap();
    assert_eq!(
        webhook
            .sign(
                "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk",
                1649367553,
                br#"{"email":"test@example.com","username":"test_user"}"#
            )
            .unwrap(),
        "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw="
    );
}
