//! Real local HTTP + SQLite; signed fixtures never contact SendGrid.
#[path = "support/sendgrid_inbox.rs"]
mod inbox;
#[path = "support/email_suppression.rs"]
mod suppression;

use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::STANDARD};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer},
    pkcs8::EncodePublicKey,
};
use puxiri::db::Database;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

const NOW: i64 = 1_800_000_000;
struct Fixture {
    directory: TempDir,
    database: Option<Arc<Database>>,
    key: SigningKey,
    config: String,
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
        inbox::schema(&database).await;
        let key = SigningKey::from_slice(&[42; 32]).unwrap(); // public artificial test material
        let config = "sendgrid-fixture-v1".to_owned();
        let (base, task) = serve(database.clone(), &key, &config).await;
        Self {
            directory,
            database: Some(database),
            key,
            config,
            base,
            task,
        }
    }
    async fn restart(&mut self, config: &str) {
        self.task.abort();
        // Join before releasing the process lock; endpoint owns another Arc.
        let old = std::mem::replace(&mut self.task, tokio::spawn(async {}));
        let _ = old.await;
        let old_db = self.database.take().unwrap();
        old_db.close().await;
        drop(old_db);
        self.database = Some(Arc::new(
            Database::open(self.directory.path()).await.unwrap(),
        ));
        self.config = config.into();
        (self.base, self.task) =
            serve(self.database.as_ref().unwrap().clone(), &self.key, config).await;
    }
    async fn post(&self, body: Vec<u8>, headers: HeaderMap) -> u16 {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .post(format!("{}/callbacks/sendgrid", self.base))
            .headers(headers)
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
    async fn send(&self, events: Value) -> u16 {
        let body = serde_json::to_vec(&events).unwrap();
        let headers = signed(&self.key, &body, NOW);
        self.post(body, headers).await
    }
    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM sendgrid_inbox")
            .fetch_one(self.db().pool())
            .await
            .unwrap()
    }
    fn db(&self) -> &Database {
        self.database.as_ref().unwrap()
    }
}
async fn serve(
    database: Arc<Database>,
    key: &SigningKey,
    config: &str,
) -> (String, tokio::task::JoinHandle<()>) {
    let encoded = STANDARD.encode(key.verifying_key().to_public_key_der().unwrap().as_bytes());
    let key = inbox::public_key(&encoded).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = inbox::router(database, key, config, Arc::new(|| NOW));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, task)
}
fn signed(key: &SigningKey, body: &[u8], timestamp: i64) -> HeaderMap {
    let timestamp = timestamp.to_string();
    let mut bytes = timestamp.as_bytes().to_vec();
    bytes.extend_from_slice(body);
    let signature: Signature = key.sign(&bytes);
    let mut headers = HeaderMap::new();
    headers.insert(inbox::TIMESTAMP_HEADER, timestamp.parse().unwrap());
    headers.insert(
        inbox::SIGNATURE_HEADER,
        STANDARD
            .encode(signature.to_der().as_bytes())
            .parse()
            .unwrap(),
    );
    headers
}
fn event(id: &str, kind: &str) -> Value {
    json!({"sg_event_id":id,"event":kind,"timestamp":NOW-10,"email":"recipient@example.invalid","sg_message_id":"fixture-message.filter-1"})
}

#[test]
fn official_sendgrid_helper_vector_verifies_spki_der_and_crlf_sensitive_payload() {
    // Published SendGrid Node helper fixture. Timestamp is deliberately old:
    // crypto-only vector does not bypass the HTTP endpoint freshness policy.
    let key = inbox::public_key("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE83T4O/n84iotIvIW4mdBgQ/7dAfSmpqIM8kF9mN1flpVKS3GRqe62gw+2fNNRaINXvVpiglSI8eNEc6wEA3F+g==").unwrap();
    let signature = "MEUCIGHQVtGj+Y3LkG9fLcxf3qfI10QysgDWmMOVmxG0u6ZUAiEAyBiXDWzM+uOe5W0JuG+luQAbPIqHh89M15TluLtEZtM=";
    let body = concat!(
        r#"[{"email":"hello@world.com","event":"dropped","reason":"Bounced Address","sg_event_id":"ZHJvcC0xMDk5NDkxOS1MUnpYbF9OSFN0T0doUTRrb2ZTbV9BLTA","sg_message_id":"LRzXl_NHStOGhQ4kofSm_A.filterdrecv-p3mdw1-756b745b58-kmzbl-18-5F5FC76C-9.0","smtp-id":"<LRzXl_NHStOGhQ4kofSm_A@ismtpd0039p1iad1.sendgrid.net>","timestamp":1600112492}]"#,
        "\r\n"
    );
    assert!(inbox::verify(
        &key,
        "1600112502",
        signature,
        body.as_bytes()
    ));
    assert!(!inbox::verify(
        &key,
        "1600112502",
        signature,
        body.trim_end().as_bytes()
    ));
    assert!(!inbox::verify(
        &key,
        "1600112503",
        signature,
        body.as_bytes()
    ));
    assert!(inbox::public_key("not-base64").is_err());
    assert!(inbox::public_key(&STANDARD.encode([1u8; 64])).is_err());
}

