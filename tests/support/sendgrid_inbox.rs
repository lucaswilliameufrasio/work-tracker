//! Experimental SendGrid ingress only. No product route, suppression or key fetch.
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use p256::{
    ecdsa::{Signature, VerifyingKey, signature::Verifier},
    pkcs8::DecodePublicKey,
};
use puxiri::db::Database;
use serde_json::Value;
use std::sync::Arc;

pub const SIGNATURE_HEADER: &str = "x-twilio-email-event-webhook-signature";
pub const TIMESTAMP_HEADER: &str = "x-twilio-email-event-webhook-timestamp";
pub const BODY_LIMIT: usize = 1024 * 1024;
const EVENT_LIMIT: usize = 512;
// Fixture policy, NOT a provider guarantee. Retry signing timestamp behavior and
// production freshness/retention must be validated with an authorized account.
const MAX_AGE: i64 = 25 * 60 * 60;

pub fn public_key(encoded: &str) -> Result<VerifyingKey, ()> {
    if encoded.len() > 512 {
        return Err(());
    }
    let der = STANDARD.decode(encoded).map_err(|_| ())?;
    VerifyingKey::from_public_key_der(&der).map_err(|_| ())
}

pub fn verify(key: &VerifyingKey, timestamp: &str, signature: &str, body: &[u8]) -> bool {
    if timestamp.len() > 20 || signature.len() > 256 || body.len() > BODY_LIMIT {
        return false;
    }
    let Ok(der) = STANDARD.decode(signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_der(&der) else {
        return false;
    };
    let mut signed = Vec::with_capacity(timestamp.len() + body.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.extend_from_slice(body);
    // RustCrypto's Verifier applies SHA-256 once, matching SendGrid's helper.
    key.verify(&signed, &signature).is_ok()
}

struct Endpoint {
    database: Arc<Database>,
    key: VerifyingKey,
    config: String,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

pub async fn schema(database: &Database) {
    sqlx::query("CREATE TABLE IF NOT EXISTS sendgrid_inbox (config TEXT NOT NULL, event_id TEXT NOT NULL, event_json BLOB NOT NULL, PRIMARY KEY(config,event_id))")
        .execute(database.pool()).await.unwrap();
}

pub fn router(
    database: Arc<Database>,
    key: VerifyingKey,
    config: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> Router {
    Router::new()
        .route("/callbacks/sendgrid", post(receive))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(Endpoint {
            database,
            key,
            config: config.into(),
            clock,
        }))
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    if headers.get_all(name).iter().count() != 1 {
        return None;
    }
    headers.get(name)?.to_str().ok()
}

async fn receive(
    State(endpoint): State<Arc<Endpoint>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(timestamp) = single_header(&headers, TIMESTAMP_HEADER) else {
        return StatusCode::UNAUTHORIZED;
    };
    let Some(signature) = single_header(&headers, SIGNATURE_HEADER) else {
        return StatusCode::UNAUTHORIZED;
    };
    // Checked arithmetic and injected clock keep extremes/window tests safe.
    let age = timestamp
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .and_then(|timestamp| (endpoint.clock)().checked_sub(timestamp));
    if !age.is_some_and(|age| (-300..=MAX_AGE).contains(&age))
        || !verify(&endpoint.key, timestamp, signature, &body)
    {
        return StatusCode::UNAUTHORIZED;
    }
    // Authentication precedes JSON, lookup and every write, even duplicate ids.
    let Ok(events) = serde_json::from_slice::<Vec<Value>>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if events.is_empty() || events.len() > EVENT_LIMIT {
        return StatusCode::BAD_REQUEST;
    }
    let mut validated = Vec::with_capacity(events.len());
    for event in events {
        let Some(id) = event.get("sg_event_id").and_then(Value::as_str) else {
            return StatusCode::BAD_REQUEST;
        };
        let Some(kind) = event.get("event").and_then(Value::as_str) else {
            return StatusCode::BAD_REQUEST;
        };
        // Docs disagree on a 100-char upper bound; fixture permits 1024, not a
        // protocol assertion. Unknown kinds/missing message ids are quarantined.
        if id.is_empty()
            || id.len() > 1024
            || id.chars().any(char::is_control)
            || kind.is_empty()
            || kind.len() > 128
        {
            return StatusCode::BAD_REQUEST;
        }
        // Canonical per-event JSON allows rebatching/whitespace changes with a
        // fresh valid signature. We don't dedup by signature or entire raw batch.
        validated.push((
            id.to_owned(),
            serde_json::to_vec(&event).expect("JSON value serializes"),
        ));
    }
    let Ok(mut transaction) = endpoint.database.pool().begin_with("BEGIN IMMEDIATE").await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    for (id, json) in validated {
        if sqlx::query("INSERT INTO sendgrid_inbox(config,event_id,event_json) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(&endpoint.config).bind(&id).bind(&json).execute(&mut *transaction).await.is_err() { return StatusCode::SERVICE_UNAVAILABLE; }
        let stored: Result<Vec<u8>, _> = sqlx::query_scalar(
            "SELECT event_json FROM sendgrid_inbox WHERE config=? AND event_id=?",
        )
        .bind(&endpoint.config)
        .bind(&id)
        .fetch_one(&mut *transaction)
        .await;
        let Ok(stored) = stored else {
            return StatusCode::SERVICE_UNAVAILABLE;
        };
        if stored != json {
            return StatusCode::CONFLICT;
        }
    }
    if transaction.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::NO_CONTENT
}
