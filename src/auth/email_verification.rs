use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{SessionToken, SessionTokenError, time::format_timestamp};

#[derive(Debug, Error)]
pub enum EmailVerificationError {
    #[error("email verification lifetime must be positive")]
    InvalidPolicy,
    #[error("email verification deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("email is not eligible for verification")]
    EmailNotEligible,
    #[error("secure email verification token operation failed: {0}")]
    Token(#[from] SessionTokenError),
    #[error("email verification persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("email verification audit persistence failed: {0}")]
    Audit(#[from] crate::audit::AuditEventError),
}

#[derive(Clone, Copy)]
pub struct EmailVerificationPolicy {
    lifetime: TimeDelta,
}

impl EmailVerificationPolicy {
    pub fn new(lifetime: TimeDelta) -> Result<Self, EmailVerificationError> {
        if lifetime <= TimeDelta::zero() {
            return Err(EmailVerificationError::InvalidPolicy);
        }
        Ok(Self { lifetime })
    }
}

pub struct EmailVerificationToken(SessionToken);

impl EmailVerificationToken {
    pub fn parse(value: &str) -> Result<Self, SessionTokenError> {
        SessionToken::parse(value).map(Self)
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    fn digest(&self) -> [u8; 32] {
        self.0.digest()
    }
}

pub struct IssuedEmailVerification {
    pub id: String,
    pub token: EmailVerificationToken,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct EmailVerificationStore {
    pool: SqlitePool,
    policy: EmailVerificationPolicy,
}

impl EmailVerificationStore {
    pub fn new(pool: SqlitePool, policy: EmailVerificationPolicy) -> Self {
        Self { pool, policy }
    }

    pub async fn issue(
        &self,
        user_id: &str,
        email_id: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedEmailVerification, EmailVerificationError> {
        let expires_at = now
            .checked_add_signed(self.policy.lifetime)
            .ok_or(EmailVerificationError::DeadlineOutOfRange)?;
        let token = EmailVerificationToken(SessionToken::generate()?);
        let token_hash = token.digest().to_vec();
        let issued_id = Uuid::now_v7().to_string();
        let created_at_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;

        let eligible = sqlx::query_scalar::<_, String>(
            "SELECT user_emails.id \
             FROM user_emails JOIN users ON users.id = user_emails.user_id \
             WHERE user_emails.id = ? AND user_emails.user_id = ? \
               AND user_emails.confirmed_at IS NULL AND users.state = 'active'",
        )
        .bind(email_id)
        .bind(user_id)
        .fetch_optional(&mut **transaction)
        .await?;
        if eligible.is_none() {
            return Err(EmailVerificationError::EmailNotEligible);
        }

        sqlx::query(
            "UPDATE email_verification_tokens SET consumed_at = ? \
             WHERE email_id = ? AND consumed_at IS NULL",
        )
        .bind(&created_at_text)
        .bind(email_id)
        .execute(&mut **transaction)
        .await?;

        sqlx::query(
            "INSERT INTO email_verification_tokens \
             (id, token_hash, email_id, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&issued_id)
        .bind(token_hash)
        .bind(email_id)
        .bind(created_at_text)
        .bind(expires_at_text)
        .execute(&mut **transaction)
        .await?;

        transaction.commit().await?;
        Ok(IssuedEmailVerification {
            id: issued_id,
            token,
            expires_at,
        })
    }

    pub async fn confirm(
        &self,
        token: &EmailVerificationToken,
        now: DateTime<Utc>,
    ) -> Result<bool, EmailVerificationError> {
        let token_hash = token.digest().to_vec();
        let now_text = format_timestamp(now);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let row = sqlx::query(
            "SELECT tokens.id, tokens.email_id, tokens.created_at, tokens.expires_at \
             FROM email_verification_tokens AS tokens \
             JOIN user_emails ON user_emails.id = tokens.email_id \
             JOIN users ON users.id = user_emails.user_id \
             WHERE tokens.token_hash = ? AND tokens.consumed_at IS NULL \
               AND user_emails.confirmed_at IS NULL AND users.state = 'active'",
        )
        .bind(token_hash)
        .fetch_optional(&mut **transaction)
        .await?;

        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let created_at = row.try_get::<String, _>("created_at")?;
        let expires_at = row.try_get::<String, _>("expires_at")?;
        let Some(created_at) = parse_timestamp(&created_at) else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let Some(expires_at) = parse_timestamp(&expires_at) else {
            transaction.rollback().await?;
            return Ok(false);
        };
        if created_at > now || expires_at <= now {
            transaction.rollback().await?;
            return Ok(false);
        }

        let token_id = row.try_get::<String, _>("id")?;
        let email_id = row.try_get::<String, _>("email_id")?;
        let consumed = sqlx::query(
            "UPDATE email_verification_tokens SET consumed_at = ? \
             WHERE id = ? AND consumed_at IS NULL",
        )
        .bind(&now_text)
        .bind(token_id)
        .execute(&mut **transaction)
        .await?;
        if consumed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }

        let confirmed = sqlx::query(
            "UPDATE user_emails SET confirmed_at = ? \
             WHERE id = ? AND confirmed_at IS NULL",
        )
        .bind(now_text)
        .bind(&email_id)
        .execute(&mut **transaction)
        .await?;
        if confirmed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }

        audit::append_in_transaction(
            &mut transaction,
            None,
            AuditEventKind::EmailAddressConfirmed,
            AuditEventResult::Success,
            &AuditEventTarget::email_address(&email_id),
            now,
        )
        .await?;

        transaction.commit().await?;
        Ok(true)
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}