#[tokio::test]
async fn authenticated_batches_deduplicate_per_event_concurrently_and_after_rebatch_and_restart() {
    let mut fixture = Fixture::new().await;
    let batch = json!([event("a", "delivered"), event("b", "spamreport")]);
    let (first, second) = tokio::join!(fixture.send(batch.clone()), fixture.send(batch.clone()));
    assert_eq!((first, second), (204, 204));
    assert_eq!(fixture.count().await, 2);
    assert_eq!(
        fixture
            .send(json!([event("b", "spamreport"), event("c", "bounce")]))
            .await,
        204
    );
    assert_eq!(fixture.count().await, 3);
    let pretty = serde_json::to_vec_pretty(&batch).unwrap();
    assert_eq!(
        fixture
            .post(
                pretty.clone(),
                signed(&fixture.key, &pretty, NOW - 24 * 3600)
            )
            .await,
        204
    );
    fixture.restart("sendgrid-fixture-v1").await;
    assert_eq!(fixture.send(batch.clone()).await, 204);
    assert_eq!(fixture.count().await, 3);
    fixture.restart("sendgrid-fixture-v2").await;
    assert_eq!(fixture.send(batch).await, 204);
    assert_eq!(fixture.count().await, 5); // different config identity doesn't alias old ids
    fixture.db().close().await;
}

#[tokio::test]
async fn invalid_authentication_raw_bytes_and_timestamp_never_reach_inbox_even_on_replay() {
    let fixture = Fixture::new().await;
    let body = serde_json::to_vec(&json!([event("replay", "delivered")])).unwrap();
    assert_eq!(
        fixture
            .post(body.clone(), signed(&fixture.key, &body, NOW))
            .await,
        204
    );
    let other = SigningKey::from_slice(&[43; 32]).unwrap();
    let mut duplicate = signed(&fixture.key, &body, NOW);
    duplicate.append(inbox::TIMESTAMP_HEADER, NOW.to_string().parse().unwrap());
    let mut malformed = signed(&fixture.key, &body, NOW);
    malformed.insert(inbox::SIGNATURE_HEADER, "not-a-signature".parse().unwrap());
    let mut duplicate_signature = signed(&fixture.key, &body, NOW);
    duplicate_signature.append(
        inbox::SIGNATURE_HEADER,
        duplicate_signature[inbox::SIGNATURE_HEADER].clone(),
    );
    for headers in [
        HeaderMap::new(),
        duplicate,
        duplicate_signature,
        malformed,
        signed(&other, &body, NOW),
        signed(&fixture.key, &body, NOW - 25 * 3600 - 1),
        signed(&fixture.key, &body, NOW + 301),
        signed(&fixture.key, &body, i64::MIN),
        signed(&fixture.key, &body, i64::MAX),
    ] {
        assert_eq!(fixture.post(body.clone(), headers).await, 401);
    }
    let mut changed = body.clone();
    changed.push(b' ');
    assert_eq!(
        fixture
            .post(changed, signed(&fixture.key, &body, NOW))
            .await,
        401
    );
    assert_eq!(fixture.count().await, 1);
    fixture.db().close().await;
}

