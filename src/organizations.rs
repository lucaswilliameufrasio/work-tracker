use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, Transaction};
use thiserror::Error;

use crate::{
    access::{
        AccessCapability, AccessContext, AccessStore, AccessStoreError, AccountState,
        OrganizationRole, ProjectRole, authorize,
    },
    audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationMembershipChange {
    RoleChanged,
    Unchanged,
    Removed,
    Left,
}

#[derive(Debug, Error)]
pub enum OrganizationMembershipError {
    #[error("actor cannot manage this organization's membership")]
    Forbidden,
    #[error("target is not an available organization member")]
    TargetUnavailable,
    #[error("organization membership does not exist")]
    MembershipNotFound,
    #[error("ownership changes require the dedicated transfer operation")]
    OwnerMustTransferOwnership,
    #[error("owner cannot be assigned through an ordinary role change")]
    OwnerAssignmentRequiresTransfer,
    #[error("the transfer target is already the organization owner")]
    TargetAlreadyOwner,
    #[error("an organization guest cannot own the organization")]
    GuestCannotOwnOrganization,
    #[error("an organization guest must have at least one project membership")]
    GuestRequiresProject,
    #[error("a project manager cannot become an organization guest")]
    GuestCannotBeProjectManager,
    #[error("revoke the separate installation-admin grant before this change")]
    InstallationAdminMustBeChangedFirst,
    #[error("replace this user's only project manager role before organization removal")]
    LastProjectManager,
    #[error("persisted organization or project membership is invalid")]
    CorruptMembership,
    #[error("organization access context could not be loaded: {0}")]
    Access(#[from] AccessStoreError),
    #[error("organization membership persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("organization membership audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

/// Changes membership for users already known to this organization. Invitations
/// and installation-admin grants remain separate workflows.
#[derive(Clone)]
pub struct OrganizationMembershipStore {
    pool: SqlitePool,
}

impl OrganizationMembershipStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Changes an existing member to admin/member/guest. Ownership transfer is
    /// intentionally available only through `transfer_ownership`.
    pub async fn set_role(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        role: OrganizationRole,
        now: DateTime<Utc>,
    ) -> Result<OrganizationMembershipChange, OrganizationMembershipError> {
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let actor_context = load_organization_actor(&mut transaction, actor_user_id).await?;
        if authorize(
            &actor_context,
            AccessCapability::ManageOrganizationMembership,
        )
        .is_err()
        {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::Forbidden);
        }

        if role == OrganizationRole::Owner {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::OwnerAssignmentRequiresTransfer);
        }

        let target_context = load_target_context(&mut transaction, target_user_id).await?;
        let Some(current_role) = target_context.organization_role() else {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::TargetUnavailable);
        };
        if current_role == OrganizationRole::Owner {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::OwnerMustTransferOwnership);
        }
        if current_role == role {
            transaction.rollback().await?;
            return Ok(OrganizationMembershipChange::Unchanged);
        }

        if organization_role_rank(role) > organization_role_rank(current_role)
            && target_context.account_state() != AccountState::Active
        {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::TargetUnavailable);
        }

        if role == OrganizationRole::Guest {
            if has_installation_admin_grant(&mut transaction, target_user_id).await? {
                transaction.rollback().await?;
                return Err(OrganizationMembershipError::InstallationAdminMustBeChangedFirst);
            }
            let project_state = project_membership_state(&mut transaction, target_user_id).await?;
            if project_state.memberships == 0 {
                transaction.rollback().await?;
                return Err(OrganizationMembershipError::GuestRequiresProject);
            }
            if project_state.has_manager {
                transaction.rollback().await?;
                return Err(OrganizationMembershipError::GuestCannotBeProjectManager);
            }
        }

        let update = sqlx::query(
            "UPDATE organization_members SET role = ? \
             WHERE organization_id = 1 AND user_id = ? AND role = ?",
        )
        .bind(organization_role_as_str(role))
        .bind(target_user_id)
        .bind(organization_role_as_str(current_role))
        .execute(&mut **transaction)
        .await?;
        if update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::CorruptMembership);
        }
        append_organization_membership_event(
            &mut transaction,
            actor_user_id,
            target_user_id,
            AuditEventKind::OrganizationMemberRoleChanged,
            Some(current_role),
            Some(role),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(OrganizationMembershipChange::RoleChanged)
    }

    /// Replaces the current owner with an active, non-guest organization
    /// member. The prior owner becomes admin; installation-admin grants do not
    /// change as a side effect.
    pub async fn transfer_ownership(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<OrganizationMembershipChange, OrganizationMembershipError> {
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let actor_context = load_organization_actor(&mut transaction, actor_user_id).await?;
        if authorize(
            &actor_context,
            AccessCapability::TransferOrganizationOwnership,
        )
        .is_err()
        {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::Forbidden);
        }

        if actor_user_id == target_user_id {
            transaction.rollback().await?;
            return Ok(OrganizationMembershipChange::Unchanged);
        }

        let target_context = load_target_context(&mut transaction, target_user_id).await?;
        let Some(target_role) = target_context.organization_role() else {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::TargetUnavailable);
        };
        if target_role == OrganizationRole::Owner {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::TargetAlreadyOwner);
        }
        if target_role == OrganizationRole::Guest {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::GuestCannotOwnOrganization);
        }
        if target_context.account_state() != AccountState::Active {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::TargetUnavailable);
        }

        let old_owner_update = sqlx::query(
            "UPDATE organization_members SET role = 'admin' \
             WHERE organization_id = 1 AND user_id = ? AND role = 'owner'",
        )
        .bind(actor_user_id)
        .execute(&mut **transaction)
        .await?;
        if old_owner_update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::CorruptMembership);
        }
        let target_update = sqlx::query(
            "UPDATE organization_members SET role = 'owner' \
             WHERE organization_id = 1 AND user_id = ? AND role = ?",
        )
        .bind(target_user_id)
        .bind(organization_role_as_str(target_role))
        .execute(&mut **transaction)
        .await?;
        if target_update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::CorruptMembership);
        }

        append_organization_membership_event(
            &mut transaction,
            actor_user_id,
            actor_user_id,
            AuditEventKind::OrganizationMemberRoleChanged,
            Some(OrganizationRole::Owner),
            Some(OrganizationRole::Admin),
            now,
        )
        .await?;
        append_organization_membership_event(
            &mut transaction,
            actor_user_id,
            target_user_id,
            AuditEventKind::OrganizationMemberRoleChanged,
            Some(target_role),
            Some(OrganizationRole::Owner),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(OrganizationMembershipChange::RoleChanged)
    }

    /// Removes a member and all their project memberships atomically. A project
    /// cannot lose its last manager, and installation-admin grants must be
    /// handled separately before organization removal.
    pub async fn remove(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<OrganizationMembershipChange, OrganizationMembershipError> {
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let actor_context = load_organization_actor(&mut transaction, actor_user_id).await?;
        if authorize(
            &actor_context,
            AccessCapability::ManageOrganizationMembership,
        )
        .is_err()
        {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::Forbidden);
        }

        let target_context = load_target_context(&mut transaction, target_user_id).await?;
        let Some(current_role) = target_context.organization_role() else {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::MembershipNotFound);
        };
        remove_membership_in_transaction(
            &mut transaction,
            actor_user_id,
            target_user_id,
            current_role,
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(OrganizationMembershipChange::Removed)
    }

    /// Allows an active member to leave the organization. Their project
    /// memberships are removed atomically; ownership, installation-admin and
    /// last-project-manager safeguards still apply.
    pub async fn leave(
        &self,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<OrganizationMembershipChange, OrganizationMembershipError> {
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let context = load_organization_actor(&mut transaction, user_id).await?;
        let Some(current_role) = context.organization_role() else {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::MembershipNotFound);
        };
        if authorize(&context, AccessCapability::LeaveOrganization).is_err() {
            transaction.rollback().await?;
            return Err(OrganizationMembershipError::Forbidden);
        }

        remove_membership_in_transaction(&mut transaction, user_id, user_id, current_role, now)
            .await?;
        transaction.commit().await?;

        Ok(OrganizationMembershipChange::Left)
    }
}

