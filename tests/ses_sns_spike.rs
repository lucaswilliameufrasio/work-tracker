//! SNS protocol/crypto fixtures with real HTTP/SQLite, no AWS calls.
#[path = "support/sns_contract.rs"]
mod sns;
use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode},
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    hash::MessageDigest,
    pkey::{PKey, Private},
    rsa::Rsa,
    sign::Signer,
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, KeyUsage, SubjectAlternativeName},
    },
};
use puxiri::db::Database;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const TOPIC: &str = "arn:aws:sns:us-east-1:123456789012:puxiri-fixture";
const PATH: &str = "/SimpleNotificationService-0123456789abcdef0123456789abcdef.pem";
const HOST: &str = "sns.us-east-1.amazonaws.com";
struct Certificates {
    root: X509,
    leaf: X509,
    key: PKey<Private>,
}
fn certificates(host: &str, expired: bool) -> Certificates {
    let root_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let leaf_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "Puxiri artificial root")
        .unwrap();
    let name = name.build();
    let mut root = X509::builder().unwrap();
    root.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(128, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();
    root.set_serial_number(&serial).unwrap();
    root.set_subject_name(&name).unwrap();
    root.set_issuer_name(&name).unwrap();
    root.set_pubkey(&root_key).unwrap();
    root.set_not_before(&Asn1Time::from_unix(NOW - 86400).unwrap())
        .unwrap();
    root.set_not_after(&Asn1Time::from_unix(NOW + 86400).unwrap())
        .unwrap();
    root.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    root.append_extension(
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()
            .unwrap(),
    )
    .unwrap();
    root.sign(&root_key, MessageDigest::sha256()).unwrap();
    let root = root.build();
    let mut leaf = X509::builder().unwrap();
    leaf.set_version(2).unwrap();
    leaf.set_serial_number(&serial).unwrap();
    let mut subject = X509NameBuilder::new().unwrap();
    subject.append_entry_by_text("CN", host).unwrap();
    let subject = subject.build();
    leaf.set_subject_name(&subject).unwrap();
    leaf.set_issuer_name(root.subject_name()).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    leaf.set_not_before(&Asn1Time::from_unix(NOW - 86400).unwrap())
        .unwrap();
    leaf.set_not_after(&Asn1Time::from_unix(if expired { NOW - 1 } else { NOW + 3600 }).unwrap())
        .unwrap();
    leaf.append_extension(BasicConstraints::new().critical().build().unwrap())
        .unwrap();
    leaf.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .build()
            .unwrap(),
    )
    .unwrap();
    let san = SubjectAlternativeName::new()
        .dns(host)
        .build(&leaf.x509v3_context(Some(&root), None))
        .unwrap();
    leaf.append_extension(san).unwrap();
    leaf.sign(&root_key, MessageDigest::sha256()).unwrap();
    Certificates {
        root,
        leaf: leaf.build(),
        key: leaf_key,
    }
}
struct Fixture {
    _directory: TempDir,
    database: Arc<Database>,
    key: PKey<Private>,
    base: String,
    hits: Arc<AtomicUsize>,
    certificate_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.certificate_task.abort();
        self.server_task.abort();
    }
}
impl Fixture {
    async fn new(
        certificates: Certificates,
        trust: Option<X509>,
        status: u16,
        body: Option<Vec<u8>>,
    ) -> Self {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(Database::open(directory.path()).await.unwrap());
        sns::schema(&database).await;
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let pem = body.unwrap_or_else(|| certificates.leaf.to_pem().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cert_base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            PATH,
            get(move || {
                let count = count.clone();
                let pem = pem.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .status(StatusCode::from_u16(status).unwrap())
                        .header("location", "http://127.0.0.1:1/no-follow")
                        .body(Body::from(pem))
                        .unwrap()
                }
            }),
        );
        let certificate_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let endpoint = sns::Endpoint::local(
            database.clone(),
            TOPIC,
            trust.unwrap_or(certificates.root),
            &cert_base,
            NOW,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = sns::router(endpoint);
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _directory: directory,
            database,
            key: certificates.key,
            base,
            hits,
            certificate_task,
            server_task,
        }
    }
    async fn post(&self, envelope: &Value) -> u16 {
        self.raw(serde_json::to_vec(envelope).unwrap()).await
    }
    async fn raw(&self, body: Vec<u8>) -> u16 {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(4))
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .post(format!("{}/callbacks/ses-sns", self.base))
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM sns_inbox")
            .fetch_one(self.database.pool())
            .await
            .unwrap()
    }
    fn signed(&self, mut envelope: Value, version: &str) -> Value {
        envelope["SignatureVersion"] = json!(version);
        envelope["Signature"] = json!("");
        let parsed: sns::Envelope = serde_json::from_value(envelope.clone()).unwrap();
        let canonical = sns::canonical(&parsed).unwrap();
        let mut signer = Signer::new(
            if version == "1" {
                MessageDigest::sha1()
            } else {
                MessageDigest::sha256()
            },
            &self.key,
        )
        .unwrap();
        signer.update(&canonical).unwrap();
        envelope["Signature"] = json!(STANDARD.encode(signer.sign_to_vec().unwrap()));
        envelope
    }
}
fn envelope() -> Value {
    json!({"Type":"Notification","MessageId":"fixture-message-id","Message":"{\"eventType\":\"Bounce\",\"mail\":{\"messageId\":\"fixture-ses-id\"}}",
    "Timestamp":"2027-01-15T08:00:00Z","TopicArn":TOPIC,"SignatureVersion":"2","Signature":"","SigningCertURL":format!("https://{HOST}{PATH}")})
}

