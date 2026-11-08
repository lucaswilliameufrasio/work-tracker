use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{PasswordError, PasswordService, normalize_email_address, time::format_timestamp};

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("user is not eligible for a password credential")]
    UserNotActive,
    #[error("password operation failed: {0}")]
    Password(#[from] PasswordError),
    #[error("credential persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("credential audit persistence failed: {0}")]
    Audit(#[from] crate::audit::AuditEventError),
}

pub(crate) struct VerifiedEmailCredential {
    pub(crate) user_id: String,
    pub(crate) email_key: String,
    pub(crate) password_hash: String,
}

#[derive(Clone)]
pub struct CredentialStore {
    pool: SqlitePool,
    password_service: PasswordService,
}

impl CredentialStore {
    pub fn new(pool: SqlitePool, password_service: PasswordService) -> Self {
        Self {
            pool,
            password_service,
        }
    }

    /// Sets or replaces a password and revokes every other session atomically.
    /// The optional session ID is preserved for an authenticated password change;
    /// reset/setup callers should pass `None` to revoke every session.
    pub async fn set_password(
        &self,
        actor_user_id: Option<&str>,
        user_id: &str,
        password: String,
        now: DateTime<Utc>,
        preserve_session_id: Option<&str>,
    ) -> Result<u64, CredentialError> {
        let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        if state.as_deref() != Some("active") {
            return Err(CredentialError::UserNotActive);
        }

        let password_hash = self.password_service.hash(password).await?;
        let changed_at = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *transaction)
            .await?;
        if state.as_deref() != Some("active") {
            return Err(CredentialError::UserNotActive);
        }

        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT (user_id) DO UPDATE SET \
                 password_hash = excluded.password_hash, \
                 changed_at = excluded.changed_at",
        )
        .bind(user_id)
        .bind(password_hash)
        .bind(&changed_at)
        .execute(&mut *transaction)
        .await?;

        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ? \
             WHERE user_id = ? AND revoked_at IS NULL \
               AND (? IS NULL OR id != ?)",
        )
        .bind(changed_at)
        .bind(user_id)
        .bind(preserve_session_id)
        .bind(preserve_session_id)
        .execute(&mut *transaction)
        .await?;

        audit::append_in_transaction(
            &mut transaction,
            actor_user_id,
            AuditEventKind::PasswordChanged,
            AuditEventResult::Success,
            &AuditEventTarget::user(user_id),
            now,
        )
        .await?;

        transaction.commit().await?;
        Ok(revoked.rows_affected())
    }

    pub async fn password_hash(&self, user_id: &str) -> Result<Option<String>, CredentialError> {
        sqlx::query_scalar(
            "SELECT credentials.password_hash \
             FROM password_credentials AS credentials \
             JOIN users ON users.id = credentials.user_id \
             WHERE credentials.user_id = ? AND users.state = 'active'",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(CredentialError::Database)
    }

    /// Verifies a user credential without distinguishing missing/inactive users
    /// from a wrong password. `dummy_hash` must use the currently supported PHC
    /// profile and should be generated once for the running application.
    pub async fn verify_password_for_user(
        &self,
        user_id: &str,
        password: String,
        dummy_hash: &str,
    ) -> Result<bool, CredentialError> {
        Ok(self
            .verify_password_hash_for_user(user_id, password, dummy_hash)
            .await?
            .is_some())
    }

    pub(crate) async fn verify_password_hash_for_user(
        &self,
        user_id: &str,
        password: String,
        dummy_hash: &str,
    ) -> Result<Option<String>, CredentialError> {
        let row = sqlx::query(
            "SELECT users.state, credentials.password_hash \
             FROM users \
             LEFT JOIN password_credentials AS credentials \
               ON credentials.user_id = users.id \
             WHERE users.id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        };

        let state: String = row.try_get("state")?;
        let password_hash: Option<String> = row.try_get("password_hash")?;
        if state != "active" {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        }

        let Some(password_hash) = password_hash else {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        };

        let verified = self
            .password_service
            .verify(password, password_hash.clone())
            .await
            .map_err(CredentialError::Password)?;
        if !verified {
            return Ok(None);
        }
        Ok(Some(password_hash))
    }

    /// Resolves only confirmed email addresses; every non-match still performs
    /// one dummy KDF and returns the same authentication result.
    pub async fn verify_password_for_email(
        &self,
        email_address: &str,
        password: String,
        dummy_hash: &str,
    ) -> Result<bool, CredentialError> {
        Ok(self
            .verify_email_credential(email_address, password, dummy_hash)
            .await?
            .is_some())
    }

    pub(crate) async fn verify_email_credential(
        &self,
        email_address: &str,
        password: String,
        dummy_hash: &str,
    ) -> Result<Option<VerifiedEmailCredential>, CredentialError> {
        let Ok(email) = normalize_email_address(email_address) else {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT users.id, users.state, credentials.password_hash \
             FROM user_emails \
             JOIN users ON users.id = user_emails.user_id \
             LEFT JOIN password_credentials AS credentials \
               ON credentials.user_id = users.id \
             WHERE user_emails.address_key = ? \
               AND user_emails.confirmed_at IS NOT NULL",
        )
        .bind(&email.key)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        };
        let user_id: String = row.try_get("id")?;
        let state: String = row.try_get("state")?;
        let password_hash: Option<String> = row.try_get("password_hash")?;
        if state != "active" {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        }
        let Some(password_hash) = password_hash else {
            self.verify_dummy(password, dummy_hash).await?;
            return Ok(None);
        };

        if !self
            .password_service
            .verify(password, password_hash.clone())
            .await?
        {
            return Ok(None);
        }

        Ok(Some(VerifiedEmailCredential {
            user_id,
            email_key: email.key,
            password_hash,
        }))
    }

    async fn verify_dummy(
        &self,
        password: String,
        dummy_hash: &str,
    ) -> Result<bool, CredentialError> {
        self.password_service
            .verify(password, dummy_hash.to_owned())
            .await?;
        Ok(false)
    }
}
