use std::net::IpAddr;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::auth::{
    credentials::VerifiedEmailCredential,
    rate_limit::{AuthRateLimitError, AuthRateLimiter},
    session::{IssuedSession, SessionError, SessionStore},
    time::format_timestamp,
};

use super::store::{TotpCredentialError, TotpCredentialStore};

const MFA_LOGIN_TOKEN_BYTES: usize = 32;
const MFA_LOGIN_TOKEN_DOMAIN: &[u8] = b"puxiri mfa login challenge v1\0";
const PASSWORD_FINGERPRINT_DOMAIN: &[u8] = b"puxiri mfa login credential fingerprint v1\0";
const MFA_LOGIN_CHALLENGE_LIFETIME: TimeDelta = TimeDelta::minutes(10);

#[derive(Debug, Error)]
pub enum MfaLoginTokenError {
    #[error("MFA login challenge token encoding is invalid")]
    InvalidEncoding,
    #[error("secure MFA login challenge token generation failed")]
    RandomnessFailed,
}

pub struct MfaLoginToken(Zeroizing<String>);

impl MfaLoginToken {
    pub fn parse(value: &str) -> Result<Self, MfaLoginTokenError> {
        if value.len() != 43
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(MfaLoginTokenError::InvalidEncoding);
        }

        let decoded = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| MfaLoginTokenError::InvalidEncoding)?,
        );
        let canonical = Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice()));
        if decoded.len() != MFA_LOGIN_TOKEN_BYTES || canonical.as_str() != value {
            return Err(MfaLoginTokenError::InvalidEncoding);
        }

        Ok(Self(Zeroizing::new(value.to_owned())))
    }

    fn generate() -> Result<Self, MfaLoginTokenError> {
        let mut random_bytes = Zeroizing::new([0_u8; MFA_LOGIN_TOKEN_BYTES]);
        OsRng
            .try_fill_bytes(&mut *random_bytes)
            .map_err(|_| MfaLoginTokenError::RandomnessFailed)?;
        Ok(Self(Zeroizing::new(
            URL_SAFE_NO_PAD.encode(random_bytes.as_slice()),
        )))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(MFA_LOGIN_TOKEN_DOMAIN);
        digest.update(self.0.as_bytes());
        digest.finalize().into()
    }
}

pub struct MfaLoginChallenge {
    token: MfaLoginToken,
    expires_at: DateTime<Utc>,
}