#[tokio::test]
async fn invalid_conflicting_or_failed_batch_rolls_back_every_event_and_can_retry() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture.send(json!([event("original", "delivered")])).await,
        204
    );
    assert_eq!(
        fixture
            .send(json!([
                event("new", "delivered"),
                event("original", "bounce")
            ]))
            .await,
        409
    );
    assert_eq!(fixture.count().await, 1);
    assert_eq!(
        fixture
            .send(json!([event("new","delivered"),{"event":"bounce"}]))
            .await,
        400
    );
    assert_eq!(
        fixture
            .send(json!([
                event("same-batch", "delivered"),
                event("same-batch", "bounce")
            ]))
            .await,
        409
    );
    assert_eq!(fixture.count().await, 1);
    sqlx::raw_sql("CREATE TRIGGER fail_second BEFORE INSERT ON sendgrid_inbox WHEN NEW.event_id='fail' BEGIN SELECT RAISE(ABORT,'fixture write failure'); END")
        .execute(fixture.db().pool()).await.unwrap();
    let batch = json!([event("first", "processed"), event("fail", "bounce")]);
    assert_eq!(fixture.send(batch.clone()).await, 503);
    assert_eq!(fixture.count().await, 1);
    sqlx::query("DROP TRIGGER fail_second")
        .execute(fixture.db().pool())
        .await
        .unwrap();
    assert_eq!(fixture.send(batch).await, 204);
    assert_eq!(fixture.count().await, 3);
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT event_json FROM sendgrid_inbox WHERE event_id='original'")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&original).unwrap()["event"],
        "delivered"
    );
    fixture.db().close().await;
}

#[tokio::test]
async fn bounded_batches_quarantine_unknown_and_uncorrelatable_events_without_suppression() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.send(json!([])).await, 400);
    assert_eq!(fixture.send(json!({"event":"bounce"})).await, 400);
    assert_eq!(
        fixture
            .send(json!(
                (0..513)
                    .map(|i| event(&format!("event-{i}"), "processed"))
                    .collect::<Vec<_>>()
            ))
            .await,
        400
    );
    let huge = vec![b'x'; inbox::BODY_LIMIT + 1];
    assert_eq!(
        fixture
            .post(huge.clone(), signed(&fixture.key, &huge, NOW))
            .await,
        413
    );
    assert_eq!(fixture.count().await, 0);
    let invalid = b"not-json".to_vec();
    assert_eq!(
        fixture
            .post(invalid.clone(), signed(&fixture.key, &invalid, NOW))
            .await,
        400
    );
    let mut delayed = event("delayed", "bounce");
    delayed.as_object_mut().unwrap().remove("sg_message_id");
    assert_eq!(fixture.send(json!([delayed,{"sg_event_id":"account","event":"account_status_change","type":"compliance_suspend"},
        {"sg_event_id":"future","event":"new-provider-kind"}])).await,204);
    assert_eq!(fixture.count().await, 3);
    // Above the Resend fixture's 64KiB ceiling: SendGrid batches have a distinct
    // limit. Use actual TCP body extraction, not a direct handler call.
    let mut large = event("large", "dropped");
    large["reason"] = json!("x".repeat(80 * 1024));
    assert_eq!(fixture.send(json!([large])).await, 204);
    assert_eq!(fixture.count().await, 4);
    // This fixture has no suppression/users/send-state tables or effects.
    fixture.db().close().await;
}