async fn remove_membership_in_transaction(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    target_user_id: &str,
    current_role: OrganizationRole,
    now: DateTime<Utc>,
) -> Result<(), OrganizationMembershipError> {
    if current_role == OrganizationRole::Owner {
        return Err(OrganizationMembershipError::OwnerMustTransferOwnership);
    }
    if has_installation_admin_grant(transaction, target_user_id).await? {
        return Err(OrganizationMembershipError::InstallationAdminMustBeChangedFirst);
    }

    let project_rows = sqlx::query(
        "SELECT project_members.project_id, project_members.role, \
                (SELECT COUNT(*) FROM project_members AS managers \
                 WHERE managers.project_id = project_members.project_id \
                   AND managers.role = 'manager') AS manager_count \
         FROM project_members \
         WHERE project_members.organization_id = 1 \
           AND project_members.user_id = ? \
         ORDER BY project_members.project_id",
    )
    .bind(target_user_id)
    .fetch_all(&mut **transaction)
    .await?;

    let mut memberships = Vec::with_capacity(project_rows.len());
    for row in project_rows {
        let project_id = row.try_get::<String, _>("project_id")?;
        let role = parse_project_role(&row.try_get::<String, _>("role")?)?;
        let manager_count = row.try_get::<i64, _>("manager_count")?;
        if role == ProjectRole::Manager && manager_count <= 1 {
            return Err(OrganizationMembershipError::LastProjectManager);
        }
        memberships.push((project_id, role));
    }

    for (project_id, project_role) in memberships {
        let delete = sqlx::query(
            "DELETE FROM project_members \
             WHERE project_id = ? AND user_id = ? AND organization_id = 1",
        )
        .bind(&project_id)
        .bind(target_user_id)
        .execute(&mut **transaction)
        .await?;
        if delete.rows_affected() != 1 {
            return Err(OrganizationMembershipError::CorruptMembership);
        }
        audit::append_in_transaction(
            transaction,
            Some(actor_user_id),
            AuditEventKind::ProjectMemberRemoved,
            AuditEventResult::Success,
            &AuditEventTarget::project_membership(
                project_id,
                target_user_id,
                Some(project_role),
                None,
            ),
            now,
        )
        .await?;
    }

    let delete = sqlx::query(
        "DELETE FROM organization_members \
         WHERE organization_id = 1 AND user_id = ? AND role = ?",
    )
    .bind(target_user_id)
    .bind(organization_role_as_str(current_role))
    .execute(&mut **transaction)
    .await?;
    if delete.rows_affected() != 1 {
        return Err(OrganizationMembershipError::CorruptMembership);
    }
    append_organization_membership_event(
        transaction,
        actor_user_id,
        target_user_id,
        AuditEventKind::OrganizationMemberRemoved,
        Some(current_role),
        None,
        now,
    )
    .await?;

    Ok(())
}

