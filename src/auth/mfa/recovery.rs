use std::net::IpAddr;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::super::{
    PasswordError,
    credentials::{CredentialError, CredentialStore},
    normalize_email_address,
    rate_limit::{AuthRateLimitError, AuthRateLimiter},
    recovery::{RecoveryCodeError, RecoveryCodeStore},
};
use crate::auth::time::format_timestamp;

const RECOVERY_TOKEN_BYTES: usize = 32;
const RECOVERY_TOKEN_DOMAIN: &[u8] = b"puxiri mfa recovery token v1\0";
const RECOVERY_SESSION_LIFETIME: TimeDelta = TimeDelta::minutes(10);

#[derive(Debug, Error)]
pub enum MfaRecoveryTokenError {
    #[error("MFA recovery token encoding is invalid")]
    InvalidEncoding,
    #[error("secure MFA recovery token generation failed")]
    RandomnessFailed,
}

pub struct MfaRecoveryToken(Zeroizing<String>);

impl MfaRecoveryToken {
    pub fn parse(value: &str) -> Result<Self, MfaRecoveryTokenError> {
        if value.len() != 43
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(MfaRecoveryTokenError::InvalidEncoding);
        }

        let decoded = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| MfaRecoveryTokenError::InvalidEncoding)?,
        );
        let canonical = Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice()));
        if decoded.len() != RECOVERY_TOKEN_BYTES || canonical.as_str() != value {
            return Err(MfaRecoveryTokenError::InvalidEncoding);
        }

        Ok(Self(Zeroizing::new(value.to_owned())))
    }

    fn generate() -> Result<Self, MfaRecoveryTokenError> {
        let mut random_bytes = Zeroizing::new([0_u8; RECOVERY_TOKEN_BYTES]);
        OsRng
            .try_fill_bytes(&mut *random_bytes)
            .map_err(|_| MfaRecoveryTokenError::RandomnessFailed)?;
        Ok(Self(Zeroizing::new(
            URL_SAFE_NO_PAD.encode(random_bytes.as_slice()),
        )))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(RECOVERY_TOKEN_DOMAIN);
        digest.update(self.0.as_bytes());
        digest.finalize().into()
    }
}

pub struct MfaRecoveryAccess {
    id: String,
    token: MfaRecoveryToken,
    expires_at: DateTime<Utc>,
}

impl MfaRecoveryAccess {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn token(&self) -> &MfaRecoveryToken {
        &self.token
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

pub enum MfaRecoveryAuthentication {
    Started(MfaRecoveryAccess),
    Rejected {
        blocked_until: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Error)]
pub enum MfaRecoveryError {
    #[error("MFA recovery credential verification failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("MFA recovery code operation failed: {0}")]
    RecoveryCode(#[from] RecoveryCodeError),
    #[error("MFA recovery rate-limit operation failed: {0}")]
    RateLimit(#[from] AuthRateLimitError),
    #[error("MFA recovery persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("MFA recovery audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
    #[error("MFA recovery deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error(transparent)]
    Token(#[from] MfaRecoveryTokenError),
}

impl MfaRecoveryError {
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Credential(CredentialError::Password(PasswordError::Busy))
        )
    }
}

#[derive(Clone)]
pub struct MfaRecoveryService {
    pool: SqlitePool,
    credentials: CredentialStore,
    recovery_codes: RecoveryCodeStore,
    rate_limiter: AuthRateLimiter,
    dummy_hash: String,
}

impl MfaRecoveryService {
    pub fn new(
        pool: SqlitePool,
        credentials: CredentialStore,
        recovery_codes: RecoveryCodeStore,
        rate_limiter: AuthRateLimiter,
        dummy_hash: String,
    ) -> Self {
        Self {
            pool,
            credentials,
            recovery_codes,
            rate_limiter,
            dummy_hash,
        }
    }