impl MfaLoginChallenge {
    pub fn token(&self) -> &MfaLoginToken {
        &self.token
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

pub enum MfaLoginAuthentication {
    Authenticated(IssuedSession),
    Rejected {
        blocked_until: Option<DateTime<Utc>>,
    },
}

pub(crate) enum MfaLoginPasswordOutcome {
    Authenticated(IssuedSession),
    MfaRequired(MfaLoginChallenge),
    SetupRequired,
    RateLimited(DateTime<Utc>),
    Rejected,
}

#[derive(Debug, Error)]
pub enum MfaLoginError {
    #[error("MFA login challenge persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("MFA login session operation failed: {0}")]
    Session(#[from] SessionError),
    #[error("MFA login TOTP operation failed: {0}")]
    Totp(#[from] TotpCredentialError),
    #[error("MFA login rate-limit operation failed: {0}")]
    RateLimit(#[from] AuthRateLimitError),
    #[error("MFA login deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("MFA login challenge state is malformed")]
    InvalidState,
    #[error(transparent)]
    Token(#[from] MfaLoginTokenError),
}

#[derive(Clone)]
pub struct MfaLoginService {
    pool: SqlitePool,
    sessions: SessionStore,
    totp_credentials: TotpCredentialStore,
    rate_limiter: AuthRateLimiter,
}

impl MfaLoginService {
    pub fn new(
        pool: SqlitePool,
        sessions: SessionStore,
        totp_credentials: TotpCredentialStore,
        rate_limiter: AuthRateLimiter,
    ) -> Self {
        Self {
            pool,
            sessions,
            totp_credentials,
            rate_limiter,
        }
    }

    pub(crate) async fn after_password_verification(
        &self,
        verified: &VerifiedEmailCredential,
        client_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<MfaLoginPasswordOutcome, MfaLoginError> {
        let expires_at = now
            .checked_add_signed(MFA_LOGIN_CHALLENGE_LIFETIME)
            .ok_or(MfaLoginError::DeadlineOutOfRange)?;
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        Uuid::parse_str(&verified.user_id).map_err(|_| MfaLoginError::InvalidState)?;
        let credential_fingerprint = password_fingerprint(&verified.password_hash);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;

        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until_in_transaction(&mut transaction, client_ip, &verified.email_key, now)
            .await?
        {
            transaction.rollback().await?;
            return Ok(MfaLoginPasswordOutcome::RateLimited(blocked_until));
        }

        let credential_is_current: i64 = sqlx::query_scalar(
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
        .fetch_one(&mut **transaction)
        .await?;
        if credential_is_current != 1 {
            transaction.rollback().await?;
            return Ok(MfaLoginPasswordOutcome::Rejected);
        }

        let is_installation_admin: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM installation_admins WHERE user_id = ?)",
        )
        .bind(&verified.user_id)
        .fetch_one(&mut **transaction)
        .await?;
        let active_totp_credential_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM mfa_credentials \
             WHERE user_id = ? AND activated_at IS NOT NULL",
        )
        .bind(&verified.user_id)
        .fetch_optional(&mut **transaction)
        .await?;

        if is_installation_admin == 1 && active_totp_credential_id.is_none() {
            transaction.rollback().await?;
            return Ok(MfaLoginPasswordOutcome::SetupRequired);
        }

        if let Some(totp_credential_id) = active_totp_credential_id {
            Uuid::parse_str(&totp_credential_id).map_err(|_| MfaLoginError::InvalidState)?;
            let token = MfaLoginToken::generate()?;
            let token_hash = token.digest();
            sqlx::query("DELETE FROM mfa_login_challenges WHERE expires_at <= ?")
                .bind(&now_text)
                .execute(&mut **transaction)
                .await?;
            sqlx::query(
                "INSERT INTO mfa_login_challenges \
                 (token_hash, user_id, email_key, credential_fingerprint, \
                  totp_credential_id, created_at, expires_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(token_hash.as_slice())
            .bind(&verified.user_id)
            .bind(&verified.email_key)
            .bind(credential_fingerprint.as_slice())
            .bind(totp_credential_id)
            .bind(&now_text)
            .bind(&expires_at_text)
            .execute(&mut **transaction)
            .await?;
            transaction.commit().await?;
            return Ok(MfaLoginPasswordOutcome::MfaRequired(MfaLoginChallenge {
                token,
                expires_at,
            }));
        }

        let Some(issued) = self
            .sessions
            .issue_if_credential_matches_in_transaction(
                &mut transaction,
                &verified.user_id,
                &verified.email_key,
                &verified.password_hash,
                now,
                None,
            )
            .await?
        else {
            transaction.rollback().await?;
            return Ok(MfaLoginPasswordOutcome::Rejected);
        };
        self.rate_limiter
            .record_success_in_transaction(&mut transaction, &verified.email_key)
            .await?;
        transaction.commit().await?;
        Ok(MfaLoginPasswordOutcome::Authenticated(issued))
    }

    pub async fn complete(
        &self,
        token: &MfaLoginToken,
        code: &str,
        client_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<MfaLoginAuthentication, MfaLoginError> {
        let token_hash = token.digest();
        let now_text = format_timestamp(now);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let challenge = sqlx::query(
            "SELECT challenges.user_id, challenges.email_key, \
                    challenges.credential_fingerprint, challenges.created_at, \
                    challenges.expires_at, password_credentials.password_hash \
             FROM mfa_login_challenges AS challenges \
             JOIN users ON users.id = challenges.user_id \
             JOIN password_credentials \
               ON password_credentials.user_id = users.id \
             JOIN user_emails \
               ON user_emails.user_id = users.id \
              AND user_emails.address_key = challenges.email_key \
              AND user_emails.confirmed_at IS NOT NULL \
             JOIN mfa_credentials \
               ON mfa_credentials.id = challenges.totp_credential_id \
              AND mfa_credentials.user_id = users.id \
              AND mfa_credentials.activated_at IS NOT NULL \
             WHERE challenges.token_hash = ? \
               AND challenges.expires_at > ? AND users.state = 'active'",
        )
        .bind(token_hash.as_slice())
        .bind(&now_text)
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(challenge) = challenge else {
            sqlx::query("DELETE FROM mfa_login_challenges WHERE token_hash = ?")
                .bind(token_hash.as_slice())
                .execute(&mut **transaction)
                .await?;
            transaction.commit().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: None,
            });
        };

        let user_id: String = challenge.try_get("user_id")?;
        let email_key: String = challenge.try_get("email_key")?;
        let stored_fingerprint: Vec<u8> = challenge.try_get("credential_fingerprint")?;
        let created_at_text: String = challenge.try_get("created_at")?;
        let expires_at_text: String = challenge.try_get("expires_at")?;
        let password_hash: String = challenge.try_get("password_hash")?;
        let Some(created_at) = parse_timestamp(&created_at_text) else {
            return Err(MfaLoginError::InvalidState);
        };
        let Some(expires_at) = parse_timestamp(&expires_at_text) else {
            return Err(MfaLoginError::InvalidState);
        };
        let Some(maximum_expires_at) = created_at.checked_add_signed(MFA_LOGIN_CHALLENGE_LIFETIME)
        else {
            return Err(MfaLoginError::InvalidState);
        };
        if created_at > now || expires_at != maximum_expires_at || expires_at <= now {
            sqlx::query("DELETE FROM mfa_login_challenges WHERE token_hash = ?")
                .bind(token_hash.as_slice())
                .execute(&mut **transaction)
                .await?;
            transaction.commit().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: None,
            });
        }

        let current_fingerprint = password_fingerprint(&password_hash);
        let fingerprint_is_current = stored_fingerprint.len() == current_fingerprint.len()
            && stored_fingerprint
                .as_slice()
                .ct_eq(current_fingerprint.as_slice())
                .unwrap_u8()
                == 1;
        if !fingerprint_is_current {
            sqlx::query("DELETE FROM mfa_login_challenges WHERE token_hash = ?")
                .bind(token_hash.as_slice())
                .execute(&mut **transaction)
                .await?;
            transaction.commit().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: None,
            });
        }

        if let Some(blocked_until) = self
            .rate_limiter
            .blocked_until_in_transaction(&mut transaction, client_ip, &email_key, now)
            .await?
        {
            transaction.rollback().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: Some(blocked_until),
            });
        }