struct ProjectMembershipState {
    memberships: i64,
    has_manager: bool,
}

async fn load_organization_actor(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
) -> Result<AccessContext, OrganizationMembershipError> {
    AccessStore::load_organization_context_in_transaction(transaction, actor_user_id, false)
        .await?
        .ok_or(OrganizationMembershipError::Forbidden)
}

async fn load_target_context(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    target_user_id: &str,
) -> Result<AccessContext, OrganizationMembershipError> {
    AccessStore::load_organization_context_in_transaction(transaction, target_user_id, false)
        .await?
        .ok_or(OrganizationMembershipError::TargetUnavailable)
}

async fn has_installation_admin_grant(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<bool, sqlx::Error> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS (SELECT 1 FROM installation_admins WHERE user_id = ?)",
    )
    .bind(user_id)
    .fetch_one(&mut **transaction)
    .await?;
    Ok(exists == 1)
}

async fn project_membership_state(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<ProjectMembershipState, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS memberships, \
                COALESCE(SUM(CASE WHEN role = 'manager' THEN 1 ELSE 0 END), 0) AS managers \
         FROM project_members WHERE organization_id = 1 AND user_id = ?",
    )
    .bind(user_id)
    .fetch_one(&mut **transaction)
    .await?;
    Ok(ProjectMembershipState {
        memberships: row.try_get("memberships")?,
        has_manager: row.try_get::<i64, _>("managers")? > 0,
    })
}

async fn append_organization_membership_event(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    target_user_id: &str,
    event_kind: AuditEventKind,
    role_before: Option<OrganizationRole>,
    role_after: Option<OrganizationRole>,
    now: DateTime<Utc>,
) -> Result<(), AuditEventError> {
    audit::append_in_transaction(
        transaction,
        Some(actor_user_id),
        event_kind,
        AuditEventResult::Success,
        &AuditEventTarget::organization_membership(target_user_id, role_before, role_after),
        now,
    )
    .await?;
    Ok(())
}

