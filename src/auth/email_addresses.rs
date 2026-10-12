use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{
    EmailAddressError, normalize_email_address, reauthentication::RecentReauthentication,
    time::format_timestamp,
};

#[derive(Debug, Error)]
pub enum EmailAddressManagementError {
    #[error("email address is invalid")]
    InvalidAddress(#[from] EmailAddressError),
    #[error("account is not eligible to manage email addresses")]
    UserNotActive,
    #[error("email address is already associated with an account")]
    AddressUnavailable,
    #[error("the final email address cannot be removed")]
    LastAddress,
    #[error("the final confirmed email address cannot be removed")]
    LastConfirmedAddress,
    #[error("a recent password reauthentication is required")]
    ReauthenticationRequired,
    #[error("installation-admin email changes require a recent MFA verification")]
    MfaRequired,
    #[error("persisted email address state is malformed")]
    CorruptState,
    #[error("email address persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddressRecord {
    pub id: String,
    pub address: String,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub is_primary: bool,
}

#[derive(Clone)]
pub struct EmailAddressStore {
    pool: SqlitePool,
}

impl EmailAddressStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Adds an unconfirmed address. Callers must require recent
    /// reauthentication before invoking this sensitive account operation.
    pub async fn add_pending(
        &self,
        proof: &RecentReauthentication,
        address: &str,
        now: DateTime<Utc>,
    ) -> Result<EmailAddressRecord, EmailAddressManagementError> {
        let email = normalize_email_address(address)?;
        let user_id = proof.user_id.as_str();
        let email_id = Uuid::now_v7().to_string();
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_active_user(&mut transaction, user_id).await?;
        self.require_recent_reauthentication(&mut transaction, proof, user_id, now)
            .await?;
        let already_used = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM user_emails WHERE address_key = ?)",
        )
        .bind(&email.key)
        .fetch_one(&mut *transaction)
        .await?;
        if already_used != 0 {
            transaction.rollback().await?;
            return Err(EmailAddressManagementError::AddressUnavailable);
        }

        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES (?, ?, ?, ?, 0, NULL, ?)",
        )
        .bind(&email_id)
        .bind(user_id)
        .bind(&email.address)
        .bind(&email.key)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(user_id),
            AuditEventKind::EmailAddressAdded,
            AuditEventResult::Success,
            &AuditEventTarget::email_address(&email_id),
            now,
        )
        .await
        .map_err(map_audit_error)?;
        transaction.commit().await?;
        Ok(EmailAddressRecord {
            id: email_id,
            address: email.address,
            confirmed_at: None,
            is_primary: false,
        })
    }

    pub async fn list_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<EmailAddressRecord>, EmailAddressManagementError> {
        let rows = sqlx::query(
            "SELECT id, address, confirmed_at, is_primary FROM user_emails \
             WHERE user_id = ? ORDER BY is_primary DESC, created_at ASC, id ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        let mut addresses = Vec::with_capacity(rows.len());
        for row in rows {
            let confirmed_at = match row.try_get::<Option<String>, _>("confirmed_at")? {
                Some(value) => {
                    let timestamp = DateTime::parse_from_rfc3339(&value)
                        .map_err(|_| EmailAddressManagementError::CorruptState)?
                        .with_timezone(&Utc);
                    Some(timestamp)
                }
                None => None,
            };
            addresses.push(EmailAddressRecord {
                id: row.try_get("id")?,
                address: row.try_get("address")?,
                confirmed_at,
                is_primary: row.try_get::<i64, _>("is_primary")? == 1,
            });
        }
        Ok(addresses)
    }

    /// Promotes only an already-confirmed address and changes the primary
    /// pointer atomically. The caller must require recent reauthentication.
    pub async fn set_primary(
        &self,
        proof: &RecentReauthentication,
        email_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, EmailAddressManagementError> {
        let user_id = proof.user_id.as_str();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_active_user(&mut transaction, user_id).await?;
        self.require_recent_reauthentication(&mut transaction, proof, user_id, now)
            .await?;
        let confirmed = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM user_emails \
             WHERE id = ? AND user_id = ? AND confirmed_at IS NOT NULL)",
        )
        .bind(email_id)
        .bind(user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if confirmed == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }

        sqlx::query("UPDATE user_emails SET is_primary = 0 WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;
        let promoted = sqlx::query(
            "UPDATE user_emails SET is_primary = 1 \
             WHERE id = ? AND user_id = ? AND confirmed_at IS NOT NULL",
        )
        .bind(email_id)
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;
        if promoted.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        audit::append_in_transaction(
            &mut transaction,
            Some(user_id),
            AuditEventKind::EmailAddressPrimaryChanged,
            AuditEventResult::Success,
            &AuditEventTarget::email_address(email_id),
            now,
        )
        .await
        .map_err(map_audit_error)?;
        transaction.commit().await?;
        Ok(true)
    }

    /// Removes an owned address and cascades its pending verification/reset
    /// tokens. A primary removal promotes another confirmed address first.
    /// The caller must require recent reauthentication.
    pub async fn remove(
        &self,
        proof: &RecentReauthentication,
        email_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, EmailAddressManagementError> {
        let user_id = proof.user_id.as_str();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_active_user(&mut transaction, user_id).await?;
        self.require_recent_reauthentication(&mut transaction, proof, user_id, now)
            .await?;
        let target = sqlx::query(
            "SELECT is_primary, confirmed_at FROM user_emails \
             WHERE id = ? AND user_id = ?",
        )
        .bind(email_id)
        .bind(user_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(target) = target else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let is_primary: i64 = target.try_get("is_primary")?;
        let is_confirmed = target
            .try_get::<Option<String>, _>("confirmed_at")?
            .is_some();
        let total_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM user_emails WHERE user_id = ?")
                .bind(user_id)
                .fetch_one(&mut *transaction)
                .await?;
        if total_count <= 1 {
            transaction.rollback().await?;
            return Err(EmailAddressManagementError::LastAddress);
        }
        if is_confirmed {
            let confirmed_count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM user_emails \
                 WHERE user_id = ? AND confirmed_at IS NOT NULL",
            )
            .bind(user_id)
            .fetch_one(&mut *transaction)
            .await?;
            if confirmed_count <= 1 {
                transaction.rollback().await?;
                return Err(EmailAddressManagementError::LastConfirmedAddress);
            }
        }

        if is_primary == 1 {
            let replacement = sqlx::query_scalar::<_, String>(
                "SELECT id FROM user_emails \
                 WHERE user_id = ? AND id != ? AND confirmed_at IS NOT NULL \
                 ORDER BY created_at ASC, id ASC LIMIT 1",
            )
            .bind(user_id)
            .bind(email_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(replacement) = replacement else {
                transaction.rollback().await?;
                return Err(EmailAddressManagementError::LastConfirmedAddress);
            };
            sqlx::query("UPDATE user_emails SET is_primary = 0 WHERE user_id = ?")
                .bind(user_id)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("UPDATE user_emails SET is_primary = 1 WHERE id = ? AND user_id = ?")
                .bind(replacement)
                .bind(user_id)
                .execute(&mut *transaction)
                .await?;
        }

        let removed = sqlx::query("DELETE FROM user_emails WHERE id = ? AND user_id = ?")
            .bind(email_id)
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;
        if removed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        audit::append_in_transaction(
            &mut transaction,
            Some(user_id),
            AuditEventKind::EmailAddressRemoved,
            AuditEventResult::Success,
            &AuditEventTarget::email_address(email_id),
            now,
        )
        .await
        .map_err(map_audit_error)?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn require_active_user(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        user_id: &str,
    ) -> Result<(), EmailAddressManagementError> {
        let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut **transaction)
            .await?;
        if state.as_deref() != Some("active") {
            return Err(EmailAddressManagementError::UserNotActive);
        }
        Ok(())
    }

    async fn require_recent_reauthentication(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        proof: &RecentReauthentication,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), EmailAddressManagementError> {
        if proof.user_id != user_id || !proof.is_current_at(now) {
            return Err(EmailAddressManagementError::ReauthenticationRequired);
        }
        let active_session = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS( \
                 SELECT 1 FROM sessions \
                 JOIN users ON users.id = sessions.user_id \
                 WHERE sessions.id = ? AND sessions.user_id = ? \
                   AND sessions.token_hash = ? \
                   AND sessions.reauthenticated_at = ? \
                   AND sessions.revoked_at IS NULL AND users.state = 'active')",
        )
        .bind(&proof.session_id)
        .bind(user_id)
        .bind(&proof.token_digest)
        .bind(format_timestamp(proof.authenticated_at))
        .fetch_one(&mut **transaction)
        .await?;
        if active_session != 1 {
            return Err(EmailAddressManagementError::ReauthenticationRequired);
        }

        let installation_admin = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM installation_admins WHERE user_id = ?)",
        )
        .bind(user_id)
        .fetch_one(&mut **transaction)
        .await?;
        if installation_admin == 1 {
            return Err(EmailAddressManagementError::MfaRequired);
        }
        Ok(())
    }
}

fn map_audit_error(error: audit::AuditEventError) -> EmailAddressManagementError {
    match error {
        audit::AuditEventError::Database(error) => EmailAddressManagementError::Database(error),
    }
}
