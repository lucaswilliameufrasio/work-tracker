//! Local-only SNS verification experiment. Certificate TLS transport is NOT
//! proven: a fixed loopback resolver stands in for the validated AWS origin.
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    hash::MessageDigest,
    sign::Verifier,
    stack::Stack,
    x509::{X509, X509StoreContext, store::X509StoreBuilder, verify::X509VerifyParam},
};
use puxiri::db::Database;
use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const BODY_LIMIT: usize = 1024 * 1024;
const CERT_LIMIT: usize = 16384;
const MAX_AGE: i64 = 31 * 86400; // fixture, not finalized provider replay policy

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Envelope {
    pub message: String,
    pub message_id: String,
    pub subject: Option<String>,
    pub timestamp: String,
    pub topic_arn: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub signature_version: String,
    pub signature: String,
    #[serde(rename = "SigningCertURL")]
    pub cert_url: String,
    #[serde(rename = "SubscribeURL")]
    pub subscribe_url: Option<String>,
    pub token: Option<String>,
}
pub fn canonical(envelope: &Envelope) -> Option<Vec<u8>> {
    let mut fields = vec![
        ("Message", envelope.message.as_str()),
        ("MessageId", envelope.message_id.as_str()),
    ];
    match envelope.kind.as_str() {
        "Notification" => {
            if let Some(subject) = &envelope.subject {
                fields.push(("Subject", subject));
            }
        }
        "SubscriptionConfirmation" | "UnsubscribeConfirmation" => {
            fields.push(("SubscribeURL", envelope.subscribe_url.as_deref()?));
        }
        _ => return None,
    }
    fields.push(("Timestamp", &envelope.timestamp));
    if envelope.kind != "Notification" {
        fields.push(("Token", envelope.token.as_deref()?));
    }
    fields.extend([
        ("TopicArn", envelope.topic_arn.as_str()),
        ("Type", envelope.kind.as_str()),
    ]);
    let mut bytes = Vec::new();
    for (name, value) in fields {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(b'\n');
    }
    Some(bytes)
}

pub fn certificate_path(raw: &str, host: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || url.host_str()? != host
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path();
    let hash = path
        .strip_prefix("/SimpleNotificationService-")?
        .strip_suffix(".pem")?;
    if hash.len() != 32 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Reject URL parser normalization, percent encodings and explicit ports.
    if raw != format!("https://{host}{path}") {
        return None;
    }
    Some(path.to_owned())
}

fn verify_certificate(pem: &[u8], trusted_root: &X509, host: &str, now: i64) -> Option<X509> {
    let certificates = X509::stack_from_pem(pem).ok()?;
    if certificates.len() != 1 {
        return None;
    }
    let cert = certificates.into_iter().next()?;
    let mut store = X509StoreBuilder::new().ok()?;
    store.add_cert(trusted_root.clone()).ok()?;
    let mut params = X509VerifyParam::new().ok()?;
    params.set_host(host).ok()?;
    params.set_time(now);
    store.set_param(&params).ok()?;
    let store = store.build();
    let chain = Stack::new().ok()?;
    let mut context = X509StoreContext::new().ok()?;
    if !context
        .init(&store, &cert, &chain, |context| context.verify_cert())
        .ok()?
    {
        return None;
    }
    let key = cert.public_key().ok()?;
    if key.rsa().ok()?.size() < 256 {
        return None;
    }
    Some(cert)
}

