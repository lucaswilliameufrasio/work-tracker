//! SaaS fake only; local SQLite/HTTP/client are real. No Cloudflare provisioning.
#[path = "support/cloudflare_queue.rs"]
mod queue;
#[path = "support/email_suppression.rs"]
mod suppression;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use puxiri::db::Database;
use queue::{Consumer, Failure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}
impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }
}
struct Call {
    path: String,
    headers: HeaderMap,
    body: Value,
    persisted_at_call: i64,
    effects_at_call: i64,
}
struct Server {
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<Call>>,
    database: Mutex<Option<Arc<Database>>>,
}
struct Fixture {
    directory: TempDir,
    database: Option<Arc<Database>>,
    server: Arc<Server>,
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
        queue::schema(&database).await;
        let server = Arc::new(Server {
            replies: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            database: Mutex::new(Some(database.clone())),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(post(receive))
            .with_state(server.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            database: Some(database),
            server,
            base,
            task,
        }
    }
    fn db(&self) -> &Database {
        self.database.as_ref().unwrap()
    }
    fn client(&self) -> Consumer {
        Consumer::local(
            &self.base,
            self.database.as_ref().unwrap().clone(),
            Duration::from_secs(2),
        )
        .unwrap()
    }
    fn replies(&self, replies: Vec<Reply>) {
        self.server.replies.lock().unwrap().extend(replies);
    }
    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM cloudflare_inbox")
            .fetch_one(self.db().pool())
            .await
            .unwrap()
    }
    async fn reopen(&mut self) {
        // Clients are dropped by the caller; the fake's db reference must also
        // be released before opening the same installation lock again.
        self.server.database.lock().unwrap().take();
        let old = self.database.take().unwrap();
        old.close().await;
        drop(old);
        let database = Arc::new(Database::open(self.directory.path()).await.unwrap());
        *self.server.database.lock().unwrap() = Some(database.clone());
        self.database = Some(database);
    }
}
async fn receive(
    State(server): State<Arc<Server>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let database = server.database.lock().unwrap().as_ref().unwrap().clone();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cloudflare_inbox")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let effects_at_call = sqlx::query_scalar("SELECT count(*) FROM cf_feedback_audit")
        .fetch_one(database.pool())
        .await
        .unwrap();
    server.calls.lock().unwrap().push(Call {
        path: uri.path().into(),
        headers,
        body: serde_json::from_slice(&body).unwrap(),
        persisted_at_call: count,
        effects_at_call,
    });
    let reply = server
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("unexpected hidden retry/request");
    tokio::time::sleep(reply.delay).await;
    (
        StatusCode::from_u16(reply.status).unwrap(),
        [("location", "http://127.0.0.1:1/never-forward")],
        reply.body,
    )
}
fn event(id: &str, kind: &str) -> Value {
    json!({"type":format!("cf.email.sending.message.{kind}"),
    "source":{"type":"email.sending","zoneId":queue::ZONE,"domain":queue::DOMAIN},
    "payload":{"eventId":id,"messageId":"fixture-email-id","recipient":"recipient@example.invalid","terminal":true,"delivery":{"status":kind}},
    "metadata":{"accountId":queue::ACCOUNT,"eventSubscriptionId":queue::SUBSCRIPTION,"eventSchemaVersion":1,"eventTimestamp":"2026-10-04T12:00:00Z"}})
}
fn message(event: Value, lease: &str, content_type: &str) -> Value {
    let bytes = serde_json::to_vec(&event).unwrap();
    json!({"id":"ephemeral-id-not-dedup-key","body":if content_type=="text"{String::from_utf8(bytes).unwrap()}else{STANDARD.encode(bytes)},
        "lease_id":lease,"attempts":2,"timestamp_ms":1800000000000u64,"metadata":{"CF-Content-Type":content_type}})
}
fn pull(messages: Vec<Value>) -> Reply {
    Reply::json(
        json!({"success":true,"errors":[],"result":{"messages":messages,"message_backlog_count":10}}),
    )
}
fn ack(count: usize) -> Reply {
    Reply::json(
        json!({"success":true,"errors":[],"result":{"ackCount":count,"retryCount":0,"warnings":{}}}),
    )
}

