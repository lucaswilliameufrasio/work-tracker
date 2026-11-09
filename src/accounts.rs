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
    #[error("actor is not authorized for this account lifecycle operation")]
    Forbidden,
    #[error("target account does not exist")]
    TargetNotFound,
    #[error("target account is not active")]
    TargetNotActive,
    #[error("target account is not currently banned")]
    TargetNotBanned,
    #[error("target account is not currently disabled")]
    TargetNotDisabled,
    #[error(
        "ensure another active organization owner before disabling or anonymizing this account"
    )]
    LastOrganizationOwner,
    #[error(
        "ensure another active installation admin before disabling or anonymizing this account"
    )]
    LastInstallationAdmin,
    #[error("ensure another active project manager before disabling or anonymizing this account")]
    LastProjectManager,
    #[error("an anonymized account cannot be restored or returned to active state")]
    AnonymizedAccount,
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

/// Internal, transactionally audited account lifecycle operations.
/// Callers must pass an actor ID from a validated session; the store reloads
/// and rechecks current account and installation-admin state in its write
/// transaction. Transport and administrator-facing audit browsing are
/// separate boundaries.
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
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        require_installation_admin(
            &mut transaction,
            actor_user_id,
            AccessCapability::BanInstallationUser,
        )
        .await?;

        let state = load_account_state(&mut transaction, target_user_id).await?;
        if is_account_anonymized(&mut transaction, target_user_id).await? {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::AnonymizedAccount);
        }
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
        .execute(&mut **transaction)
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
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        require_installation_admin(
            &mut transaction,
            actor_user_id,
            AccessCapability::BanInstallationUser,
        )
        .await?;

        let state = load_account_state(&mut transaction, target_user_id).await?;
        if is_account_anonymized(&mut transaction, target_user_id).await? {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::AnonymizedAccount);
        }
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
        .execute(&mut **transaction)
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

    /// Disables only the account identified by the caller's validated session.
    /// The account's memberships and authored work remain, but the transition
    /// is refused if it would leave the organization without an owner, the
    /// installation without an active administrator, or a managed project
    /// without an active manager.
    pub async fn disable_self(
        &self,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountStatusChange, AccountAdministrationError> {
        let now_text = format_timestamp(now);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let context =
            AccessStore::load_organization_context_in_transaction(&mut transaction, user_id, false)
                .await?;
        let Some(context) = context else {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::TargetNotFound);
        };

        match context.account_state() {
            AccountState::Active => {
                if authorize(&context, AccessCapability::DeactivateOwnAccount).is_err() {
                    transaction.rollback().await?;
                    return Err(AccountAdministrationError::TargetNotActive);
                }
            }
            AccountState::Disabled => {
                transaction.rollback().await?;
                return Ok(AccountStatusChange::Unchanged);
            }
            AccountState::Banned => {
                transaction.rollback().await?;
                return Err(AccountAdministrationError::TargetNotActive);
            }
        }

        ensure_deactivation_preserves_responsible_accounts(&mut transaction, user_id).await?;
        let update = sqlx::query(
            "UPDATE users SET state = 'disabled', updated_at = ? \
             WHERE id = ? AND state = 'active'",
        )
        .bind(&now_text)
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::TargetNotActive);
        }

        invalidate_account_credentials(&mut transaction, user_id, &now_text).await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(user_id),
            AuditEventKind::UserDisabled,
            AuditEventResult::Success,
            &AuditEventTarget::user(user_id),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(AccountStatusChange::Changed)
    }

    /// Reactivates a disabled account for an active installation admin.
    /// Revoked sessions and one-time credentials are not restored.
    pub async fn reactivate(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountStatusChange, AccountAdministrationError> {
        let now_text = format_timestamp(now);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        require_installation_admin(
            &mut transaction,
            actor_user_id,
            AccessCapability::ReactivateInstallationUser,
        )
        .await?;

        let state = load_account_state(&mut transaction, target_user_id).await?;
        if is_account_anonymized(&mut transaction, target_user_id).await? {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::AnonymizedAccount);
        }
        match state {
            AccountState::Disabled => {}
            AccountState::Active => {
                transaction.rollback().await?;
                return Ok(AccountStatusChange::Unchanged);
            }
            AccountState::Banned => {
                transaction.rollback().await?;
                return Err(AccountAdministrationError::TargetNotDisabled);
            }
        }

        let update = sqlx::query(
            "UPDATE users SET state = 'active', updated_at = ? \
             WHERE id = ? AND state = 'disabled'",
        )
        .bind(&now_text)
        .bind(target_user_id)
        .execute(&mut **transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::TargetNotDisabled);
        }

        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::UserReactivated,
            AuditEventResult::Success,
            &AuditEventTarget::user(target_user_id),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(AccountStatusChange::Changed)
    }

    /// Irreversibly removes login and contact identities while retaining the
    /// account row, memberships, and authored-work references.
    pub async fn anonymize(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountStatusChange, AccountAdministrationError> {
        let now_text = format_timestamp(now);
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let actor_context = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?;
        let Some(actor_context) = actor_context else {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::Forbidden);
        };
        if authorize(
            &actor_context,
            AccessCapability::AnonymizeInstallationAccount,
        )
        .is_err()
            || actor_user_id == target_user_id
        {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::Forbidden);
        }

        load_account_state(&mut transaction, target_user_id).await?;
        if is_account_anonymized(&mut transaction, target_user_id).await? {
            transaction.rollback().await?;
            return Ok(AccountStatusChange::Unchanged);
        }
        ensure_deactivation_preserves_responsible_accounts(&mut transaction, target_user_id)
            .await?;

        redact_organization_invite_recipients(&mut transaction, target_user_id, &now_text).await?;
        let update = sqlx::query(
            "UPDATE users \
             SET display_name = 'Usuário removido', state = 'disabled', \
                 anonymized_at = ?, updated_at = ? \
             WHERE id = ? AND anonymized_at IS NULL",
        )
        .bind(&now_text)
        .bind(&now_text)
        .bind(target_user_id)
        .execute(&mut **transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AccountAdministrationError::AnonymizedAccount);
        }

        delete_account_credentials(&mut transaction, target_user_id).await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::UserAnonymized,
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
    capability: AccessCapability,
) -> Result<(), AccountAdministrationError> {
    let context =
        AccessStore::load_organization_context_in_transaction(transaction, actor_user_id, false)
            .await?;
    let Some(context) = context else {
        return Err(AccountAdministrationError::Forbidden);
    };
    if authorize(&context, capability).is_err() {
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

async fn is_account_anonymized(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<bool, sqlx::Error> {
    let anonymized_at: Option<String> =
        sqlx::query_scalar("SELECT anonymized_at FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_one(&mut **transaction)
            .await?;
    Ok(anonymized_at.is_some())
}

async fn redact_organization_invite_recipients(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE organization_invites \
         SET email = 'redacted+' || id || '@invalid.invalid', \
             email_key = lower(trim('redacted+' || id || '@invalid.invalid')), \
             recipient_redacted_at = ? \
         WHERE recipient_redacted_at IS NULL \
           AND email_key IN (SELECT address_key FROM user_emails WHERE user_id = ?) \
           AND (accepted_at IS NOT NULL OR canceled_at IS NOT NULL)",
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE organization_invites \
         SET email = 'redacted+' || id || '@invalid.invalid', \
             email_key = lower(trim('redacted+' || id || '@invalid.invalid')), \
             recipient_redacted_at = ?, canceled_at = ? \
         WHERE recipient_redacted_at IS NULL \
           AND email_key IN (SELECT address_key FROM user_emails WHERE user_id = ?) \
           AND accepted_at IS NULL AND canceled_at IS NULL",
    )
    .bind(now)
    .bind(now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn delete_account_credentials(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    for query in [
        "DELETE FROM mfa_login_challenges WHERE user_id = ?",
        "DELETE FROM mfa_recovery_enrollments \
         WHERE recovery_session_id IN ( \
             SELECT id FROM mfa_recovery_sessions WHERE user_id = ? \
         )",
        "DELETE FROM mfa_recovery_sessions WHERE user_id = ?",
        "DELETE FROM mfa_credentials WHERE user_id = ?",
        "DELETE FROM recovery_codes WHERE user_id = ?",
        "DELETE FROM sessions WHERE user_id = ?",
        "DELETE FROM password_reset_tokens WHERE user_id = ?",
        "DELETE FROM email_verification_tokens \
         WHERE email_id IN (SELECT id FROM user_emails WHERE user_id = ?)",
        "DELETE FROM password_credentials WHERE user_id = ?",
        "DELETE FROM auth_identities WHERE user_id = ?",
        "DELETE FROM user_emails WHERE user_id = ?",
        "DELETE FROM installation_admins WHERE user_id = ?",
    ] {
        sqlx::query(query)
            .bind(user_id)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
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

async fn ensure_deactivation_preserves_responsible_accounts(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<(), AccountAdministrationError> {
    let is_organization_owner: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM organization_members \
             WHERE organization_id = 1 AND user_id = ? AND role = 'owner' \
         )",
    )
    .bind(user_id)
    .fetch_one(&mut **transaction)
    .await?;
    if is_organization_owner {
        let other_active_owners: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM organization_members \
             JOIN users ON users.id = organization_members.user_id \
             WHERE organization_members.organization_id = 1 \
               AND organization_members.role = 'owner' \
               AND users.state = 'active' \
               AND organization_members.user_id != ?",
        )
        .bind(user_id)
        .fetch_one(&mut **transaction)
        .await?;
        if other_active_owners == 0 {
            return Err(AccountAdministrationError::LastOrganizationOwner);
        }
    }

    let is_installation_admin: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM installation_admins WHERE user_id = ?)")
            .bind(user_id)
            .fetch_one(&mut **transaction)
            .await?;
    if is_installation_admin {
        let other_active_admins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM installation_admins \
             JOIN users ON users.id = installation_admins.user_id \
             WHERE users.state = 'active' AND installation_admins.user_id != ?",
        )
        .bind(user_id)
        .fetch_one(&mut **transaction)
        .await?;
        if other_active_admins == 0 {
            return Err(AccountAdministrationError::LastInstallationAdmin);
        }
    }

    let managed_projects: Vec<String> = sqlx::query_scalar(
        "SELECT project_id FROM project_members \
         WHERE user_id = ? AND role = 'manager' ORDER BY project_id",
    )
    .bind(user_id)
    .fetch_all(&mut **transaction)
    .await?;
    for project_id in managed_projects {
        let has_other_active_manager: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM project_members \
                 JOIN users ON users.id = project_members.user_id \
                 WHERE project_members.project_id = ? \
                   AND project_members.role = 'manager' \
                   AND project_members.user_id != ? \
                   AND users.state = 'active' \
             )",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_one(&mut **transaction)
        .await?;
        if !has_other_active_manager {
            return Err(AccountAdministrationError::LastProjectManager);
        }
    }

    Ok(())
}