const RECIPIENT: &str = "recipient@example.invalid";
async fn registered(fixture: &Fixture, topic: &str, id: &str, recipient: &str) {
    sqlx::query("INSERT INTO ses_feedback_sends(topic,ses_id,recipient) VALUES(?,?,?)")
        .bind(topic)
        .bind(id)
        .bind(recipient)
        .execute(fixture.database.pool())
        .await
        .unwrap();
}
fn feedback_event(kind: &str) -> Value {
    let mut value = json!({"notificationType":kind,"mail":{"sendingAccountId":"123456789012","messageId":"fixture-ses-id","destination":[RECIPIENT]},
        "complaint":{"complainedRecipients":[{"emailAddress":RECIPIENT}],"complaintFeedbackType":"abuse","feedbackId":"complaint-feedback"},
        "bounce":{"bounceType":"Permanent","bounceSubType":"General","feedbackId":"bounce-feedback","bouncedRecipients":[{"emailAddress":RECIPIENT,"action":"failed","status":"5.1.1","diagnosticCode":"PRIVATE"}]}});
    if kind != "Complaint" {
        value.as_object_mut().unwrap().remove("complaint");
    }
    if kind != "Bounce" {
        value.as_object_mut().unwrap().remove("bounce");
    }
    value
}
async fn post_feedback(fixture: &Fixture, id: &str, event: Value) -> u16 {
    let mut value = envelope();
    value["MessageId"] = json!(id);
    value["Message"] = json!(event.to_string());
    fixture.post(&fixture.signed(value, "2")).await
}
async fn effects(fixture: &Fixture) -> (i64, i64) {
    (
        sqlx::query_scalar("SELECT count(*) FROM ses_feedback_suppression")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM ses_feedback_audit")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn correlated_complaint_is_atomic_deduplicated_and_never_bans_or_clears_on_delivery() {
    let mut fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    sqlx::raw_sql("CREATE TABLE fixture_users(id TEXT PRIMARY KEY,banned INTEGER); INSERT INTO fixture_users VALUES('user',0);
        CREATE TABLE fixture_bell(id TEXT PRIMARY KEY); INSERT INTO fixture_bell VALUES('notification');")
        .execute(fixture.database.pool()).await.unwrap();
    let event = feedback_event("Complaint");
    let (a, b) = tokio::join!(
        post_feedback(&fixture, "complaint", event.clone()),
        post_feedback(&fixture, "complaint", event.clone())
    );
    assert_eq!((a, b), (204, 204));
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(post_feedback(&fixture, "republished", event).await, 204);
    assert_eq!(
        post_feedback(&fixture, "late-delivery", feedback_event("Delivery")).await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT banned FROM fixture_users")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fixture_bell")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        1
    );
    let audit: Vec<(String, String, String)> =
        sqlx::query_as("SELECT feedback_id,ses_id,reason FROM ses_feedback_audit")
            .fetch_all(fixture.database.pool())
            .await
            .unwrap();
    assert!(!format!("{audit:?}").contains(RECIPIENT));
    assert!(!format!("{audit:?}").contains("PRIVATE"));
    fixture.server_task.abort();
    let _ = (&mut fixture.server_task).await;
    fixture.database.close().await;
    let directory = std::mem::replace(&mut fixture._directory, TempDir::new().unwrap());
    drop(fixture); // Release application state and its directory ownership lock.
    let reopened = Database::open(directory.path()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ses_feedback_suppression")
            .fetch_one(reopened.pool())
            .await
            .unwrap(),
        1
    );
    reopened.close().await;
}

#[tokio::test]
async fn only_explicit_permanent_invalid_status_suppresses_bounce_and_complaint_takes_precedence() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    for (index, (kind, subtype, status, action)) in [
        ("Transient", "MailboxFull", "4.2.2", "delayed"),
        ("Transient", "General", "5.1.1", "failed"),
        ("Permanent", "General", "5.7.1", "failed"),
        ("Permanent", "Suppressed", "5.1.1", "failed"),
        ("Undetermined", "Undetermined", "5.1.1", "failed"),
        ("Permanent", "General", "5.1.1", "delayed"),
        ("Permanent", "General", "", "failed"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut event = feedback_event("Bounce");
        event["bounce"]["bounceType"] = json!(kind);
        event["bounce"]["bounceSubType"] = json!(subtype);
        event["bounce"]["bouncedRecipients"][0]["status"] = json!(status);
        event["bounce"]["bouncedRecipients"][0]["action"] = json!(action);
        assert_eq!(
            post_feedback(&fixture, &format!("non-invalid-{index}"), event).await,
            204
        );
    }
    assert_eq!(effects(&fixture).await, (0, 0));
    for (index,recipients) in [json!([{ "emailAddress":"other@example.invalid","action":"failed","status":"5.1.1"}]),
        json!([{ "emailAddress":RECIPIENT,"action":"failed","status":"5.1.1"},{"emailAddress":"other@example.invalid"}])].into_iter().enumerate() {
        let mut event=feedback_event("Bounce");event["bounce"]["bouncedRecipients"]=recipients;
        assert_eq!(post_feedback(&fixture,&format!("bounce-recipient-{index}"),event).await,204);
    }
    assert_eq!(effects(&fixture).await, (0, 0));
    assert_eq!(
        post_feedback(&fixture, "invalid", feedback_event("Bounce")).await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(
        post_feedback(&fixture, "complaint", feedback_event("Complaint")).await,
        204
    );
    let mut bounce = feedback_event("Bounce");
    bounce["bounce"]["feedbackId"] = json!("another-bounce");
    assert_eq!(
        post_feedback(&fixture, "after-complaint", bounce).await,
        204
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT reason FROM ses_feedback_suppression")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        "complaint"
    );
    fixture.database.close().await;
}

#[tokio::test]
async fn ambiguous_complaint_candidates_and_non_abuse_feedback_have_no_effect() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    for (index, kind) in ["not-spam", "auth-failure", "virus", "other", ""]
        .into_iter()
        .enumerate()
    {
        let mut event = feedback_event("Complaint");
        event["complaint"]["complaintFeedbackType"] = json!(kind);
        assert_eq!(
            post_feedback(&fixture, &format!("not-abuse-{index}"), event).await,
            204
        );
    }
    let mut multiple = feedback_event("Complaint");
    multiple["complaint"]["complainedRecipients"] =
        json!([{"emailAddress":RECIPIENT},{"emailAddress":"other@example.invalid"}]);
    assert_eq!(
        post_feedback(&fixture, "multiple-candidates", multiple).await,
        204
    );
    let mut destination = feedback_event("Complaint");
    destination["mail"]["destination"] = json!([RECIPIENT, "other@example.invalid"]);
    assert_eq!(
        post_feedback(&fixture, "multiple-destination", destination).await,
        204
    );
    let mut missing = feedback_event("Complaint");
    missing["complaint"]
        .as_object_mut()
        .unwrap()
        .remove("complaintFeedbackType");
    assert_eq!(
        post_feedback(&fixture, "missing-feedback-type", missing).await,
        204
    );
    let mut missing = feedback_event("Complaint");
    missing["complaint"]
        .as_object_mut()
        .unwrap()
        .remove("feedbackId");
    assert_eq!(
        post_feedback(&fixture, "missing-feedback-id", missing).await,
        204
    );
    let mut oversized = feedback_event("Complaint");
    oversized["complaint"]["feedbackId"] = json!("x".repeat(257));
    assert_eq!(
        post_feedback(&fixture, "oversized-feedback-id", oversized).await,
        204
    );
    assert_eq!(effects(&fixture).await, (0, 0));
    let mut fraud = feedback_event("Complaint");
    fraud["complaint"]["complaintFeedbackType"] = json!("fraud");
    assert_eq!(post_feedback(&fixture, "fraud", fraud).await, 204);
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.database.close().await;
}

#[tokio::test]
async fn account_topic_message_and_recipient_must_match_before_projection_and_early_callback_can_replay()
 {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, "another-topic", "fixture-ses-id", RECIPIENT).await;
    assert_eq!(
        post_feedback(&fixture, "early", feedback_event("Complaint")).await,
        204
    );
    assert_eq!(effects(&fixture).await, (0, 0));
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    let mut account = feedback_event("Complaint");
    account["mail"]["sendingAccountId"] = json!("999999999999");
    assert_eq!(post_feedback(&fixture, "wrong-account", account).await, 204);
    let mut recipient = feedback_event("Complaint");
    recipient["complaint"]["complainedRecipients"][0]["emailAddress"] =
        json!("other@example.invalid");
    assert_eq!(
        post_feedback(&fixture, "wrong-recipient", recipient).await,
        204
    );
    let mut id = feedback_event("Complaint");
    id["mail"]["messageId"] = json!("unknown");
    assert_eq!(post_feedback(&fixture, "unknown-id", id).await, 204);
    let mut missing = feedback_event("Complaint");
    missing["mail"].as_object_mut().unwrap().remove("messageId");
    assert_eq!(post_feedback(&fixture, "missing-id", missing).await, 204);
    let mut destination = feedback_event("Complaint");
    destination["mail"]["destination"] = json!(["other@example.invalid"]);
    assert_eq!(
        post_feedback(&fixture, "wrong-destination", destination).await,
        204
    );
    let mut ambiguous = feedback_event("Complaint");
    ambiguous["eventType"] = json!("Bounce");
    assert_eq!(post_feedback(&fixture, "ambiguous", ambiguous).await, 204);
    assert_eq!(effects(&fixture).await, (0, 0));
    assert_eq!(
        post_feedback(&fixture, "early", feedback_event("Complaint")).await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.database.close().await;
}

#[tokio::test]
async fn tampered_or_malformed_message_never_authorizes_address_effects() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    let mut value = envelope();
    value["Message"] = json!(feedback_event("Complaint").to_string());
    let mut signed = fixture.signed(value, "2");
    signed["Message"] = json!(feedback_event("Bounce").to_string());
    assert_eq!(fixture.post(&signed).await, 401);
    let mut malformed = envelope();
    malformed["Message"] = json!("not-json");
    assert_eq!(fixture.post(&fixture.signed(malformed, "2")).await, 204);
    let mut publishing = feedback_event("Bounce");
    publishing
        .as_object_mut()
        .unwrap()
        .remove("notificationType");
    publishing["eventType"] = json!("Bounce");
    assert_eq!(
        post_feedback(&fixture, "event-publishing", publishing).await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(fixture.count().await, 2);
    fixture.database.close().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_inbox_suppression_and_projection_before_ack() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    sqlx::raw_sql("CREATE TRIGGER fail_feedback BEFORE INSERT ON ses_feedback_audit BEGIN SELECT RAISE(ABORT,'fixture audit failure'); END")
        .execute(fixture.database.pool()).await.unwrap();
    assert_eq!(
        post_feedback(&fixture, "atomic", feedback_event("Complaint")).await,
        503
    );
    assert_eq!(fixture.count().await, 0);
    assert_eq!(effects(&fixture).await, (0, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ses_feedback_projection")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER fail_feedback")
        .execute(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(
        post_feedback(&fixture, "atomic", feedback_event("Complaint")).await,
        204
    );
    assert_eq!(effects(&fixture).await, (1, 1));
    fixture.database.close().await;
}

#[tokio::test]
async fn conflicting_feedback_identity_cannot_mutate_a_second_address_without_audit() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    registered(&fixture, TOPIC, "fixture-ses-id", RECIPIENT).await;
    registered(&fixture, TOPIC, "second-ses-id", "other@example.invalid").await;
    assert_eq!(
        post_feedback(&fixture, "first", feedback_event("Complaint")).await,
        204
    );
    let mut conflict = feedback_event("Complaint");
    conflict["mail"]["messageId"] = json!("second-ses-id");
    conflict["mail"]["destination"] = json!(["other@example.invalid"]);
    conflict["complaint"]["complainedRecipients"][0]["emailAddress"] =
        json!("other@example.invalid");
    assert_eq!(post_feedback(&fixture, "second", conflict).await, 503);
    assert_eq!(effects(&fixture).await, (1, 1));
    assert_eq!(fixture.count().await, 1);
    fixture.database.close().await;
}

#[test]
fn canonical_fields_match_aws_order_and_unescape_json_with_one_final_newline() {
    let mut value = envelope();
    value["Message"] = json!("first\nsecond é");
    value["Subject"] = json!("subject");
    let parsed: sns::Envelope = serde_json::from_value(value).unwrap();
    assert_eq!(
        String::from_utf8(sns::canonical(&parsed).unwrap()).unwrap(),
        format!(
            "Message\nfirst\nsecond é\nMessageId\nfixture-message-id\nSubject\nsubject\nTimestamp\n2027-01-15T08:00:00Z\nTopicArn\n{TOPIC}\nType\nNotification\n"
        )
    );
    let mut confirm = envelope();
    confirm["Type"] = json!("SubscriptionConfirmation");
    confirm["SubscribeURL"] = json!("https://example.invalid/confirm");
    confirm["Token"] = json!("fixture-token");
    let parsed: sns::Envelope = serde_json::from_value(confirm).unwrap();
    let canonical = String::from_utf8(sns::canonical(&parsed).unwrap()).unwrap();
    assert!(canonical.contains("SubscribeURL\nhttps://example.invalid/confirm\nTimestamp\n"));
    assert!(canonical.contains(&format!(
        "Token\nfixture-token\nTopicArn\n{TOPIC}\nType\nSubscriptionConfirmation\n"
    )));
}

#[test]
fn certificate_origin_policy_rejects_ssrf_normalization_and_region_confusion() {
    assert!(sns::certificate_path(&format!("https://{HOST}{PATH}"), HOST).is_some());
    for raw in [
        format!("http://{HOST}{PATH}"),
        format!("https://{HOST}.evil.invalid{PATH}"),
        format!("https://{HOST}@127.0.0.1{PATH}"),
        format!("https://{HOST}:443{PATH}"),
        format!("https://sns.eu-west-1.amazonaws.com{PATH}"),
        format!("https://{HOST}{PATH}?token=x"),
        format!("https://{HOST}{PATH}#fragment"),
        format!("https://{HOST}/dir/..{PATH}"),
        format!("https://{HOST}/%53impleNotificationService-0123456789abcdef0123456789abcdef.pem"),
        "https://169.254.169.254/latest/meta-data".into(),
        format!("https://{HOST}/other.pem"),
    ] {
        assert!(sns::certificate_path(&raw, HOST).is_none(), "{raw}");
    }
}

#[tokio::test]
async fn valid_v1_v2_notifications_authenticate_and_deduplicate_before_ack() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    for version in ["1", "2"] {
        let signed = fixture.signed(envelope(), version);
        assert_eq!(fixture.post(&signed).await, 204);
    }
    assert_eq!(fixture.count().await, 1);
    let mut subject = envelope();
    subject["MessageId"] = json!("subject-message");
    subject["Subject"] = json!("subject é\nsecond line");
    assert_eq!(fixture.post(&fixture.signed(subject, "2")).await, 204);
    assert_eq!(fixture.count().await, 2);
    let mut wrong_version = fixture.signed(envelope(), "2");
    wrong_version["SignatureVersion"] = json!("1");
    assert_eq!(fixture.post(&wrong_version).await, 401);
    let mut modified = fixture.signed(envelope(), "2");
    modified["Message"] = json!("changed after signing");
    assert_eq!(fixture.post(&modified).await, 401);
    assert_eq!(fixture.count().await, 2);
    fixture.database.close().await;
}

#[tokio::test]
async fn unexpected_topic_version_cert_origin_and_timestamp_do_not_fetch_or_write() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    for (field, value) in [
        ("TopicArn", "arn:aws:sns:us-east-1:999999999999:other"),
        ("SignatureVersion", "3"),
        ("SigningCertURL", "http://169.254.169.254/latest/meta-data"),
        ("Timestamp", "not-a-date"),
        ("Timestamp", "2099-01-01T00:00:00Z"),
        ("Signature", "not-base64!"),
    ] {
        let mut signed = fixture.signed(envelope(), "2");
        signed[field] = json!(value);
        assert!([400, 401].contains(&fixture.post(&signed).await));
    }
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.count().await, 0);
    fixture.database.close().await;
}

