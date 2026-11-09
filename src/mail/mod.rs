use std::time::Duration;

use aws_lc_rs::aead::{AES_256_GCM, Aad, NONCE_LEN, Nonce, RandomizedNonceKey};
use reqwest::{Url, redirect::Policy};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::{master_key::MasterKey, time::format_timestamp};

const BODY_LIMIT: usize = 65_536;
const KEY_LIMIT: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProvider {
    Sabia,
    Resend,
    MillionSend,
}

impl MailProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sabia => "sabia",
            Self::Resend => "resend",
            Self::MillionSend => "millionsend",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sabia" => Some(Self::Sabia),
            "resend" => Some(Self::Resend),
            "millionsend" => Some(Self::MillionSend),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum MailError {
    #[error("mail sender is not configured")]
    NotConfigured,
    #[error("mail sender configuration was rejected")]
    InvalidConfiguration,
    #[error("the provider rejected the message")]
    Rejected,
    #[error("the provider did not confirm acceptance")]
    Unknown,
    #[error("mail configuration could not be stored")]
    Database(#[from] sqlx::Error),
    #[error("mail key protection failed")]
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailSenderStatus {
    pub provider: MailProvider,
    pub from_address: Option<String>,
    pub from_name: Option<String>,
    pub template_key: Option<String>,
    pub sabia_origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationMail {
    pub to: String,
    pub invite_url: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailReceipt {
    pub provider_message_id: String,
}

pub struct MailSenderConfiguration {
    pub provider: MailProvider,
    pub api_key: Zeroizing<String>,
    pub from_address: Option<String>,
    pub from_name: Option<String>,
    pub template_key: Option<String>,
    pub sabia_origin: Option<String>,
}

#[derive(Clone)]
pub struct MailSenderStore {
    pool: SqlitePool,
    master_key: MasterKey,
}

impl MailSenderStore {
    pub fn new(pool: SqlitePool, master_key: MasterKey) -> Self {
        Self { pool, master_key }
    }

    pub async fn status(&self) -> Result<Option<MailSenderStatus>, MailError> {
        let row = sqlx::query(
            "SELECT provider, from_address, from_name, template_key, sabia_origin \
             FROM mail_sender WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        Ok(Some(MailSenderStatus {
            provider: MailProvider::parse(row.try_get("provider")?)
                .ok_or(MailError::InvalidConfiguration)?,
            from_address: row.try_get("from_address")?,
            from_name: row.try_get("from_name")?,
            template_key: row.try_get("template_key")?,
            sabia_origin: row.try_get("sabia_origin")?,
        }))
    }

    pub async fn replace(
        &self,
        configuration: MailSenderConfiguration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<MailSenderStatus, MailError> {
        validate_configuration(&configuration)?;
        let envelope = encrypt_api_key(&self.master_key, &configuration.api_key)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM mail_sender WHERE id = 1")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO mail_sender \
             (id, provider, api_key_envelope, from_address, from_name, template_key, \
              sabia_origin, updated_at) \
             VALUES (1, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(configuration.provider.as_str())
        .bind(envelope)
        .bind(&configuration.from_address)
        .bind(&configuration.from_name)
        .bind(&configuration.template_key)
        .bind(&configuration.sabia_origin)
        .bind(format_timestamp(now))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(MailSenderStatus {
            provider: configuration.provider,
            from_address: configuration.from_address,
            from_name: configuration.from_name,
            template_key: configuration.template_key,
            sabia_origin: configuration.sabia_origin,
        })
    }

    pub async fn send_invitation(
        &self,
        message: &InvitationMail,
    ) -> Result<MailReceipt, MailError> {
        let row = sqlx::query(
            "SELECT provider, api_key_envelope, from_address, from_name, template_key, sabia_origin \
             FROM mail_sender WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Err(MailError::NotConfigured);
        };
        use sqlx::Row;
        let provider =
            MailProvider::parse(row.try_get("provider")?).ok_or(MailError::InvalidConfiguration)?;
        let envelope: Vec<u8> = row.try_get("api_key_envelope")?;
        let api_key = decrypt_api_key(&self.master_key, &envelope)?;
        let from_address: Option<String> = row.try_get("from_address")?;
        let from_name: Option<String> = row.try_get("from_name")?;
        let template_key: Option<String> = row.try_get("template_key")?;
        let sabia_origin: Option<String> = row.try_get("sabia_origin")?;
        match provider {
            MailProvider::Sabia => {
                send_sabia(
                    sabia_origin
                        .as_deref()
                        .ok_or(MailError::InvalidConfiguration)?,
                    &api_key,
                    template_key
                        .as_deref()
                        .ok_or(MailError::InvalidConfiguration)?,
                    message,
                )
                .await
            }
            MailProvider::Resend => {
                send_direct(
                    "https://api.resend.com/emails",
                    &api_key,
                    from_address
                        .as_deref()
                        .ok_or(MailError::InvalidConfiguration)?,
                    from_name.as_deref().unwrap_or("Puxiri"),
                    message,
                )
                .await
            }
            MailProvider::MillionSend => {
                send_direct(
                    "https://api.millionsend.com/emails",
                    &api_key,
                    from_address
                        .as_deref()
                        .ok_or(MailError::InvalidConfiguration)?,
                    from_name.as_deref().unwrap_or("Puxiri"),
                    message,
                )
                .await
            }
        }
    }
}

pub fn validate_configuration(configuration: &MailSenderConfiguration) -> Result<(), MailError> {
    if configuration.api_key.is_empty()
        || configuration.api_key.len() > KEY_LIMIT
        || configuration
            .api_key
            .chars()
            .any(|character| character.is_control())
    {
        return Err(MailError::InvalidConfiguration);
    }
    match configuration.provider {
        MailProvider::Sabia => {
            if configuration.from_address.is_some() || configuration.from_name.is_some() {
                return Err(MailError::InvalidConfiguration);
            }
            let template = configuration
                .template_key
                .as_deref()
                .ok_or(MailError::InvalidConfiguration)?;
            if !valid_template_key(template) {
                return Err(MailError::InvalidConfiguration);
            }
            let origin = configuration
                .sabia_origin
                .as_deref()
                .ok_or(MailError::InvalidConfiguration)?;
            if allowed_origin(origin).is_none() {
                return Err(MailError::InvalidConfiguration);
            }
        }
        MailProvider::Resend | MailProvider::MillionSend => {
            if configuration.template_key.is_some() || configuration.sabia_origin.is_some() {
                return Err(MailError::InvalidConfiguration);
            }
            let address = configuration
                .from_address
                .as_deref()
                .ok_or(MailError::InvalidConfiguration)?;
            if !valid_mailbox(address) {
                return Err(MailError::InvalidConfiguration);
            }
            if configuration.from_name.as_deref().is_some_and(|name| {
                name.is_empty() || name.len() > 120 || name.contains(['<', '>', '"', '\r', '\n'])
            }) {
                return Err(MailError::InvalidConfiguration);
            }
        }
    }
    Ok(())
}

async fn send_sabia(
    origin: &str,
    api_key: &str,
    template: &str,
    message: &InvitationMail,
) -> Result<MailReceipt, MailError> {
    let origin = allowed_origin(origin).ok_or(MailError::InvalidConfiguration)?;
    let url = format!("{origin}/v1/messages");
    let body = json!({
        "channel": "email",
        "to": message.to,
        "template": template,
        "locale": "pt-BR",
        "message_type": "transactional",
        "data": {"invite_url": message.invite_url}
    });
    let (status, payload) = post_json(&url, api_key, &message.idempotency_key, &body).await?;
    if status != 202 {
        return Err(if (400..500).contains(&status) {
            MailError::Rejected
        } else {
            MailError::Unknown
        });
    }
    let id = payload
        .get("delivery_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or(MailError::Unknown)?;
    if payload.get("status").and_then(Value::as_str) != Some("queued") {
        return Err(MailError::Unknown);
    }
    Ok(MailReceipt {
        provider_message_id: id.to_owned(),
    })
}

async fn send_direct(
    url: &str,
    api_key: &str,
    from_address: &str,
    from_name: &str,
    message: &InvitationMail,
) -> Result<MailReceipt, MailError> {
    let from = if from_name.is_empty() {
        from_address.to_owned()
    } else {
        format!("{from_name} <{from_address}>")
    };
    let body = json!({
        "from": from,
        "to": [message.to],
        "subject": "Convite para o Puxiri",
        "text": format!(
            "Você recebeu um convite para o Puxiri. Abra {} e entre com o mesmo email. Um login sem convite não abre o trabalho.",
            message.invite_url
        )
    });
    let (status, payload) = post_json(url, api_key, &message.idempotency_key, &body).await?;
    if status != 200 {
        return Err(if (400..500).contains(&status) {
            MailError::Rejected
        } else {
            MailError::Unknown
        });
    }
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or(MailError::Unknown)?;
    Ok(MailReceipt {
        provider_message_id: id.to_owned(),
    })
}

async fn post_json(
    url: &str,
    api_key: &str,
    idempotency_key: &str,
    body: &Value,
) -> Result<(u16, Value), MailError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| MailError::Unknown)?;
    let response = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", idempotency_key)
        .json(body)
        .send()
        .await
        .map_err(|_| MailError::Unknown)?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|_| MailError::Unknown)? {
        if bytes.len().saturating_add(chunk.len()) > BODY_LIMIT {
            return Err(MailError::Unknown);
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Ok((status, Value::Null));
    }
    let payload = serde_json::from_slice(&bytes).map_err(|_| MailError::Unknown)?;
    Ok((status, payload))
}

fn allowed_origin(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if url.path() != "/" && url.path() != "" {
        return None;
    }
    let host = url.host_str()?;
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme_ok {
        return None;
    }
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

fn valid_template_key(value: &str) -> bool {
    let mut previous_separator = true;
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    for byte in value.bytes() {
        let separator = byte == b'.' || byte == b'-';
        if separator && previous_separator {
            return false;
        }
        if !separator && !byte.is_ascii_lowercase() && !byte.is_ascii_digit() {
            return false;
        }
        previous_separator = separator;
    }
    !previous_separator
}

fn valid_mailbox(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && domain.len() <= 253
        && domain.contains('.')
        && !local.contains([' ', '<', '>', '"'])
        && !domain.contains([' ', '<', '>', '"'])
        && value.is_ascii()
}

fn encrypt_api_key(master_key: &MasterKey, secret: &str) -> Result<Vec<u8>, MailError> {
    let key_material = master_key
        .derive_mail_api_key()
        .map_err(|_| MailError::Secret)?;
    let cipher = RandomizedNonceKey::new(&AES_256_GCM, key_material.as_slice())
        .map_err(|_| MailError::Secret)?;
    let key_id = master_key.key_id();
    let mut ciphertext = Zeroizing::new(secret.as_bytes().to_vec());
    let nonce = cipher
        .seal_in_place_append_tag(Aad::from(mail_aad(&key_id)), &mut *ciphertext)
        .map_err(|_| MailError::Secret)?;
    let mut envelope = Vec::with_capacity(1 + 16 + NONCE_LEN + ciphertext.len());
    envelope.push(1);
    envelope.extend_from_slice(&key_id);
    envelope.extend_from_slice(nonce.as_ref());
    envelope.extend_from_slice(ciphertext.as_slice());
    Ok(envelope)
}

fn decrypt_api_key(
    master_key: &MasterKey,
    envelope: &[u8],
) -> Result<Zeroizing<String>, MailError> {
    if envelope.first() != Some(&1) || envelope.len() < 1 + 16 + NONCE_LEN + 16 {
        return Err(MailError::Secret);
    }
    let key_id = master_key.key_id();
    if envelope[1..17] != key_id {
        return Err(MailError::Secret);
    }
    let key_material = master_key
        .derive_mail_api_key()
        .map_err(|_| MailError::Secret)?;
    let cipher = RandomizedNonceKey::new(&AES_256_GCM, key_material.as_slice())
        .map_err(|_| MailError::Secret)?;
    let nonce_bytes: &[u8; NONCE_LEN] = envelope[17..17 + NONCE_LEN]
        .try_into()
        .map_err(|_| MailError::Secret)?;
    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| MailError::Secret)?;
    let mut plaintext = Zeroizing::new(envelope[17 + NONCE_LEN..].to_vec());
    let opened = cipher
        .open_in_place(nonce, Aad::from(mail_aad(&key_id)), &mut plaintext)
        .map_err(|_| MailError::Secret)?;
    let text = std::str::from_utf8(opened).map_err(|_| MailError::Secret)?;
    Ok(Zeroizing::new(text.to_owned()))
}

fn mail_aad(key_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = b"puxiri mail api key envelope v1\0".to_vec();
    aad.extend_from_slice(key_id);
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn direct_providers_use_separate_hosts_and_send_text_only() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_server = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().fallback(axum::routing::post(
                |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                    captured_server.lock().unwrap().push((
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned(),
                        String::from_utf8(body.to_vec()).unwrap_or_default(),
                    ));
                    axum::Json(serde_json::json!({"id": "provider-message"}))
                },
            ));
            axum::serve(listener, app).await.unwrap();
        });
        let message = InvitationMail {
            to: "person@example.test".to_owned(),
            invite_url: "https://tracker.example.test/".to_owned(),
            idempotency_key: "01890f3e-7b1c-7cc4-98a7-123456789abc".to_owned(),
        };
        let resend = format!("http://{address}/emails");
        let millionsend = format!("http://{address}/emails");
        let first = send_direct(
            &resend,
            "resend-key",
            "puxiri@example.test",
            "Puxiri",
            &message,
        )
        .await
        .unwrap();
        let second = send_direct(
            &millionsend,
            "millionsend-key",
            "puxiri@example.test",
            "Puxiri",
            &message,
        )
        .await
        .unwrap();
        assert_eq!(first.provider_message_id, "provider-message");
        assert_eq!(second.provider_message_id, "provider-message");
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].0.contains("resend-key"));
        assert!(calls[1].0.contains("millionsend-key"));
        assert!(!calls[0].1.contains("\"html\""));
        assert!(!calls[1].1.contains("\"html\""));
        assert!(calls[0].1.contains("\"text\""));
    }
}