const RECIPIENT: &str = "recipient@example.invalid";
fn config() -> String {
    format!(
        "{}/{}/{}",
        queue::ACCOUNT,
        queue::QUEUE,
        queue::SUBSCRIPTION
    )
}
async fn registered(fixture: &Fixture, scope: &str) {
    sqlx::query(
        "INSERT INTO cf_feedback_sends(config,message_id,recipient) VALUES(?,'fixture-email-id',?)",
    )
    .bind(scope)
    .bind(RECIPIENT)
    .execute(fixture.db().pool())
    .await
    .unwrap();
}
fn classified(id: &str, kind: &str) -> Value {
    let mut value = event(id, kind);
    if kind == "complained" {
        value["payload"]["complaint"] = json!({"type":"abuse"});
    }
    if kind == "bounced" {
        value["payload"]["bounce"] =
            json!({"type":"hard","classification":"permanent_failure","reason":"PRIVATE"});
        value["payload"]["delivery"]["smtpEnhancedStatusCode"] = json!("5.1.1");
    }
    value
}
async fn effects(fixture: &Fixture) -> (i64, i64) {
    (
        sqlx::query_scalar("SELECT count(*) FROM cf_feedback_suppression")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM cf_feedback_audit")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
    )
}
async fn consume(fixture: &Fixture, value: Value, lease: &str) -> Result<usize, Failure> {
    fixture.replies(vec![pull(vec![message(value, lease, "json")]), ack(1)]);
    fixture.client().consume_once().await
}