        let user_uuid = Uuid::parse_str(&user_id).map_err(|_| MfaLoginError::InvalidState)?;
        let code_is_valid = self
            .totp_credentials
            .verify_in_transaction(&mut transaction, user_uuid, code, now)
            .await?;
        if !code_is_valid {
            let blocked_until = self
                .rate_limiter
                .record_failure_in_transaction(&mut transaction, client_ip, &email_key, now)
                .await?;
            transaction.commit().await?;
            return Ok(MfaLoginAuthentication::Rejected { blocked_until });
        }

        let consumed = sqlx::query(
            "DELETE FROM mfa_login_challenges \
             WHERE token_hash = ? AND user_id = ? AND expires_at > ?",
        )
        .bind(token_hash.as_slice())
        .bind(&user_id)
        .bind(&now_text)
        .execute(&mut **transaction)
        .await?;
        if consumed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: None,
            });
        }

        let Some(issued) = self
            .sessions
            .issue_if_credential_matches_in_transaction(
                &mut transaction,
                &user_id,
                &email_key,
                &password_hash,
                now,
                Some(now),
            )
            .await?
        else {
            transaction.rollback().await?;
            return Ok(MfaLoginAuthentication::Rejected {
                blocked_until: None,
            });
        };
        self.rate_limiter
            .record_success_in_transaction(&mut transaction, &email_key)
            .await?;
        transaction.commit().await?;
        Ok(MfaLoginAuthentication::Authenticated(issued))
    }
}

fn password_fingerprint(password_hash: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(PASSWORD_FINGERPRINT_DOMAIN);
    digest.update(password_hash.as_bytes());
    digest.finalize().into()
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}
