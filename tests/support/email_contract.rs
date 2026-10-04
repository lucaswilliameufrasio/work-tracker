//! Local-only Phase 0 REST spike. Not a production adapter, scheduler or probe.
//! No TLS/account entitlement/delivery is proved by a protocol fake.
use std::{net::IpAddr, time::Duration};

use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy)]
pub enum Provider {
    Cloudflare,
    Resend,
}

#[derive(Clone, Debug, Serialize)]
pub struct Email {
    pub from: String,
    pub to: String,
    pub reply_to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

#[derive(Debug, PartialEq)]
pub enum Delivery {
    Accepted,
    Delivered,
    Queued,
    PermanentBounce,
    Suppressed,
}

#[derive(Debug, PartialEq)]
pub struct Receipt {
    pub message_id: Option<String>,
    pub delivery: Delivery,
}

#[derive(Debug, PartialEq)]
pub enum FailureKind {
    InvalidRequest,
    Authentication,
    Rejected,
    Throttled,
    Conflict,
    Temporary,
    Protocol,
    Transport,
}

#[derive(Debug, PartialEq)]
pub enum Acceptance {
    Rejected,
    Unknown,
}

// Deliberately contains no URLs, token, recipient, provider body or underlying
// reqwest error: even Debug is safe for this fixture's logging contract.
#[derive(Debug, PartialEq)]
pub struct Failure {
    pub kind: FailureKind,
    pub acceptance: Acceptance,
    pub status: Option<u16>,
    pub retry_after_seconds: Option<u64>,
}

fn failure(kind: FailureKind, acceptance: Acceptance, status: Option<u16>) -> Failure {
    Failure {
        kind,
        acceptance,
        status,
        retry_after_seconds: None,
    }
}

pub struct LocalEmailClient {
    client: Client,
    provider: Provider,
    endpoint: Url,
    token: String,
}

impl LocalEmailClient {
    pub fn new(
        provider: Provider,
        base: &str,
        account: &str,
        token: &str,
        timeout: Duration,
    ) -> Result<Self, Failure> {
        let invalid = || failure(FailureKind::InvalidRequest, Acceptance::Rejected, None);
        let mut endpoint = Url::parse(base).map_err(|_| invalid())?;
        let ip: IpAddr = endpoint
            .host_str()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        if !ip.is_loopback()
            || endpoint.scheme() != "http"
            || endpoint.path() != "/"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || token.is_empty()
            || token.bytes().any(|byte| !(33..=126).contains(&byte))
            || timeout.is_zero()
        {
            return Err(invalid());
        }
        let path = match provider {
            Provider::Cloudflare => {
                if account.len() != 32 || !account.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(invalid());
                }
                format!("/client/v4/accounts/{account}/email/sending/send")
            }
            Provider::Resend => "/emails".into(),
        };
        endpoint.set_path(&path);
        let client = Client::builder()
            .timeout(timeout)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| invalid())?;
        Ok(Self {
            client,
            provider,
            endpoint,
            token: token.into(),
        })
    }

    pub async fn send(&self, email: &Email, idempotency_key: &str) -> Result<Receipt, Failure> {
        // Fixture header hygiene, not full address/template/product validation.
        if [&email.from, &email.to, &email.reply_to, &email.subject]
            .iter()
            .any(|value| value.is_empty() || value.chars().any(|ch| ch == '\r' || ch == '\n'))
            || email.text.is_empty()
            || email.html.is_empty()
        {
            return Err(failure(
                FailureKind::InvalidRequest,
                Acceptance::Rejected,
                None,
            ));
        }
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.token)
            .json(email);
        if matches!(self.provider, Provider::Resend) {
            if idempotency_key.is_empty()
                || idempotency_key.len() > 256
                || idempotency_key
                    .bytes()
                    .any(|byte| !(33..=126).contains(&byte))
            {
                return Err(failure(
                    FailureKind::InvalidRequest,
                    Acceptance::Rejected,
                    None,
                ));
            }
            request = request.header("Idempotency-Key", idempotency_key);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| failure(FailureKind::Transport, Acceptance::Unknown, None))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failure(FailureKind::Transport, Acceptance::Unknown, Some(status)))?
        {
            if body.len() + chunk.len() > 65536 {
                return Err(failure(
                    FailureKind::Protocol,
                    Acceptance::Unknown,
                    Some(status),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if !(200..300).contains(&status) {
            let (kind, acceptance) = match status {
                400 | 422 => (FailureKind::InvalidRequest, Acceptance::Rejected),
                401 | 403 => (FailureKind::Authentication, Acceptance::Rejected),
                429 => (FailureKind::Throttled, Acceptance::Rejected),
                409 if matches!(self.provider, Provider::Resend) => {
                    let name = serde_json::from_slice::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("name")
                                .and_then(|name| name.as_str())
                                .map(str::to_owned)
                        });
                    match name.as_deref() {
                        Some("concurrent_idempotent_requests") | Some("resource_locked") => {
                            (FailureKind::Temporary, Acceptance::Unknown)
                        }
                        Some("invalid_idempotent_request") => {
                            (FailureKind::Conflict, Acceptance::Rejected)
                        }
                        _ => (FailureKind::Protocol, Acceptance::Unknown),
                    }
                }
                500..=599 => (FailureKind::Temporary, Acceptance::Unknown),
                300..=399 => (FailureKind::Protocol, Acceptance::Unknown),
                400..=499 => (FailureKind::Rejected, Acceptance::Rejected),
                _ => (FailureKind::Protocol, Acceptance::Unknown),
            };
            return Err(Failure {
                kind,
                acceptance,
                status: Some(status),
                retry_after_seconds: retry_after,
            });
        }
        let protocol = || failure(FailureKind::Protocol, Acceptance::Unknown, Some(status));
        match self.provider {
            Provider::Resend => {
                #[derive(Deserialize)]
                struct Response {
                    id: String,
                }
                let result: Response = serde_json::from_slice(&body).map_err(|_| protocol())?;
                if result.id.is_empty()
                    || result.id.len() > 256
                    || result.id.chars().any(char::is_control)
                {
                    return Err(protocol());
                }
                Ok(Receipt {
                    message_id: Some(result.id),
                    delivery: Delivery::Accepted,
                })
            }
            Provider::Cloudflare => {
                #[derive(Deserialize)]
                struct ResultBody {
                    message_id: Option<String>,
                    delivered: Vec<String>,
                    queued: Vec<String>,
                    permanent_bounces: Vec<String>,
                    #[serde(default)]
                    suppressed_recipients: Vec<String>,
                }
                #[derive(Deserialize)]
                struct Response {
                    success: bool,
                    errors: Vec<serde_json::Value>,
                    result: ResultBody,
                }
                let result: Response = serde_json::from_slice(&body).map_err(|_| protocol())?;
                if !result.success
                    || !result.errors.is_empty()
                    || result.result.message_id.as_ref().is_some_and(|id| {
                        id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                    })
                {
                    return Err(protocol());
                }
                let outcomes = [
                    (&result.result.delivered, Delivery::Delivered),
                    (&result.result.queued, Delivery::Queued),
                    (&result.result.permanent_bounces, Delivery::PermanentBounce),
                    (&result.result.suppressed_recipients, Delivery::Suppressed),
                ];
                if outcomes
                    .iter()
                    .map(|(addresses, _)| addresses.len())
                    .sum::<usize>()
                    != 1
                {
                    return Err(protocol());
                }
                let delivery = outcomes
                    .into_iter()
                    .find_map(|(addresses, delivery)| {
                        (addresses.as_slice() == [email.to.as_str()]).then_some(delivery)
                    })
                    .ok_or_else(protocol)?;
                Ok(Receipt {
                    message_id: result.result.message_id,
                    delivery,
                })
            }
        }
    }
}