    /// Authenticates a restricted recovery flow with a confirmed email,
    /// password, and one unused recovery code. Installation admins and users
    /// without active TOTP are deliberately indistinguishable rejections.
    pub async fn start(
        &self,
        email_address: &str,
        password: String,
        recovery_code: &str,
        client_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<MfaRecoveryAuthentication, MfaRecoveryError> {
        let identity_key = normalize_email_address(email_address)
            .map(|email| email.key)
            .unwrap_or_else(|_| String::from("invalid-email"));
        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until(client_ip, &identity_key, now)
            .await?
        {
            return Ok(MfaRecoveryAuthentication::Rejected {
                blocked_until: Some(blocked_until),
            });
        }

        let verified = self
            .credentials
            .verify_email_credential(email_address, password, &self.dummy_hash)
            .await?;
        let Some(verified) = verified else {
            return self.reject_attempt(client_ip, &identity_key, now).await;
        };

        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until(client_ip, &identity_key, now)
            .await?
        {
            return Ok(MfaRecoveryAuthentication::Rejected {
                blocked_until: Some(blocked_until),
            });
        }

        let expires_at = now
            .checked_add_signed(RECOVERY_SESSION_LIFETIME)
            .ok_or(MfaRecoveryError::DeadlineOutOfRange)?;
        let recovery_session_id = Uuid::now_v7().to_string();
        let token = MfaRecoveryToken::generate()?;
        let token_digest = token.digest();
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let current_password: i64 = sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM users \
                 JOIN password_credentials ON password_credentials.user_id = users.id \
                 JOIN user_emails ON user_emails.user_id = users.id \
                 WHERE users.id = ? AND users.state = 'active' \
                   AND password_credentials.password_hash = ? \
                   AND user_emails.address_key = ? \
                   AND user_emails.confirmed_at IS NOT NULL)",
        )
        .bind(&verified.user_id)
        .bind(&verified.password_hash)
        .bind(&verified.email_key)
        .fetch_one(&mut *transaction)
        .await?;
        let active_totp: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mfa_credentials \
             WHERE user_id = ? AND activated_at IS NOT NULL)",
        )
        .bind(&verified.user_id)
        .fetch_one(&mut *transaction)
        .await?;
        let installation_admin: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM installation_admins WHERE user_id = ?)",
        )
        .bind(&verified.user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if current_password != 1 || active_totp != 1 || installation_admin == 1 {
            transaction.rollback().await?;
            return self.reject_attempt(client_ip, &identity_key, now).await;
        }

        sqlx::query(
            "UPDATE mfa_recovery_sessions SET invalidated_at = ? \
             WHERE user_id = ? AND completed_at IS NULL AND invalidated_at IS NULL \
               AND expires_at <= ?",
        )
        .bind(&now_text)
        .bind(&verified.user_id)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        let recovery_already_open: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mfa_recovery_sessions \
             WHERE user_id = ? AND completed_at IS NULL AND invalidated_at IS NULL \
               AND expires_at > ?)",
        )
        .bind(&verified.user_id)
        .bind(&now_text)
        .fetch_one(&mut *transaction)
        .await?;
        if recovery_already_open == 1 {
            transaction.rollback().await?;
            return self.reject_attempt(client_ip, &identity_key, now).await;
        }

        if !self
            .recovery_codes
            .consume_in_transaction(&mut transaction, &verified.user_id, recovery_code, now)
            .await?
        {
            transaction.rollback().await?;
            return self.reject_attempt(client_ip, &identity_key, now).await;
        }

        sqlx::query(
            "INSERT INTO mfa_recovery_sessions \
             (id, token_hash, user_id, created_at, expires_at, completed_at, invalidated_at) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL)",
        )
        .bind(&recovery_session_id)
        .bind(token_digest.as_slice())
        .bind(&verified.user_id)
        .bind(&now_text)
        .bind(&expires_at_text)
        .execute(&mut *transaction)
        .await?;
        self.rate_limiter
            .record_success_in_transaction(&mut transaction, &identity_key)
            .await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(&verified.user_id),
            AuditEventKind::MfaRecoveryStarted,
            AuditEventResult::Success,
            &AuditEventTarget::mfa_recovery_session(&recovery_session_id),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(MfaRecoveryAuthentication::Started(MfaRecoveryAccess {
            id: recovery_session_id,
            token,
            expires_at,
        }))
    }

    /// Checks only a live recovery token record; this token never validates as
    /// a normal login session or recent MFA proof. Its deadline is not extended.
    /// Mutating recovery operations must revalidate it in their own transaction.
    pub async fn validate(
        &self,
        token: &MfaRecoveryToken,
        now: DateTime<Utc>,
    ) -> Result<bool, MfaRecoveryError> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM mfa_recovery_sessions \
                 JOIN users ON users.id = mfa_recovery_sessions.user_id \
                 WHERE mfa_recovery_sessions.token_hash = ? \
                   AND mfa_recovery_sessions.expires_at > ? \
                   AND mfa_recovery_sessions.completed_at IS NULL \
                   AND mfa_recovery_sessions.invalidated_at IS NULL \
                   AND NOT EXISTS (SELECT 1 FROM installation_admins \
                                   WHERE installation_admins.user_id = users.id) \
                   AND EXISTS (SELECT 1 FROM mfa_credentials \
                               WHERE mfa_credentials.user_id = users.id \
                                 AND mfa_credentials.activated_at IS NOT NULL) \
                   AND users.state = 'active')",
        )
        .bind(token.digest().as_slice())
        .bind(format_timestamp(now))
        .fetch_one(&self.pool)
        .await?;
        Ok(exists == 1)
    }

    async fn reject_attempt(
        &self,
        client_ip: IpAddr,
        identity_key: &str,
        now: DateTime<Utc>,
    ) -> Result<MfaRecoveryAuthentication, MfaRecoveryError> {
        let blocked_until = self
            .rate_limiter
            .record_failure(client_ip, identity_key, now)
            .await?;
        Ok(MfaRecoveryAuthentication::Rejected { blocked_until })
    }
}
