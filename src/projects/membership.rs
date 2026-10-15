use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, Transaction};
use thiserror::Error;

use crate::{
    access::{
        AccessCapability, AccessContext, AccessStore, AccessStoreError, AccountState,
        OrganizationRole, ProjectRole, authorize,
    },
    audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget},
    auth::time::format_timestamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectMembershipChange {
    Added,
    RoleChanged,
    Unchanged,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMembershipRecord {
    pub user_id: String,
    pub display_name: String,
    pub role: ProjectRole,
    pub primary_email: Option<String>,
}

#[derive(Debug, Error)]
pub enum ProjectMembershipError {
    #[error("actor cannot access this project's membership")]
    Forbidden,
    #[error("target is not eligible for this membership change")]
    TargetUnavailable,
    #[error("project membership does not exist")]
    MembershipNotFound,
    #[error("only organization owners or admins can change project-manager membership")]
    ManagerMembershipChangeForbidden,
    #[error("organization guests cannot be project managers")]
    GuestCannotBeManager,
    #[error("the last project manager must be replaced before removal or demotion")]
    LastManager,
    #[error("the project is read-only until an active manager is assigned")]
    ActiveManagerRequired,
    #[error("persisted project membership role is invalid")]
    CorruptMembership,
    #[error("authorization context could not be loaded: {0}")]
    Access(#[from] AccessStoreError),
    #[error("project membership persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("project membership audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

/// Changes explicit membership for a user who already belongs to the
/// organization. Invite and organization-membership creation stay separate.
#[derive(Clone)]
pub struct ProjectMembershipStore {
    pool: SqlitePool,
}

impl ProjectMembershipStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Lists project participants. Explicit project members see display names
    /// and roles; project managers and organization owners/admins also see
    /// confirmed primary email addresses for management. The actor ID must
    /// come from an already validated request session.
    pub async fn list(
        &self,
        actor_user_id: &str,
        project_id: &str,
    ) -> Result<Vec<ProjectMembershipRecord>, ProjectMembershipError> {
        let mut transaction = self.pool.begin().await?;
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            false,
        )
        .await?
        .ok_or(ProjectMembershipError::Forbidden)?;
        if authorize(&context, AccessCapability::ReadProjectMembership).is_err() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::Forbidden);
        }

        let can_read_email = context.project_role() == Some(ProjectRole::Manager)
            || matches!(
                context.organization_role(),
                Some(OrganizationRole::Owner | OrganizationRole::Admin)
            );
        let rows = sqlx::query(
            "SELECT project_members.user_id, users.display_name, project_members.role, \
                    CASE WHEN ? = 1 THEN ( \
                        SELECT user_emails.address FROM user_emails \
                        WHERE user_emails.user_id = project_members.user_id \
                          AND user_emails.is_primary = 1 \
                          AND user_emails.confirmed_at IS NOT NULL \
                        LIMIT 1 \
                    ) ELSE NULL END AS primary_email \
             FROM project_members \
             JOIN users ON users.id = project_members.user_id \
             WHERE project_members.project_id = ? \
             ORDER BY users.display_name COLLATE NOCASE, project_members.user_id",
        )
        .bind(if can_read_email { 1_i64 } else { 0_i64 })
        .bind(project_id)
        .fetch_all(&mut *transaction)
        .await?;

        let mut members = Vec::with_capacity(rows.len());
        for row in rows {
            let role = row.try_get::<String, _>("role")?;
            members.push(ProjectMembershipRecord {
                user_id: row.try_get("user_id")?,
                display_name: row.try_get("display_name")?,
                role: parse_project_role(&role)?,
                primary_email: row.try_get("primary_email")?,
            });
        }

        transaction.commit().await?;
        Ok(members)
    }

    /// Adds an active organization member or changes their project role.
    /// The actor ID must come from an already validated request session.
    pub async fn set_role(
        &self,
        actor_user_id: &str,
        project_id: &str,
        target_user_id: &str,
        role: ProjectRole,
        now: DateTime<Utc>,
    ) -> Result<ProjectMembershipChange, ProjectMembershipError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor_context = load_actor_context(&mut transaction, actor_user_id, project_id).await?;

        if authorize(&actor_context, AccessCapability::ManageProjectMembership).is_err() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::Forbidden);
        }

        let target_context = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            target_user_id,
            false,
        )
        .await?;
        let Some(target_context) = target_context else {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::TargetUnavailable);
        };
        if target_context.organization_role().is_none() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::TargetUnavailable);
        }

        let current_role = sqlx::query_scalar::<_, String>(
            "SELECT role FROM project_members WHERE project_id = ? AND user_id = ?",
        )
        .bind(project_id)
        .bind(target_user_id)
        .fetch_optional(&mut *transaction)
        .await?
        .map(|value| parse_project_role(&value))
        .transpose()?;

        if role == ProjectRole::Manager
            && target_context.organization_role() == Some(OrganizationRole::Guest)
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::GuestCannotBeManager);
        }

        if current_role == Some(role) {
            transaction.rollback().await?;
            return Ok(ProjectMembershipChange::Unchanged);
        }

        if !actor_context.project_has_active_manager() && role != ProjectRole::Manager {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::ActiveManagerRequired);
        }

        if (current_role == Some(ProjectRole::Manager) || role == ProjectRole::Manager)
            && !can_assign_project_manager(&actor_context)
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::ManagerMembershipChangeForbidden);
        }

        if target_context.account_state() != AccountState::Active
            && current_role
                .is_none_or(|current| project_role_rank(role) > project_role_rank(current))
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::TargetUnavailable);
        }

        if current_role == Some(ProjectRole::Manager)
            && role != ProjectRole::Manager
            && project_manager_count(&mut transaction, project_id).await? <= 1
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::LastManager);
        }

        let timestamp = format_timestamp(now);
        let (change, event_kind, role_before, role_after) = match current_role {
            None => {
                sqlx::query(
                    "INSERT INTO project_members \
                     (project_id, organization_id, user_id, role, created_at) \
                     VALUES (?, 1, ?, ?, ?)",
                )
                .bind(project_id)
                .bind(target_user_id)
                .bind(project_role_as_str(role))
                .bind(&timestamp)
                .execute(&mut *transaction)
                .await?;
                (
                    ProjectMembershipChange::Added,
                    AuditEventKind::ProjectMemberAdded,
                    None,
                    Some(role),
                )
            }
            Some(previous_role) => {
                sqlx::query(
                    "UPDATE project_members SET role = ? \
                     WHERE project_id = ? AND user_id = ?",
                )
                .bind(project_role_as_str(role))
                .bind(project_id)
                .bind(target_user_id)
                .execute(&mut *transaction)
                .await?;
                (
                    ProjectMembershipChange::RoleChanged,
                    AuditEventKind::ProjectMemberRoleChanged,
                    Some(previous_role),
                    Some(role),
                )
            }
        };

        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            event_kind,
            AuditEventResult::Success,
            &AuditEventTarget::project_membership(
                project_id,
                target_user_id,
                role_before,
                role_after,
            ),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(change)
    }

    /// Removes an explicit project membership while preserving at least one
    /// manager. The actor ID must come from an already validated request.
    pub async fn remove(
        &self,
        actor_user_id: &str,
        project_id: &str,
        target_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ProjectMembershipChange, ProjectMembershipError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor_context = load_actor_context(&mut transaction, actor_user_id, project_id).await?;

        if authorize(&actor_context, AccessCapability::ManageProjectMembership).is_err() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::Forbidden);
        }

        let current_role = sqlx::query_scalar::<_, String>(
            "SELECT role FROM project_members WHERE project_id = ? AND user_id = ?",
        )
        .bind(project_id)
        .bind(target_user_id)
        .fetch_optional(&mut *transaction)
        .await?
        .map(|value| parse_project_role(&value))
        .transpose()?;
        let Some(current_role) = current_role else {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::MembershipNotFound);
        };

        if !actor_context.project_has_active_manager() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::ActiveManagerRequired);
        }

        if current_role == ProjectRole::Manager && !can_assign_project_manager(&actor_context) {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::ManagerMembershipChangeForbidden);
        }

        if current_role == ProjectRole::Manager
            && project_manager_count(&mut transaction, project_id).await? <= 1
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::LastManager);
        }

        sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(target_user_id)
            .execute(&mut *transaction)
            .await?;

        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::ProjectMemberRemoved,
            AuditEventResult::Success,
            &AuditEventTarget::project_membership(
                project_id,
                target_user_id,
                Some(current_role),
                None,
            ),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(ProjectMembershipChange::Removed)
    }

    /// Allows an active project member to remove their own membership without
    /// leaving the organization. The last project manager must be replaced
    /// first; archived projects are read-only, including voluntary exit.
    pub async fn leave(
        &self,
        user_id: &str,
        project_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ProjectMembershipChange, ProjectMembershipError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = load_actor_context(&mut transaction, user_id, project_id).await?;
        if authorize(&context, AccessCapability::LeaveProject).is_err() {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::Forbidden);
        }

        let current_role = sqlx::query_scalar::<_, String>(
            "SELECT role FROM project_members WHERE project_id = ? AND user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&mut *transaction)
        .await?
        .map(|value| parse_project_role(&value))
        .transpose()?
        .ok_or(ProjectMembershipError::Forbidden)?;

        if current_role == ProjectRole::Manager
            && project_manager_count(&mut transaction, project_id).await? <= 1
        {
            transaction.rollback().await?;
            return Err(ProjectMembershipError::LastManager);
        }

        sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;

        audit::append_in_transaction(
            &mut transaction,
            Some(user_id),
            AuditEventKind::ProjectMemberRemoved,
            AuditEventResult::Success,
            &AuditEventTarget::project_membership(project_id, user_id, Some(current_role), None),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(ProjectMembershipChange::Removed)
    }
}