fn organization_role_as_str(role: OrganizationRole) -> &'static str {
    match role {
        OrganizationRole::Owner => "owner",
        OrganizationRole::Admin => "admin",
        OrganizationRole::Member => "member",
        OrganizationRole::Guest => "guest",
    }
}

fn organization_role_rank(role: OrganizationRole) -> u8 {
    match role {
        OrganizationRole::Owner => 4,
        OrganizationRole::Admin => 3,
        OrganizationRole::Member => 2,
        OrganizationRole::Guest => 1,
    }
}

fn parse_project_role(value: &str) -> Result<ProjectRole, OrganizationMembershipError> {
    match value {
        "manager" => Ok(ProjectRole::Manager),
        "member" => Ok(ProjectRole::Member),
        "viewer" => Ok(ProjectRole::Viewer),
        _ => Err(OrganizationMembershipError::CorruptMembership),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationDirectoryEntry {
    pub user_id: String,
    pub display_name: String,
    pub role: OrganizationRole,
    pub primary_email: Option<String>,
}

#[derive(Debug, Error)]
pub enum OrganizationDirectoryError {
    #[error("actor cannot read the organization directory")]
    Forbidden,
    #[error("persisted organization role is invalid")]
    CorruptRole,
    #[error("organization access context could not be loaded: {0}")]
    Access(#[from] AccessStoreError),
    #[error("organization directory query failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct OrganizationDirectoryStore {
    pool: SqlitePool,
}

impl OrganizationDirectoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Lists organization identities to active non-guest members. Only owners
    /// and admins receive confirmed primary email addresses. The actor ID
    /// must come from an already validated request session.
    pub async fn list(
        &self,
        actor_user_id: &str,
    ) -> Result<Vec<OrganizationDirectoryEntry>, OrganizationDirectoryError> {
        let mut transaction = self.pool.begin().await?;
        let Some(context) = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Err(OrganizationDirectoryError::Forbidden);
        };
        if authorize(&context, AccessCapability::ReadOrganizationDirectory).is_err() {
            transaction.rollback().await?;
            return Err(OrganizationDirectoryError::Forbidden);
        }

        let can_read_email = matches!(
            context.organization_role(),
            Some(OrganizationRole::Owner | OrganizationRole::Admin)
        );
        let entries = list_on_transaction(&mut transaction, can_read_email).await?;
        transaction.commit().await?;
        Ok(entries)
    }
}

async fn list_on_transaction(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    can_read_email: bool,
) -> Result<Vec<OrganizationDirectoryEntry>, OrganizationDirectoryError> {
    let rows = sqlx::query(
        "SELECT organization_members.user_id, users.display_name, organization_members.role, \
                CASE WHEN ? = 1 THEN ( \
                    SELECT user_emails.address FROM user_emails \
                    WHERE user_emails.user_id = organization_members.user_id \
                      AND user_emails.is_primary = 1 \
                      AND user_emails.confirmed_at IS NOT NULL \
                    LIMIT 1 \
                ) ELSE NULL END AS primary_email \
         FROM organization_members \
         JOIN users ON users.id = organization_members.user_id \
         WHERE organization_members.organization_id = 1 \
         ORDER BY users.display_name COLLATE NOCASE, organization_members.user_id",
    )
    .bind(if can_read_email { 1_i64 } else { 0_i64 })
    .fetch_all(&mut **transaction)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let role = row.try_get::<String, _>("role")?;
        entries.push(OrganizationDirectoryEntry {
            user_id: row.try_get("user_id")?,
            display_name: row.try_get("display_name")?,
            role: parse_organization_role(&role)?,
            primary_email: row.try_get("primary_email")?,
        });
    }

    Ok(entries)
}

fn parse_organization_role(value: &str) -> Result<OrganizationRole, OrganizationDirectoryError> {
    match value {
        "owner" => Ok(OrganizationRole::Owner),
        "admin" => Ok(OrganizationRole::Admin),
        "member" => Ok(OrganizationRole::Member),
        "guest" => Ok(OrganizationRole::Guest),
        _ => Err(OrganizationDirectoryError::CorruptRole),
    }
}
