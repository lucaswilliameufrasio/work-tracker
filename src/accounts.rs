use chrono::{DateTime, Utc};
use sqlx::{SqlitePool, Transaction};
use thiserror::Error;

use crate::{
    access::{AccessCapability, AccessStore, AccountState, authorize},
    audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget},
    auth::time::format_timestamp,
};

const MAX_BAN_REASON_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatusChange {
    Changed,
    Unchanged,
}

#[derive(Debug, Error)]
pub enum AccountAdministrationError {
    #[error("actor cannot ban or restore installation accounts")]
    Forbidden,
    #[error("target account does not exist")]
    TargetNotFound,
    #[error("only an active account can be banned")]
    TargetNotActive,
    #[error("target account is not currently banned")]
    TargetNotBanned,
    #[error("persisted account state is invalid")]
    CorruptState,
    #[error("ban reason must be non-empty, single-line, and at most 500 characters")]
    InvalidReason,
    #[error("account access context could not be loaded: {0}")]
    Access(#[from] crate::access::AccessStoreError),
    #[error("account lifecycle persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("account lifecycle audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

/// Internal, transactionally audited account ban and restoration operations.
/// Callers must pass an actor ID from a validated session; the store reloads
/// and rechecks the current installation-admin grant in its write transaction.
/// Transport and administrator-facing audit browsing are separate boundaries.
#[derive(Clone)]
pub struct AccountAdministrationStore {
    pool: SqlitePool,
}

impl AccountAdministrationStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Bans an active account, revoking all persisted bearer credentials that
    /// could otherwise become valid again after a later unban. Memberships and
    /// authorship are preserved; projects derive write access from the
    /// presence of an active manager.
    pub async fn ban(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountStatusChange, AccountAdministrationError> {
        let reason = validate_ban_reason(reason)?;
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_installation_admin(&mut transaction, actor_user_id).await?;

        let state = load_account_state(&mut transaction, target_user_id).await?;
        match state {
            AccountState::Banned => {
                invalidate_account_credentials(&mut transaction, target_user_id, &now_text).await?;
                transaction.commit().await?;
                return Ok(AccountStatusChange::Unchanged);
            }
            AccountState::Active => {}
            AccountState::Disabled => {
                transaction.rollback().await?;
                return Err(AccountAdministrationError::TargetNotActive);
            }
        }

        let update = sqlx::query(
            "UPDATE users SET state = 'banned', updated_at = ? \
             WHERE id = ? AND state = 'active'",
        )
        .bind(&now_text)
        .bind(target_user_id)
        .execute(&mut *transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::TargetNotActive);
        }

        invalidate_account_credentials(&mut transaction, target_user_id, &now_text).await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::UserBanned,
            AuditEventResult::Success,
            &AuditEventTarget::user_ban(target_user_id, reason),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(AccountStatusChange::Changed)
    }

    /// Restores only a banned account. Revoked sessions and consumed or
    /// invalidated one-time credentials are intentionally never recreated.
    pub async fn unban(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountStatusChange, AccountAdministrationError> {
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_installation_admin(&mut transaction, actor_user_id).await?;

        let state = load_account_state(&mut transaction, target_user_id).await?;
        match state {
            AccountState::Banned => {}
            AccountState::Active => {
                transaction.rollback().await?;
                return Ok(AccountStatusChange::Unchanged);
            }
            AccountState::Disabled => {
                transaction.rollback().await?;
                return Err(AccountAdministrationError::TargetNotBanned);
            }
        }

        let update = sqlx::query(
            "UPDATE users SET state = 'active', updated_at = ? \
             WHERE id = ? AND state = 'banned'",
        )
        .bind(&now_text)
        .bind(target_user_id)
        .execute(&mut *transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::TargetNotBanned);
        }

        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::UserUnbanned,
            AuditEventResult::Success,
            &AuditEventTarget::user(target_user_id),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(AccountStatusChange::Changed)
    }
}

async fn require_installation_admin(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
) -> Result<(), AccountAdministrationError> {
    let context =
        AccessStore::load_organization_context_in_transaction(transaction, actor_user_id, false)
            .await?;
    let Some(context) = context else {
        return Err(AccountAdministrationError::Forbidden);
    };
    if authorize(&context, AccessCapability::BanInstallationUser).is_err() {
        return Err(AccountAdministrationError::Forbidden);
    }
    Ok(())
}

async fn load_account_state(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<AccountState, AccountAdministrationError> {
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(AccountAdministrationError::TargetNotFound)?;
    match state.as_str() {
        "active" => Ok(AccountState::Active),
        "banned" => Ok(AccountState::Banned),
        "disabled" => Ok(AccountState::Disabled),
        _ => Err(AccountAdministrationError::CorruptState),
    }
}

async fn invalidate_account_credentials(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sessions SET revoked_at = ? \
         WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE email_verification_tokens SET consumed_at = ? \
         WHERE consumed_at IS NULL AND email_id IN ( \
             SELECT id FROM user_emails WHERE user_id = ? \
         )",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE password_reset_tokens SET invalidated_at = ? \
         WHERE user_id = ? AND consumed_at IS NULL AND invalidated_at IS NULL",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE recovery_codes SET invalidated_at = ? \
         WHERE user_id = ? AND consumed_at IS NULL AND invalidated_at IS NULL",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query("DELETE FROM mfa_login_challenges WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;

    sqlx::query(
        "DELETE FROM mfa_recovery_enrollments \
         WHERE recovery_session_id IN ( \
             SELECT id FROM mfa_recovery_sessions \
             WHERE user_id = ? \
         )",
    )
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE mfa_recovery_sessions SET invalidated_at = ? \
         WHERE user_id = ? AND completed_at IS NULL AND invalidated_at IS NULL",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query("DELETE FROM mfa_credentials WHERE user_id = ? AND activated_at IS NULL")
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;

    Ok(())
}

fn validate_ban_reason(reason: &str) -> Result<&str, AccountAdministrationError> {
    let reason = reason.trim();
    if reason.is_empty()
        || reason.chars().count() > MAX_BAN_REASON_CHARS
        || reason.chars().any(char::is_control)
    {
        return Err(AccountAdministrationError::InvalidReason);
    }
    Ok(reason)
}