#[tokio::test]
async fn certificate_trust_expiry_hostname_and_redirect_or_bad_body_fail_closed() {
    let unrelated_root = certificates(HOST, false).root;
    let fixtures = [
        Fixture::new(certificates(HOST, false), Some(unrelated_root), 200, None).await,
        Fixture::new(certificates(HOST, true), None, 200, None).await,
        Fixture::new(
            certificates("wrong.example.invalid", false),
            None,
            200,
            None,
        )
        .await,
        Fixture::new(certificates(HOST, false), None, 302, None).await,
        Fixture::new(certificates(HOST, false), None, 503, None).await,
        Fixture::new(
            certificates(HOST, false),
            None,
            200,
            Some(b"not a certificate".to_vec()),
        )
        .await,
        Fixture::new(
            certificates(HOST, false),
            None,
            200,
            Some(vec![b'x'; 16385]),
        )
        .await,
    ];
    for fixture in fixtures {
        assert!([401, 503].contains(&fixture.post(&fixture.signed(envelope(), "2")).await));
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.count().await, 0);
        fixture.database.close().await;
    }
}

#[tokio::test]
async fn confirmation_is_verified_without_following_payload_url_or_mutating_inbox() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    for kind in ["SubscriptionConfirmation", "UnsubscribeConfirmation"] {
        let mut value = envelope();
        value["Type"] = json!(kind);
        value["SubscribeURL"] = json!("http://127.0.0.1:1/never-follow");
        value["Token"] = json!("artificial-token");
        let signed = fixture.signed(value, "2");
        assert_eq!(fixture.post(&signed).await, 202);
        let mut modified = signed;
        modified["Token"] = json!("modified");
        assert_eq!(fixture.post(&modified).await, 401);
    }
    assert_eq!(fixture.count().await, 0);
    fixture.database.close().await;
}