async fn load_actor_context(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    project_id: &str,
) -> Result<AccessContext, ProjectMembershipError> {
    let context = AccessStore::load_project_context_in_transaction(
        transaction,
        actor_user_id,
        project_id,
        false,
    )
    .await?;
    context.ok_or(ProjectMembershipError::Forbidden)
}

fn can_assign_project_manager(context: &AccessContext) -> bool {
    matches!(
        context.organization_role(),
        Some(OrganizationRole::Owner | OrganizationRole::Admin)
    )
}

async fn project_manager_count(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_members \
         WHERE project_id = ? AND role = 'manager'",
    )
    .bind(project_id)
    .fetch_one(&mut **transaction)
    .await
}

fn project_role_as_str(role: ProjectRole) -> &'static str {
    match role {
        ProjectRole::Manager => "manager",
        ProjectRole::Member => "member",
        ProjectRole::Viewer => "viewer",
    }
}

fn project_role_rank(role: ProjectRole) -> u8 {
    match role {
        ProjectRole::Viewer => 1,
        ProjectRole::Member => 2,
        ProjectRole::Manager => 3,
    }
}

fn parse_project_role(value: &str) -> Result<ProjectRole, ProjectMembershipError> {
    match value {
        "manager" => Ok(ProjectRole::Manager),
        "member" => Ok(ProjectRole::Member),
        "viewer" => Ok(ProjectRole::Viewer),
        _ => Err(ProjectMembershipError::CorruptMembership),
    }
}