#[tokio::test]
async fn correlated_complaint_commits_before_ack_and_survives_reopen_without_banning_or_clearing_on_delivery()
 {
    let mut fixture = Fixture::new().await;
    registered(&fixture, &config()).await;
    sqlx::raw_sql("CREATE TABLE fixture_users(id TEXT PRIMARY KEY,banned INTEGER); INSERT INTO fixture_users VALUES('user',0);
        CREATE TABLE fixture_bell(id TEXT PRIMARY KEY); INSERT INTO fixture_bell VALUES('notification');")
        .execute(fixture.db().pool()).await.unwrap();
    assert_eq!(
        consume(&fixture, classified("complaint", "complained"), "lease-old").await,
        Ok(1)
    );
    assert_eq!(
        fixture
            .server
            .calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .effects_at_call,
        1
    );
    fixture.reopen().await;
    assert_eq!(
        consume(&fixture, classified("complaint", "complained"), "lease-new").await,
        Ok(1)
    );
    assert_eq!(
        consume(&fixture, event("delivery", "delivered"), "lease-delivery").await,
        Ok(1)
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
        sqlx::query_as("SELECT event_id,message_id,reason FROM cf_feedback_audit")
            .fetch_all(fixture.db().pool())
            .await
            .unwrap();
    assert!(!format!("{audit:?}").contains(RECIPIENT));
    assert!(!format!("{audit:?}").contains("PRIVATE"));
}

#[tokio::test]
async fn terminal_soft_bounce_policy_rejection_and_inconsistent_classification_never_suppress() {
    let fixture = Fixture::new().await;
    registered(&fixture, &config()).await;
    let mut cases = vec![
        event("deferred", "deferred"),
        event("failed", "failed"),
        event("rejected", "rejected"),
        event("unknown", "new_kind"),
        event("missing", "bounced"),
    ];
    let mut soft = classified("soft", "bounced");
    soft["payload"]["bounce"]["type"] = json!("soft");
    cases.push(soft);
    let mut content = classified("content", "bounced");
    content["payload"]["delivery"]["smtpEnhancedStatusCode"] = json!("5.7.1");
    cases.push(content);
    let mut transient = classified("transient", "bounced");
    transient["payload"]["bounce"]["classification"] = json!("temporary_failure");
    cases.push(transient);
    let mut nonterminal = classified("nonterminal", "bounced");
    nonterminal["payload"]["terminal"] = json!(false);
    cases.push(nonterminal);
    let mut inconsistent = classified("inconsistent", "bounced");
    inconsistent["payload"]["delivery"]["status"] = json!("delivered");
    cases.push(inconsistent);
    let mut notspam = classified("notspam", "complained");
    notspam["payload"]["complaint"]["type"] = json!("not-spam");
    cases.push(notspam);
    cases.push(event("complaint-missing", "complained"));
    for (index, value) in cases.into_iter().enumerate() {
        assert_eq!(
            consume(&fixture, value, &format!("lease-{index}")).await,
            Ok(1)
        );
    }
    assert_eq!(effects(&fixture).await, (0, 0));
    assert_eq!(
        consume(&fixture, classified("invalid", "bounced"), "lease-invalid").await,
        Ok(1)
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        consume(
            &fixture,
            classified("complaint", "complained"),
            "lease-complaint"
        )
        .await,
        Ok(1)
    );
    assert_eq!(
        consume(
            &fixture,
            classified("later-bounce", "bounced"),
            "lease-later"
        )
        .await,
        Ok(1)
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT reason FROM cf_feedback_suppression")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        "complaint"
    );
}

#[tokio::test]
async fn unknown_send_wrong_configuration_and_recipient_are_inbox_only_then_acked_event_reconciles_locally()
 {
    let fixture = Fixture::new().await;
    registered(&fixture, "other-config").await;
    assert_eq!(
        consume(&fixture, classified("early", "complained"), "lease-early").await,
        Ok(1)
    );
    assert_eq!(effects(&fixture).await, (0, 0));
    queue::feedback::reconcile(fixture.db()).await.unwrap();
    assert_eq!(effects(&fixture).await, (0, 0));
    registered(&fixture, &config()).await;
    let mut recipient = classified("mismatch", "complained");
    recipient["payload"]["recipient"] = json!("other@example.invalid");
    assert_eq!(consume(&fixture, recipient, "lease-mismatch").await, Ok(1));
    assert_eq!(effects(&fixture).await, (0, 0));
    queue::feedback::reconcile(fixture.db()).await.unwrap();
    queue::feedback::reconcile(fixture.db()).await.unwrap();
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        consume(&fixture, classified("early", "complained"), "lease-replay").await,
        Ok(1)
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    let mut scope = classified("bad-scope", "complained");
    scope["metadata"]["eventSubscriptionId"] = json!("other-subscription");
    fixture.replies(vec![pull(vec![message(scope, "bad-lease", "json")])]);
    assert_eq!(fixture.client().consume_once().await, Err(Failure::Scope));
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        fixture.server.calls.lock().unwrap().last().unwrap().body["batch_size"],
        queue::BATCH
    );
}

#[tokio::test]
async fn failed_projection_rolls_back_whole_batch_without_ack_and_conflicting_event_cannot_change_recipient()
 {
    let fixture = Fixture::new().await;
    registered(&fixture, &config()).await;
    sqlx::raw_sql("CREATE TRIGGER fail_feedback BEFORE INSERT ON cf_feedback_audit BEGIN SELECT RAISE(ABORT,'fixture failure'); END")
        .execute(fixture.db().pool()).await.unwrap();
    let batch = vec![
        message(event("delivery", "delivered"), "lease-a", "json"),
        message(classified("complaint", "complained"), "lease-b", "json"),
    ];
    fixture.replies(vec![pull(batch.clone())]);
    assert_eq!(
        fixture.client().consume_once().await,
        Err(Failure::Database)
    );
    assert_eq!(fixture.count().await, 0);
    assert_eq!(effects(&fixture).await, (0, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM cf_feedback_projection")
            .fetch_one(fixture.db().pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_feedback")
        .execute(fixture.db().pool())
        .await
        .unwrap();
    fixture.replies(vec![pull(batch), ack(2)]);
    assert_eq!(fixture.client().consume_once().await, Ok(2));
    assert_eq!(effects(&fixture).await, (1, 1));
    let mut conflict = classified("complaint", "complained");
    conflict["payload"]["recipient"] = json!("other@example.invalid");
    fixture.replies(vec![pull(vec![message(
        conflict,
        "lease-conflict",
        "json",
    )])]);
    assert_eq!(
        fixture.client().consume_once().await,
        Err(Failure::Conflict)
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn concurrent_projection_and_ack_ambiguity_never_duplicate_or_undo_committed_suppression() {
    let fixture = Fixture::new().await;
    registered(&fixture, &config()).await;
    fixture.replies(vec![
        pull(vec![message(
            classified("complaint", "complained"),
            "lease-a",
            "json",
        )]),
        pull(vec![message(
            classified("complaint", "complained"),
            "lease-b",
            "text",
        )]),
    ]);
    let a = fixture.client();
    let b = fixture.client();
    let am = a.pull().await.unwrap();
    let bm = b.pull().await.unwrap();
    let (ar, br) = tokio::join!(a.persist(&am), b.persist(&bm));
    assert_eq!((ar, br), (Ok(()), Ok(())));
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.replies(vec![
        pull(vec![message(
            classified("complaint", "complained"),
            "lease-current",
            "json",
        )]),
        ack(0),
    ]);
    assert_eq!(
        fixture.client().consume_once().await,
        Err(Failure::AckUnknown)
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        fixture
            .server
            .calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .effects_at_call,
        1
    );
}

#[tokio::test]
async fn bearer_paths_base64_json_and_text_are_persisted_before_acknowledging_leases() {
    let fixture = Fixture::new().await;
    fixture.replies(vec![
        pull(vec![
            message(event("a", "delivered"), "lease-a", "json"),
            message(event("b", "complained"), "lease-b", "text"),
        ]),
        ack(2),
    ]);
    assert_eq!(fixture.client().consume_once().await, Ok(2));
    assert_eq!(fixture.count().await, 2);
    let calls = fixture.server.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].path,
        format!(
            "/client/v4/accounts/{}/queues/{}/messages/pull",
            queue::ACCOUNT,
            queue::QUEUE
        )
    );
    assert_eq!(
        calls[0].body,
        json!({"batch_size":queue::BATCH,"visibility_timeout_ms":30000})
    );
    assert_eq!(
        calls[1].body,
        json!({"acks":[{"lease_id":"lease-a"},{"lease_id":"lease-b"}],"retries":[]})
    );
    assert!(calls[1].path.ends_with("/messages/ack"));
    assert_eq!(calls[1].persisted_at_call, 2);
    for call in calls.iter() {
        assert_eq!(
            call.headers["authorization"],
            format!("Bearer {}", queue::TOKEN)
        );
        assert_eq!(call.headers["content-type"], "application/json");
    }
}

#[tokio::test]
async fn committed_but_unacked_batch_replays_after_reopen_with_new_lease_and_no_duplicate() {
    let mut fixture = Fixture::new().await;
    fixture.replies(vec![pull(vec![message(
        event("a", "bounced"),
        "old-lease",
        "json",
    )])]);
    let client = fixture.client();
    let messages = client.pull().await.unwrap();
    client.persist(&messages).await.unwrap();
    drop(client);
    assert_eq!(fixture.count().await, 1);
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 1);
    fixture.reopen().await;
    fixture.replies(vec![
        pull(vec![message(event("a", "bounced"), "new-lease", "json")]),
        ack(1),
    ]);
    assert_eq!(fixture.client().consume_once().await, Ok(1));
    assert_eq!(fixture.count().await, 1);
    assert_eq!(
        fixture.server.calls.lock().unwrap().last().unwrap().body["acks"][0]["lease_id"],
        "new-lease"
    );
}

#[tokio::test]
async fn ack_failure_or_partial_counts_are_unknown_but_never_undo_persisted_events() {
    let fixture = Fixture::new().await;
    for reply in [
        Reply {
            status: 503,
            body: format!("{} secret-provider-body", queue::TOKEN),
            delay: Duration::ZERO,
        },
        ack(0),
        Reply::json(
            json!({"success":true,"result":{"ackCount":1,"retryCount":0,"warnings":{"lease-a":"expired"}}}),
        ),
        Reply::json(json!({"success":true,"result":{}})),
    ] {
        fixture.replies(vec![
            pull(vec![message(event("a", "delivered"), "lease-a", "json")]),
            reply,
        ]);
        let error = fixture.client().consume_once().await.unwrap_err();
        assert_eq!(error, Failure::AckUnknown);
        assert!(!format!("{error:?}").contains(queue::TOKEN));
        assert_eq!(fixture.count().await, 1);
    }
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 8);
}

#[tokio::test]
async fn invalid_scope_content_metadata_and_conflict_are_not_acknowledged() {
    let fixture = Fixture::new().await;
    let mut wrong = event("bad", "delivered");
    wrong["metadata"]["accountId"] = json!("another-account");
    let mut missing = message(event("bad", "delivered"), "lease", "json");
    missing.as_object_mut().unwrap().remove("metadata");
    let mut base64 = message(event("bad", "delivered"), "lease", "json");
    base64["body"] = json!("not-base64!");
    for messages in [
        vec![message(wrong, "lease", "json")],
        vec![missing],
        vec![base64],
        vec![message(event("bad", "delivered"), "lease", "v8")],
        vec![
            message(event("good", "delivered"), "same-lease", "json"),
            message(event("bad", "bounced"), "same-lease", "json"),
        ],
    ] {
        fixture.replies(vec![pull(messages)]);
        assert!(fixture.client().consume_once().await.is_err());
        assert_eq!(fixture.count().await, 0);
    }
    fixture.replies(vec![
        pull(vec![message(event("a", "delivered"), "lease-a", "json")]),
        ack(1),
    ]);
    fixture.client().consume_once().await.unwrap();
    fixture.replies(vec![pull(vec![
        message(event("new", "delivered"), "lease-new", "json"),
        message(event("a", "bounced"), "lease-a", "json"),
    ])]);
    assert_eq!(
        fixture.client().consume_once().await,
        Err(Failure::Conflict)
    );
    assert_eq!(fixture.count().await, 1);
    assert_eq!(
        fixture
            .server
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.path.ends_with("/ack"))
            .count(),
        1
    );
}

#[tokio::test]
async fn write_failure_has_no_ack_or_partial_commit_and_retry_can_persist_batch() {
    let fixture = Fixture::new().await;
    sqlx::raw_sql("CREATE TRIGGER fail_second BEFORE INSERT ON cloudflare_inbox WHEN NEW.event_id='fail' BEGIN SELECT RAISE(ABORT,'fixture write failure'); END")
        .execute(fixture.db().pool()).await.unwrap();
    let messages = vec![
        message(event("first", "delivered"), "lease-first", "json"),
        message(event("fail", "failed"), "lease-fail", "json"),
    ];
    fixture.replies(vec![pull(messages.clone())]);
    assert_eq!(
        fixture.client().consume_once().await,
        Err(Failure::Database)
    );
    assert_eq!(fixture.count().await, 0);
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_second")
        .execute(fixture.db().pool())
        .await
        .unwrap();
    fixture.replies(vec![pull(messages), ack(2)]);
    assert_eq!(fixture.client().consume_once().await, Ok(2));
    assert_eq!(fixture.count().await, 2);
}

#[tokio::test]
async fn empty_malformed_errors_oversized_redirect_and_timeout_do_not_spin_retry_or_ack() {
    let fixture = Fixture::new().await;
    fixture.replies(vec![pull(vec![])]);
    assert_eq!(fixture.client().consume_once().await, Ok(0));
    for reply in [
        Reply::json(
            json!({"success":false,"errors":[{"message":queue::TOKEN}],"result":{"messages":[]}}),
        ),
        Reply::json(json!({"success":true,"result":{}})),
        Reply {
            status: 200,
            body: "not-json".into(),
            delay: Duration::ZERO,
        },
        Reply {
            status: 200,
            body: "x".repeat(2 * 1024 * 1024 + 1),
            delay: Duration::ZERO,
        },
        Reply {
            status: 307,
            body: String::new(),
            delay: Duration::ZERO,
        },
        Reply {
            status: 403,
            body: queue::TOKEN.into(),
            delay: Duration::ZERO,
        },
        Reply {
            status: 429,
            body: String::new(),
            delay: Duration::ZERO,
        },
    ] {
        fixture.replies(vec![reply]);
        assert!(fixture.client().consume_once().await.is_err());
    }
    fixture.replies(vec![Reply {
        status: 200,
        body: json!({"success":true,"result":{"messages":[]}}).to_string(),
        delay: Duration::from_millis(300),
    }]);
    let client = Consumer::local(
        &fixture.base,
        fixture.database.as_ref().unwrap().clone(),
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(client.consume_once().await, Err(Failure::Transport));
    assert_eq!(fixture.count().await, 0);
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 9);
    assert!(
        Consumer::local(
            "https://api.cloudflare.com/",
            fixture.database.as_ref().unwrap().clone(),
            Duration::from_secs(1)
        )
        .is_err()
    );
}

#[tokio::test]
async fn concurrent_persistence_and_ack_timeout_keep_one_durable_event_for_redelivery() {
    let fixture = Fixture::new().await;
    fixture.replies(vec![pull(vec![message(
        event("one", "deferred"),
        "first-lease",
        "json",
    )])]);
    let client = fixture.client();
    let messages = client.pull().await.unwrap();
    let (first, second) = tokio::join!(client.persist(&messages), client.persist(&messages));
    assert_eq!((first, second), (Ok(()), Ok(())));
    assert_eq!(fixture.count().await, 1);
    fixture.replies(vec![
        pull(vec![message(event("one", "deferred"), "new-lease", "json")]),
        Reply {
            status: 200,
            body: json!({"success":true,"result":{"ackCount":1,"retryCount":0}}).to_string(),
            delay: Duration::from_millis(500),
        },
    ]);
    let short = Consumer::local(
        &fixture.base,
        fixture.database.as_ref().unwrap().clone(),
        Duration::from_millis(150),
    )
    .unwrap();
    assert_eq!(short.consume_once().await, Err(Failure::AckUnknown));
    assert_eq!(fixture.count().await, 1);
    let calls = fixture.server.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].persisted_at_call, 1);
    assert_eq!(calls[2].body["acks"][0]["lease_id"], "new-lease");
}
