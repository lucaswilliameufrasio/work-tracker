use std::net::IpAddr;

use chrono::{DateTime, Utc};
use thiserror::Error;

use super::{
    PasswordError,
    credentials::{CredentialError, CredentialStore},
    mfa::login::{
        MfaLoginAuthentication, MfaLoginChallenge, MfaLoginError, MfaLoginPasswordOutcome,
        MfaLoginService, MfaLoginToken,
    },
    normalize_email_address,
    rate_limit::{AuthRateLimitError, AuthRateLimiter},
    session::IssuedSession,
};

#[derive(Debug, Error)]
pub enum PasswordAuthenticationError {
    #[error("password authentication failed")]
    Credential(#[from] CredentialError),
    #[error("authentication rate-limit operation failed")]
    RateLimit(#[from] AuthRateLimitError),
    #[error("MFA login operation failed: {0}")]
    MfaLogin(#[from] MfaLoginError),
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
    MfaRequired(MfaLoginChallenge),
    MfaSetupRequired,
    Rejected {
        blocked_until: Option<DateTime<Utc>>,
    },
}

#[derive(Clone)]
pub struct PasswordAuthenticator {
    credentials: CredentialStore,
    rate_limiter: AuthRateLimiter,
    mfa_login: MfaLoginService,
    dummy_hash: String,
}

impl PasswordAuthenticator {
    pub fn new(
        credentials: CredentialStore,
        rate_limiter: AuthRateLimiter,
        mfa_login: MfaLoginService,
        dummy_hash: String,
    ) -> Self {
        Self {
            credentials,
            rate_limiter,
            mfa_login,
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

        match self
            .mfa_login
            .after_password_verification(&verified, client_ip, now)
            .await?
        {
            MfaLoginPasswordOutcome::Authenticated(issued) => {
                Ok(PasswordAuthentication::Authenticated(issued))
            }
            MfaLoginPasswordOutcome::MfaRequired(challenge) => {
                Ok(PasswordAuthentication::MfaRequired(challenge))
            }
            MfaLoginPasswordOutcome::SetupRequired => Ok(PasswordAuthentication::MfaSetupRequired),
            MfaLoginPasswordOutcome::RateLimited(blocked_until) => {
                Ok(PasswordAuthentication::Rejected {
                    blocked_until: Some(blocked_until),
                })
            }
            MfaLoginPasswordOutcome::Rejected => {
                let blocked_until = self
                    .rate_limiter
                    .record_failure(client_ip, &identity_key, now)
                    .await?;
                Ok(PasswordAuthentication::Rejected { blocked_until })
            }
        }
    }

    pub async fn complete_mfa_login(
        &self,
        token: &MfaLoginToken,
        code: &str,
        client_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<MfaLoginAuthentication, PasswordAuthenticationError> {
        self.mfa_login
            .complete(token, code, client_ip, now)
            .await
            .map_err(PasswordAuthenticationError::from)
    }
}
