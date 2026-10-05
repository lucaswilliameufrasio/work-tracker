//! Local-only protocol spike; no production worker, credentials or effects.
#[path = "cloudflare_feedback.rs"]
pub mod feedback;
use base64::{Engine, engine::general_purpose::STANDARD};
use puxiri::db::Database;
use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

pub const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
pub const QUEUE: &str = "abcdef0123456789abcdef0123456789";
pub const ZONE: &str = "11111111111111111111111111111111";
pub const SUBSCRIPTION: &str = "22222222222222222222222222222222";
pub const DOMAIN: &str = "send.example.invalid";
pub const TOKEN: &str = "loopback-only-queue-token";
pub const BATCH: usize = 8;

// Error Debug intentionally has no token, lease, body, address or URL.
#[derive(Debug, PartialEq)]
pub enum Failure {
    Configuration,
    Transport,
    Http(u16),
    Protocol,
    Scope,
    Database,
    Conflict,
    AckUnknown,
}

#[derive(Deserialize)]
pub struct Message {
    body: String,
    lease_id: String,
    metadata: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
struct Response {
    success: bool,
    #[serde(default)]
    errors: Vec<Value>,
    result: Value,
}
pub struct Consumer {
    client: Client,
    base: Url,
    database: Arc<Database>,
    timeout: Duration,
}
impl Consumer {
    pub fn local(base: &str, database: Arc<Database>, timeout: Duration) -> Result<Self, Failure> {
        let base = Url::parse(base).map_err(|_| Failure::Configuration)?;
        if base.scheme() != "http"
            || base.host_str() != Some("127.0.0.1")
            || base.path() != "/"
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || timeout.is_zero()
        {
            return Err(Failure::Configuration);
        }
        let client = Client::builder()
            .timeout(timeout)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| Failure::Configuration)?;
        Ok(Self {
            client,
            base,
            database,
            timeout,
        })
    }
    async fn request(&self, action: &str, body: &Value) -> Result<Value, Failure> {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "/client/v4/accounts/{ACCOUNT}/queues/{QUEUE}/messages/{action}"
        ));
        let mut response = self
            .client
            .post(url)
            .bearer_auth(TOKEN)
            .json(body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|_| Failure::Transport)?;
        if response.status().as_u16() != 200 {
            return Err(Failure::Http(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Transport)? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(Failure::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: Response = serde_json::from_slice(&bytes).map_err(|_| Failure::Protocol)?;
        if !response.success || !response.errors.is_empty() {
            return Err(Failure::Protocol);
        }
        Ok(response.result)
    }
    pub async fn pull(&self) -> Result<Vec<Message>, Failure> {
        let value = self
            .request(
                "pull",
                &json!({"batch_size":BATCH,"visibility_timeout_ms":30000}),
            )
            .await?;
        let messages: Vec<Message> =
            serde_json::from_value(value.get("messages").ok_or(Failure::Protocol)?.clone())
                .map_err(|_| Failure::Protocol)?;
        if messages.len() > BATCH {
            return Err(Failure::Protocol);
        }
        Ok(messages)
    }
    pub async fn persist(&self, messages: &[Message]) -> Result<(), Failure> {
        let mut leases = HashSet::new();
        let mut events = Vec::with_capacity(messages.len());
        for message in messages {
            if message.lease_id.is_empty()
                || message.lease_id.len() > 4096
                || !leases.insert(&message.lease_id)
            {
                return Err(Failure::Protocol);
            }
            let content_type = message
                .metadata
                .get("CF-Content-Type")
                .and_then(Value::as_str)
                .ok_or(Failure::Protocol)?;
            if message.body.len() > 256 * 1024 {
                return Err(Failure::Protocol);
            }
            let bytes = match content_type {
                "json" => STANDARD
                    .decode(&message.body)
                    .map_err(|_| Failure::Protocol)?,
                "text" => message.body.as_bytes().to_vec(),
                // Don't guess content format for v8/bytes or absent metadata.
                _ => return Err(Failure::Protocol),
            };
            if bytes.len() > 128 * 1024 {
                return Err(Failure::Protocol);
            }
            let event: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Protocol)?;
            if event.pointer("/source/type").and_then(Value::as_str) != Some("email.sending")
                || event.pointer("/source/zoneId").and_then(Value::as_str) != Some(ZONE)
                || event.pointer("/source/domain").and_then(Value::as_str) != Some(DOMAIN)
                || event.pointer("/metadata/accountId").and_then(Value::as_str) != Some(ACCOUNT)
                || event
                    .pointer("/metadata/eventSubscriptionId")
                    .and_then(Value::as_str)
                    != Some(SUBSCRIPTION)
                || event
                    .pointer("/metadata/eventSchemaVersion")
                    .and_then(Value::as_u64)
                    != Some(1)
            {
                return Err(Failure::Scope);
            }
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .ok_or(Failure::Protocol)?;
            if !kind.starts_with("cf.email.sending.message.") {
                return Err(Failure::Protocol);
            }
            let id = event
                .pointer("/payload/eventId")
                .and_then(Value::as_str)
                .ok_or(Failure::Protocol)?;
            let message_id = event
                .pointer("/payload/messageId")
                .and_then(Value::as_str)
                .ok_or(Failure::Protocol)?;
            if id.is_empty()
                || id.len() > 256
                || id.chars().any(char::is_control)
                || message_id.is_empty()
            {
                return Err(Failure::Protocol);
            }
            // Unknown sending kinds are inbox-only; they cannot mutate users or
            // suppression. No trust in queue message id/attempts as event identity.
            events.push((
                id.to_owned(),
                serde_json::to_vec(&event).map_err(|_| Failure::Protocol)?,
            ));
        }
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|_| Failure::Database)?;
        for (id, json) in events {
            sqlx::query("INSERT INTO cloudflare_inbox(config,event_id,event_json) VALUES(?,?,?) ON CONFLICT DO NOTHING")
                .bind(format!("{ACCOUNT}/{QUEUE}/{SUBSCRIPTION}")).bind(&id).bind(&json).execute(&mut *transaction).await.map_err(|_|Failure::Database)?;
            let stored: Vec<u8> = sqlx::query_scalar(
                "SELECT event_json FROM cloudflare_inbox WHERE config=? AND event_id=?",
            )
            .bind(format!("{ACCOUNT}/{QUEUE}/{SUBSCRIPTION}"))
            .bind(&id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| Failure::Database)?;
            if stored != json {
                return Err(Failure::Conflict);
            }
            feedback::apply(
                &mut transaction,
                &format!("{ACCOUNT}/{QUEUE}/{SUBSCRIPTION}"),
                &id,
                &json,
            )
            .await
            .map_err(|_| Failure::Database)?;
        }
        transaction.commit().await.map_err(|_| Failure::Database)?;
        Ok(())
    }
    // Kept private: a caller cannot acknowledge an arbitrary unpersisted batch.
    async fn acknowledge(&self, messages: &[Message]) -> Result<(), Failure> {
        let acks: Vec<Value> = messages
            .iter()
            .map(|message| json!({"lease_id":message.lease_id}))
            .collect();
        let result = self
            .request("ack", &json!({"acks":acks,"retries":[]}))
            .await
            .map_err(|_| Failure::AckUnknown)?;
        let warnings = result.get("warnings").and_then(Value::as_object);
        if result.get("ackCount").and_then(Value::as_u64) != Some(messages.len() as u64)
            || result.get("retryCount").and_then(Value::as_u64) != Some(0)
            || warnings.is_some_and(|warnings| !warnings.is_empty())
            || result.get("warnings").is_some() && warnings.is_none()
        {
            return Err(Failure::AckUnknown);
        }
        Ok(())
    }
    pub async fn consume_once(&self) -> Result<usize, Failure> {
        let messages = self.pull().await?;
        if messages.is_empty() {
            return Ok(0);
        }
        self.persist(&messages).await?;
        self.acknowledge(&messages).await?;
        Ok(messages.len())
    }
}
pub async fn schema(database: &Database) {
    feedback::schema(database).await;
    sqlx::query("CREATE TABLE IF NOT EXISTS cloudflare_inbox(config TEXT NOT NULL,event_id TEXT NOT NULL,event_json BLOB NOT NULL,PRIMARY KEY(config,event_id))")
        .execute(database.pool()).await.unwrap();
}
