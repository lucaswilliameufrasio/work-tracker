use std::net::IpAddr;

use chrono::{DateTime, Utc};
use thiserror::Error;

use super::{
    PasswordError,
    credentials::{CredentialError, CredentialStore},
    normalize_email_address,
    rate_limit::{AuthRateLimitError, AuthRateLimiter},
    session::{IssuedSession, SessionError, SessionStore},
};

#[derive(Debug, Error)]
pub enum PasswordAuthenticationError {
    #[error("password authentication failed")]
    Credential(#[from] CredentialError),
    #[error("session issuance failed")]
    Session(#[from] SessionError),
    #[error("authentication rate-limit operation failed")]
    RateLimit(#[from] AuthRateLimitError),
}

impl PasswordAuthenticationError {
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Credential(CredentialError::Password(PasswordError::Busy))
        )
    }
}

pub enum PasswordAuthentication {
    Authenticated(IssuedSession),
    Rejected {
        blocked_until: Option<DateTime<Utc>>,
    },
}

#[derive(Clone)]
pub struct PasswordAuthenticator {
    credentials: CredentialStore,
    sessions: SessionStore,
    rate_limiter: AuthRateLimiter,
    dummy_hash: String,
}

impl PasswordAuthenticator {
    pub fn new(
        credentials: CredentialStore,
        sessions: SessionStore,
        rate_limiter: AuthRateLimiter,
        dummy_hash: String,
    ) -> Self {
        Self {
            credentials,
            sessions,
            rate_limiter,
            dummy_hash,
        }
    }

    /// Performs the domain-level password login sequence. This service does not
    /// create an HTTP route, cookie, or CSRF policy; those remain adapter work.
    pub async fn authenticate(
        &self,
        email_address: &str,
        password: String,
        client_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<PasswordAuthentication, PasswordAuthenticationError> {
        let identity_key = normalize_email_address(email_address)
            .map(|email| email.key)
            .unwrap_or_else(|_| String::from("invalid-email"));

        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until(client_ip, &identity_key, now)
            .await?
        {
            return Ok(PasswordAuthentication::Rejected {
                blocked_until: Some(blocked_until),
            });
        }

        let verified = self
            .credentials
            .verify_email_credential(email_address, password, &self.dummy_hash)
            .await?;
        let Some(verified) = verified else {
            let blocked_until = self
                .rate_limiter
                .record_failure(client_ip, &identity_key, now)
                .await?;
            return Ok(PasswordAuthentication::Rejected { blocked_until });
        };

        // Re-check after the KDF so a block established by another in-flight
        // request is observed before session issuance when possible.
        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until(client_ip, &identity_key, now)
            .await?
        {
            return Ok(PasswordAuthentication::Rejected {
                blocked_until: Some(blocked_until),
            });
        }

        self.rate_limiter.record_success(&identity_key).await?;
        let issued = self
            .sessions
            .issue_if_credential_matches(
                &verified.user_id,
                &verified.email_key,
                &verified.password_hash,
                now,
            )
            .await?;
        let Some(issued) = issued else {
            let blocked_until = self
                .rate_limiter
                .record_failure(client_ip, &identity_key, now)
                .await?;
            return Ok(PasswordAuthentication::Rejected { blocked_until });
        };

        Ok(PasswordAuthentication::Authenticated(issued))
    }
}