const ATTEMPT: &str = "0190d0c4-7e9a-7b3c-9f12-1a2b3c4d5e6f";
async fn registered(fixture: &Fixture, config: &str, recipient: &str) {
    sqlx::query("INSERT INTO sg_feedback_sends(config,attempt,recipient) VALUES(?,?,?)")
        .bind(config)
        .bind(ATTEMPT)
        .bind(recipient)
        .execute(fixture.db().pool())
        .await
        .unwrap();
}
fn correlated(id: &str, kind: &str) -> Value {
    let mut value = event(id, kind);
    value["puxiri_attempt"] = json!(ATTEMPT);
    value
}
async fn effects(fixture: &Fixture) -> (i64, i64) {
    (
        sqlx::query_scalar("SELECT count(*) FROM sg_feedback_suppression")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM sg_feedback_audit")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn signed_correlated_complaint_is_deduplicated_persistent_and_never_bans_users_or_clears_on_delivery()
 {
    let mut fixture = Fixture::new().await;
    registered(&fixture, "sendgrid-fixture-v1", "recipient@example.invalid").await;
    sqlx::raw_sql("CREATE TABLE fixture_users(id TEXT PRIMARY KEY,banned INTEGER); INSERT INTO fixture_users VALUES('user',0);
        CREATE TABLE fixture_bell(id TEXT PRIMARY KEY); INSERT INTO fixture_bell VALUES('notification');")
        .execute(fixture.db().pool()).await.unwrap();
    let batch = json!([correlated("complaint", "spamreport")]);
    let (a, b) = tokio::join!(fixture.send(batch.clone()), fixture.send(batch.clone()));
    assert_eq!((a, b), (204, 204));
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.restart("sendgrid-fixture-v1").await;
    assert_eq!(fixture.send(batch).await, 204);
    assert_eq!(
        fixture
            .send(json!([correlated("late-delivery", "delivered")]))
            .await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT banned FROM fixture_users")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fixture_bell")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        1
    );
    let audit: Vec<(String, String, String)> =
        sqlx::query_as("SELECT config,attempt,reason FROM sg_feedback_audit")
            .fetch_all(fixture.db().pool())
            .await
            .unwrap();
    assert!(!format!("{audit:?}").contains("recipient@example.invalid"));
    fixture.db().close().await;
}

#[tokio::test]
async fn blocked_content_reputation_or_missing_classification_cannot_become_permanent_invalid() {
    let fixture = Fixture::new().await;
    registered(&fixture, &fixture.config, "recipient@example.invalid").await;
    for (index, (bounce_type, classification)) in [
        ("blocked", "Invalid Address"),
        ("bounce", "Content"),
        ("bounce", "Reputation"),
        ("bounce", ""),
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = correlated(&format!("not-invalid-{index}"), "bounce");
        value["type"] = json!(bounce_type);
        value["bounce_classification"] = json!(classification);
        assert_eq!(fixture.send(json!([value])).await, 204);
    }
    assert_eq!(effects(&fixture).await, (0, 0));
    let mut value = correlated("invalid", "bounce");
    value["type"] = json!("bounce");
    value["bounce_classification"] = json!("Invalid Address");
    assert_eq!(fixture.send(json!([value])).await, 204);
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.db().close().await;
}

#[tokio::test]
async fn missing_attempt_wrong_config_or_address_never_uses_message_prefix_as_correlation() {
    let fixture = Fixture::new().await;
    registered(&fixture, "another-config", "recipient@example.invalid").await;
    assert_eq!(
        fixture
            .send(json!([correlated("wrong-config", "spamreport")]))
            .await,
        204
    );
    registered(&fixture, &fixture.config, "expected@example.invalid").await;
    assert_eq!(
        fixture
            .send(json!([
                correlated("wrong-recipient", "spamreport"),
                event("no-token", "spamreport")
            ]))
            .await,
        204
    );
    assert_eq!(effects(&fixture).await, (0, 0));
    let body = serde_json::to_vec(&json!([correlated("bad-signature", "spamreport")])).unwrap();
    let other = SigningKey::from_slice(&[43; 32]).unwrap();
    assert_eq!(
        fixture.post(body.clone(), signed(&other, &body, NOW)).await,
        401
    );
    assert_eq!(effects(&fixture).await, (0, 0));
    fixture.db().close().await;
}

#[tokio::test]
async fn early_event_can_reconcile_by_authenticated_replay_without_inventing_sendgrid_message_mapping()
 {
    let fixture = Fixture::new().await;
    let mut early = correlated("early", "spamreport");
    early.as_object_mut().unwrap().remove("sg_message_id");
    assert_eq!(fixture.send(json!([early.clone()])).await, 204);
    assert_eq!(effects(&fixture).await, (0, 0));
    registered(&fixture, &fixture.config, "recipient@example.invalid").await;
    assert_eq!(fixture.send(json!([early.clone()])).await, 204);
    assert_eq!(fixture.send(json!([early])).await, 204);
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.db().close().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_all_batch_effects_and_inbox_before_http_ack() {
    let fixture = Fixture::new().await;
    registered(&fixture, &fixture.config, "recipient@example.invalid").await;
    sqlx::raw_sql("CREATE TRIGGER fail_feedback BEFORE INSERT ON sg_feedback_audit BEGIN SELECT RAISE(ABORT,'fixture audit failure'); END")
        .execute(fixture.db().pool()).await.unwrap();
    let batch = json!([
        correlated("first", "delivered"),
        correlated("second", "spamreport")
    ]);
    assert_eq!(fixture.send(batch.clone()).await, 503);
    assert_eq!(fixture.count().await, 0);
    assert_eq!(effects(&fixture).await, (0, 0));
    sqlx::query("DROP TRIGGER fail_feedback")
        .execute(fixture.db().pool())
        .await
        .unwrap();
    assert_eq!(fixture.send(batch).await, 204);
    assert_eq!(fixture.count().await, 2);
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.db().close().await;
}