pub struct Endpoint {
    database: Arc<Database>,
    topic: String,
    host: String,
    root: X509,
    certificate_base: Url,
    client: Client,
    now: i64,
}
impl Endpoint {
    pub fn local(
        database: Arc<Database>,
        topic: &str,
        root: X509,
        certificate_base: &str,
        now: i64,
    ) -> Self {
        let base = Url::parse(certificate_base).unwrap();
        assert_eq!(base.host_str(), Some("127.0.0.1"));
        assert_eq!(base.scheme(), "http");
        assert_eq!(base.path(), "/");
        assert!(base.query().is_none());
        assert!(topic.starts_with("arn:aws:sns:us-east-1:123456789012:"));
        Self {
            database,
            topic: topic.into(),
            host: "sns.us-east-1.amazonaws.com".into(),
            root,
            certificate_base: base,
            now,
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .build()
                .unwrap(),
        }
    }
    async fn authenticate(&self, envelope: &Envelope) -> Result<Vec<u8>, StatusCode> {
        if envelope.topic_arn != self.topic
            || envelope.message_id.is_empty()
            || envelope.message_id.len() > 256
            || envelope.message_id.chars().any(char::is_control)
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let hash = match envelope.signature_version.as_str() {
            "1" => MessageDigest::sha1(),
            "2" => MessageDigest::sha256(),
            _ => return Err(StatusCode::UNAUTHORIZED),
        };
        let canonical = canonical(envelope).ok_or(StatusCode::BAD_REQUEST)?;
        let timestamp = OffsetDateTime::parse(&envelope.timestamp, &Rfc3339)
            .map_err(|_| StatusCode::BAD_REQUEST)?
            .unix_timestamp();
        if !self
            .now
            .checked_sub(timestamp)
            .is_some_and(|age| (-300..=MAX_AGE).contains(&age))
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let path =
            certificate_path(&envelope.cert_url, &self.host).ok_or(StatusCode::UNAUTHORIZED)?;
        if envelope.signature.len() > 1024 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let signature = STANDARD
            .decode(&envelope.signature)
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        // Never fetch the raw payload URL; mapping is a local test resolver only.
        let mut target = self.certificate_base.clone();
        target.set_path(&path);
        let mut response = self
            .client
            .get(target)
            .send()
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if response.status().as_u16() != 200 {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let mut pem = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            if pem.len() + chunk.len() > CERT_LIMIT {
                return Err(StatusCode::UNAUTHORIZED);
            }
            pem.extend_from_slice(&chunk);
        }
        let certificate = verify_certificate(&pem, &self.root, &self.host, self.now)
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let key = certificate
            .public_key()
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let mut verifier = Verifier::new(hash, &key).map_err(|_| StatusCode::UNAUTHORIZED)?;
        verifier
            .update(&canonical)
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        if !verifier.verify(&signature).unwrap_or(false) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(canonical)
    }
}
pub async fn schema(database: &Database) {
    sqlx::query("CREATE TABLE IF NOT EXISTS sns_inbox(topic TEXT NOT NULL,message_id TEXT NOT NULL,canonical BLOB NOT NULL,PRIMARY KEY(topic,message_id))")
        .execute(database.pool()).await.unwrap();
}
pub fn router(endpoint: Endpoint) -> Router {
    Router::new()
        .route("/callbacks/ses-sns", post(receive))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(endpoint))
}
async fn receive(State(endpoint): State<Arc<Endpoint>>, body: Bytes) -> StatusCode {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let canonical = match endpoint.authenticate(&envelope).await {
        Ok(bytes) => bytes,
        Err(status) => return status,
    };
    // Confirmation is verified but never follows SubscribeURL/UnsubscribeURL or
    // calls ConfirmSubscription. Administrator activation remains separate.
    if envelope.kind != "Notification" {
        return StatusCode::ACCEPTED;
    }
    let Ok(mut transaction) = endpoint.database.pool().begin_with("BEGIN IMMEDIATE").await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if sqlx::query(
        "INSERT INTO sns_inbox(topic,message_id,canonical) VALUES(?,?,?) ON CONFLICT DO NOTHING",
    )
    .bind(&endpoint.topic)
    .bind(&envelope.message_id)
    .bind(&canonical)
    .execute(&mut *transaction)
    .await
    .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let stored: Result<Vec<u8>, _> =
        sqlx::query_scalar("SELECT canonical FROM sns_inbox WHERE topic=? AND message_id=?")
            .bind(&endpoint.topic)
            .bind(&envelope.message_id)
            .fetch_one(&mut *transaction)
            .await;
    let Ok(stored) = stored else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if stored != canonical {
        return StatusCode::CONFLICT;
    }
    if transaction.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::NO_CONTENT
}