#[tokio::test]
async fn concurrent_replay_conflicting_ids_and_write_failure_keep_atomic_inbox() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    let signed = fixture.signed(envelope(), "2");
    let (a, b) = tokio::join!(fixture.post(&signed), fixture.post(&signed));
    assert_eq!((a, b), (204, 204));
    let mut changed = envelope();
    changed["Message"] = json!("legitimately signed but conflicting");
    assert_eq!(fixture.post(&fixture.signed(changed, "2")).await, 409);
    assert_eq!(fixture.count().await, 1);
    sqlx::raw_sql("CREATE TRIGGER fail_inbox BEFORE INSERT ON sns_inbox BEGIN SELECT RAISE(ABORT,'fixture write failure'); END").execute(fixture.database.pool()).await.unwrap();
    let mut next = envelope();
    next["MessageId"] = json!("new-event");
    let signed = fixture.signed(next, "2");
    assert_eq!(fixture.post(&signed).await, 503);
    assert_eq!(fixture.count().await, 1);
    sqlx::query("DROP TRIGGER fail_inbox")
        .execute(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(fixture.post(&signed).await, 204);
    assert_eq!(fixture.count().await, 2);
    fixture.database.close().await;
}

#[tokio::test]
async fn malformed_duplicate_fields_and_oversized_envelopes_do_not_resolve_certificates() {
    let fixture = Fixture::new(certificates(HOST, false), None, 200, None).await;
    assert_eq!(fixture.raw(b"not-json".to_vec()).await, 400);
    let mut missing = envelope();
    missing.as_object_mut().unwrap().remove("TopicArn");
    assert_eq!(fixture.post(&missing).await, 400);
    let mut duplicate = serde_json::to_string(&envelope()).unwrap();
    duplicate.pop();
    duplicate.push_str(",\"TopicArn\":\"shadow-topic\"}");
    assert_eq!(fixture.raw(duplicate.into_bytes()).await, 400);
    assert_eq!(fixture.raw(vec![b'x'; sns::BODY_LIMIT + 1]).await, 413);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.count().await, 0);
    fixture.database.close().await;
}
