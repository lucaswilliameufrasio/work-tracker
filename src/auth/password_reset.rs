use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{
    PasswordError, PasswordService, SessionToken, SessionTokenError, normalize_email_address,
    time::format_timestamp,
};

#[derive(Debug, Error)]
pub enum PasswordResetError {
    #[error("password reset lifetime must be positive")]
    InvalidPolicy,
    #[error("password reset deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("secure password reset token operation failed: {0}")]
    Token(#[from] SessionTokenError),
    #[error("password reset rejected by local password policy or KDF admission: {0}")]
    Password(#[from] PasswordError),
    #[error("password reset persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("password reset audit persistence failed: {0}")]
    Audit(#[from] crate::audit::AuditEventError),
}

#[derive(Clone, Copy)]
pub struct PasswordResetPolicy {
    lifetime: TimeDelta,
}

impl PasswordResetPolicy {
    pub fn new(lifetime: TimeDelta) -> Result<Self, PasswordResetError> {
        if lifetime <= TimeDelta::zero() {
            return Err(PasswordResetError::InvalidPolicy);
        }
        Ok(Self { lifetime })
    }
}

pub struct PasswordResetToken(SessionToken);

impl PasswordResetToken {
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

pub struct IssuedPasswordReset {
    pub id: String,
    pub token: PasswordResetToken,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordResetResult {
    pub revoked_sessions: u64,
}

#[derive(Clone)]
pub struct PasswordResetStore {
    pool: SqlitePool,
    password_service: PasswordService,
    policy: PasswordResetPolicy,
}

impl PasswordResetStore {
    pub fn new(
        pool: SqlitePool,
        password_service: PasswordService,
        policy: PasswordResetPolicy,
    ) -> Self {
        Self {
            pool,
            password_service,
            policy,
        }
    }

    /// Creates a single pending token for a confirmed email with an active
    /// account and an existing local password. Unknown/ineligible addresses
    /// return `None`; public callers must return the same response either way.
    pub async fn issue_for_email(
        &self,
        email_address: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<IssuedPasswordReset>, PasswordResetError> {
        let Ok(email) = normalize_email_address(email_address) else {
            return Ok(None);
        };
        let expires_at = now
            .checked_add_signed(self.policy.lifetime)
            .ok_or(PasswordResetError::DeadlineOutOfRange)?;
        let token = PasswordResetToken(SessionToken::generate()?);
        let token_hash = token.digest().to_vec();
        let token_id = Uuid::now_v7().to_string();
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let eligible = sqlx::query(
            "SELECT user_emails.id AS email_id, user_emails.user_id \
             FROM user_emails \
             JOIN users ON users.id = user_emails.user_id \
             JOIN password_credentials ON password_credentials.user_id = users.id \
             WHERE user_emails.address_key = ? \
               AND user_emails.confirmed_at IS NOT NULL \
               AND users.state = 'active'",
        )
        .bind(&email.key)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(eligible) = eligible else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let email_id: String = eligible.try_get("email_id")?;
        let user_id: String = eligible.try_get("user_id")?;

        sqlx::query(
            "UPDATE password_reset_tokens SET invalidated_at = ? \
             WHERE email_id = ? AND consumed_at IS NULL AND invalidated_at IS NULL",
        )
        .bind(&now_text)
        .bind(&email_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO password_reset_tokens \
             (id, token_hash, user_id, email_id, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&token_id)
        .bind(token_hash)
        .bind(user_id)
        .bind(email_id)
        .bind(now_text)
        .bind(expires_at_text)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(Some(IssuedPasswordReset {
            id: token_id,
            token,
            expires_at,
        }))
    }

    /// Resets a password and revokes all existing sessions in the same
    /// transaction that consumes the token. The endpoint should not reveal
    /// whether an invalid, expired, consumed, or unknown token was supplied.
    pub async fn reset_password(
        &self,
        token: &PasswordResetToken,
        new_password: String,
        now: DateTime<Utc>,
    ) -> Result<Option<PasswordResetResult>, PasswordResetError> {
        let token_hash = token.digest().to_vec();
        let Some(prechecked_row) = self.load_eligible_token(&token_hash).await? else {
            return Ok(None);
        };
        if !prechecked_row.is_valid_at(now) {
            return Ok(None);
        }

        let new_password_hash = self.password_service.hash(new_password).await?;
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current_row) = self
            .load_eligible_token_in_transaction(&mut transaction, &token_hash)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if !current_row.is_valid_at(now) {
            transaction.rollback().await?;
            return Ok(None);
        }

        let consumed = sqlx::query(
            "UPDATE password_reset_tokens SET consumed_at = ? \
             WHERE id = ? AND token_hash = ? \
               AND consumed_at IS NULL AND invalidated_at IS NULL",
        )
        .bind(&now_text)
        .bind(&current_row.id)
        .bind(&token_hash)
        .execute(&mut *transaction)
        .await?;
        if consumed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }

        let updated_credential = sqlx::query(
            "UPDATE password_credentials SET password_hash = ?, changed_at = ? \
             WHERE user_id = ? AND EXISTS ( \
                 SELECT 1 FROM users JOIN user_emails \
                   ON user_emails.user_id = users.id \
                 WHERE users.id = password_credentials.user_id \
                   AND users.state = 'active' \
                   AND user_emails.id = ? \
                   AND user_emails.confirmed_at IS NOT NULL)",
        )
        .bind(new_password_hash)
        .bind(&now_text)
        .bind(&current_row.user_id)
        .bind(&current_row.email_id)
        .execute(&mut *transaction)
        .await?;
        if updated_credential.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }

        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ? \
             WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(&now_text)
        .bind(&current_row.user_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE password_reset_tokens SET invalidated_at = ? \
             WHERE user_id = ? AND id != ? \
               AND consumed_at IS NULL AND invalidated_at IS NULL",
        )
        .bind(&now_text)
        .bind(&current_row.user_id)
        .bind(&current_row.id)
        .execute(&mut *transaction)
        .await?;

        audit::append_in_transaction(
            &mut transaction,
            None,
            AuditEventKind::PasswordResetCompleted,
            AuditEventResult::Success,
            &AuditEventTarget::user(&current_row.user_id),
            now,
        )
        .await?;

        transaction.commit().await?;
        Ok(Some(PasswordResetResult {
            revoked_sessions: revoked.rows_affected(),
        }))
    }

    async fn load_eligible_token(
        &self,
        token_hash: &[u8],
    ) -> Result<Option<PasswordResetRow>, PasswordResetError> {
        let row = sqlx::query(
            "SELECT tokens.id, tokens.user_id, tokens.email_id, \
                    tokens.created_at, tokens.expires_at \
             FROM password_reset_tokens AS tokens \
             JOIN users ON users.id = tokens.user_id \
             JOIN user_emails ON user_emails.id = tokens.email_id \
                            AND user_emails.user_id = tokens.user_id \
             JOIN password_credentials ON password_credentials.user_id = users.id \
             WHERE tokens.token_hash = ? AND tokens.consumed_at IS NULL \
               AND tokens.invalidated_at IS NULL AND users.state = 'active' \
               AND user_emails.confirmed_at IS NOT NULL",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(PasswordResetRow::from_row)
            .transpose()
            .map_err(PasswordResetError::Database)
    }

    async fn load_eligible_token_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        token_hash: &[u8],
    ) -> Result<Option<PasswordResetRow>, PasswordResetError> {
        let row = sqlx::query(
            "SELECT tokens.id, tokens.user_id, tokens.email_id, \
                    tokens.created_at, tokens.expires_at \
             FROM password_reset_tokens AS tokens \
             JOIN users ON users.id = tokens.user_id \
             JOIN user_emails ON user_emails.id = tokens.email_id \
                            AND user_emails.user_id = tokens.user_id \
             JOIN password_credentials ON password_credentials.user_id = users.id \
             WHERE tokens.token_hash = ? AND tokens.consumed_at IS NULL \
               AND tokens.invalidated_at IS NULL AND users.state = 'active' \
               AND user_emails.confirmed_at IS NOT NULL",
        )
        .bind(token_hash)
        .fetch_optional(&mut **transaction)
        .await?;
        row.map(PasswordResetRow::from_row)
            .transpose()
            .map_err(PasswordResetError::Database)
    }
}

struct PasswordResetRow {
    id: String,
    user_id: String,
    email_id: String,
    created_at: String,
    expires_at: String,
}

impl PasswordResetRow {
    fn from_row(row: sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            email_id: row.try_get("email_id")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
        })
    }

    fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        let Ok(created_at) = DateTime::parse_from_rfc3339(&self.created_at) else {
            return false;
        };
        let Ok(expires_at) = DateTime::parse_from_rfc3339(&self.expires_at) else {
            return false;
        };
        let created_at = created_at.with_timezone(&Utc);
        let expires_at = expires_at.with_timezone(&Utc);
        created_at <= now && expires_at > now && expires_at > created_at
    }
}
