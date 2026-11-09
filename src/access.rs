use sqlx::{Row, Sqlite, SqliteConnection, SqlitePool, Transaction};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    Active,
    Banned,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationRole {
    Owner,
    Admin,
    Member,
    Guest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRole {
    Manager,
    Member,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessCapability {
    ConfigureInstallationSettings,
    ReadOperationalHealth,
    ReadOrganizationDirectory,
    ReadProjectOperationalCatalog,
    ManageOrganizationMembership,
    LeaveOrganization,
    TransferOrganizationOwnership,
    CreateProject,
    ReadProjectContent,
    MutateIssues,
    ManageProjectSchema,
    ReadProjectMembership,
    ManageProjectMembership,
    LeaveProject,
    ChangeProjectArchive,
    ExportProject,
    BanInstallationUser,
    DeactivateOwnAccount,
    ReactivateInstallationUser,
    AnonymizeInstallationAccount,
    DownloadFullBackup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessContext {
    account_state: AccountState,
    installation_admin: bool,
    organization_role: Option<OrganizationRole>,
    project_role: Option<ProjectRole>,
    project_archived: bool,
    project_has_active_manager: bool,
    recent_privileged_proof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessContextError {
    ProjectRoleWithoutOrganizationMembership,
    GuestCannotBeProjectManager,
    InstallationAdminMustBeOrganizationMember,
    GuestCannotBeInstallationAdmin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessDenied;

#[derive(Debug, Error)]
pub enum AccessStoreError {
    #[error("access context query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("persisted account or membership state is invalid")]
    CorruptState,
}

impl AccessContext {
    /// Constructs a context without project-state evidence. Project writes
    /// therefore fail closed until an `AccessStore` load confirms an active
    /// project manager.
    pub fn new(
        account_state: AccountState,
        installation_admin: bool,
        organization_role: Option<OrganizationRole>,
        project_role: Option<ProjectRole>,
        project_archived: bool,
        recent_privileged_proof: bool,
    ) -> Result<Self, AccessContextError> {
        Self::with_project_manager_state(
            account_state,
            installation_admin,
            organization_role,
            project_role,
            project_archived,
            false,
            recent_privileged_proof,
        )
    }

    fn with_project_manager_state(
        account_state: AccountState,
        installation_admin: bool,
        organization_role: Option<OrganizationRole>,
        project_role: Option<ProjectRole>,
        project_archived: bool,
        project_has_active_manager: bool,
        recent_privileged_proof: bool,
    ) -> Result<Self, AccessContextError> {
        if project_role.is_some() && organization_role.is_none() {
            return Err(AccessContextError::ProjectRoleWithoutOrganizationMembership);
        }
        if organization_role == Some(OrganizationRole::Guest)
            && project_role == Some(ProjectRole::Manager)
        {
            return Err(AccessContextError::GuestCannotBeProjectManager);
        }
        if installation_admin && organization_role.is_none() {
            return Err(AccessContextError::InstallationAdminMustBeOrganizationMember);
        }
        if installation_admin && organization_role == Some(OrganizationRole::Guest) {
            return Err(AccessContextError::GuestCannotBeInstallationAdmin);
        }

        Ok(Self {
            account_state,
            installation_admin,
            organization_role,
            project_role,
            project_archived,
            project_has_active_manager,
            recent_privileged_proof,
        })
    }

    pub fn account_state(&self) -> AccountState {
        self.account_state
    }

    pub fn organization_role(&self) -> Option<OrganizationRole> {
        self.organization_role
    }

    pub fn project_role(&self) -> Option<ProjectRole> {
        self.project_role
    }

    pub fn project_has_active_manager(&self) -> bool {
        self.project_has_active_manager
    }
}

/// Loads current account and organization membership from SQLite for one
/// organization-scoped authorization decision. Project membership is not
/// included because this query has no project identifier.
#[derive(Clone)]
pub struct AccessStore {
    pool: SqlitePool,
}

impl AccessStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Returns `None` for an unknown account. Callers must pass the ID of a
    /// session already validated for this request and must not cache the result.
    pub async fn load_organization_context(
        &self,
        user_id: &str,
        recent_privileged_proof: bool,
    ) -> Result<Option<AccessContext>, AccessStoreError> {
        let mut connection = self.pool.acquire().await?;
        load_organization_context_on_connection(&mut connection, user_id, recent_privileged_proof)
            .await
    }

    /// Loads current project access without allowing organization or
    /// installation roles to imply project-content access.
    pub async fn load_project_context(
        &self,
        user_id: &str,
        project_id: &str,
        recent_privileged_proof: bool,
    ) -> Result<Option<AccessContext>, AccessStoreError> {
        let mut connection = self.pool.acquire().await?;
        load_project_context_on_connection(
            &mut connection,
            user_id,
            project_id,
            recent_privileged_proof,
        )
        .await
    }

    /// Loads current account and organization roles under the caller's write
    /// transaction so an authorized mutation cannot use a stale role snapshot.
    pub(crate) async fn load_organization_context_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        recent_privileged_proof: bool,
    ) -> Result<Option<AccessContext>, AccessStoreError> {
        load_organization_context_on_connection(transaction, user_id, recent_privileged_proof).await
    }

    /// Loads project authorization state on the caller's transaction so
    /// protected mutations can fence membership and archive changes.
    pub async fn load_project_context_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        project_id: &str,
        recent_privileged_proof: bool,
    ) -> Result<Option<AccessContext>, AccessStoreError> {
        load_project_context_on_connection(
            transaction,
            user_id,
            project_id,
            recent_privileged_proof,
        )
        .await
    }
}

async fn load_organization_context_on_connection(
    connection: &mut SqliteConnection,
    user_id: &str,
    recent_privileged_proof: bool,
) -> Result<Option<AccessContext>, AccessStoreError> {
    let row = sqlx::query(
        "SELECT users.state AS account_state, \
                organization_members.role AS organization_role, \
                EXISTS (SELECT 1 FROM installation_admins \
                        WHERE installation_admins.user_id = users.id) \
                    AS installation_admin \
         FROM users \
         LEFT JOIN organization_members \
           ON organization_members.organization_id = 1 \
          AND organization_members.user_id = users.id \
         WHERE users.id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let account_state = parse_account_state(&row.try_get::<String, _>("account_state")?)?;
    let raw_organization_role = row.try_get::<Option<String>, _>("organization_role")?;
    let organization_role = parse_organization_role(raw_organization_role.as_deref())?;
    let installation_admin = match row.try_get::<i64, _>("installation_admin")? {
        0 => false,
        1 => true,
        _ => return Err(AccessStoreError::CorruptState),
    };

    let context = AccessContext::with_project_manager_state(
        account_state,
        installation_admin,
        organization_role,
        None,
        false,
        false,
        recent_privileged_proof,
    )
    .map_err(|_| AccessStoreError::CorruptState)?;

    Ok(Some(context))
}

async fn load_project_context_on_connection(
    connection: &mut SqliteConnection,
    user_id: &str,
    project_id: &str,
    recent_privileged_proof: bool,
) -> Result<Option<AccessContext>, AccessStoreError> {
    let row = sqlx::query(
        "SELECT users.state AS account_state, \
                organization_members.role AS organization_role, \
                EXISTS (SELECT 1 FROM installation_admins \
                        WHERE installation_admins.user_id = users.id) \
                    AS installation_admin, \
                 project_members.role AS project_role, \
                CASE WHEN projects.archived_at IS NULL THEN 0 ELSE 1 END \
                    AS project_archived, \
                EXISTS ( \
                    SELECT 1 FROM project_members AS managers \
                    JOIN users AS manager_users ON manager_users.id = managers.user_id \
                    WHERE managers.project_id = projects.id \
                      AND managers.role = 'manager' \
                      AND manager_users.state = 'active' \
                ) AS project_has_active_manager \
         FROM projects \
         JOIN users ON users.id = ? \
         LEFT JOIN organization_members \
           ON organization_members.organization_id = projects.organization_id \
          AND organization_members.user_id = users.id \
         LEFT JOIN project_members \
           ON project_members.project_id = projects.id \
          AND project_members.organization_id = projects.organization_id \
          AND project_members.user_id = users.id \
         WHERE projects.id = ?",
    )
    .bind(user_id)
    .bind(project_id)
    .fetch_optional(&mut *connection)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let account_state = parse_account_state(&row.try_get::<String, _>("account_state")?)?;
    let raw_organization_role = row.try_get::<Option<String>, _>("organization_role")?;
    let organization_role = parse_organization_role(raw_organization_role.as_deref())?;
    let installation_admin = parse_boolean_column(&row, "installation_admin")?;
    let raw_project_role = row.try_get::<Option<String>, _>("project_role")?;
    let project_role = parse_project_role(raw_project_role.as_deref())?;
    let project_archived = parse_boolean_column(&row, "project_archived")?;
    let project_has_active_manager = parse_boolean_column(&row, "project_has_active_manager")?;

    let context = AccessContext::with_project_manager_state(
        account_state,
        installation_admin,
        organization_role,
        project_role,
        project_archived,
        project_has_active_manager,
        recent_privileged_proof,
    )
    .map_err(|_| AccessStoreError::CorruptState)?;

    let is_org_admin = matches!(
        context.organization_role,
        Some(OrganizationRole::Owner | OrganizationRole::Admin)
    );
    if context.project_role.is_none() && !is_org_admin {
        return Ok(None);
    }

    Ok(Some(context))
}

fn parse_boolean_column(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<bool, AccessStoreError> {
    match row.try_get::<i64, _>(column)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(AccessStoreError::CorruptState),
    }
}

fn parse_account_state(value: &str) -> Result<AccountState, AccessStoreError> {
    match value {
        "active" => Ok(AccountState::Active),
        "banned" => Ok(AccountState::Banned),
        "disabled" => Ok(AccountState::Disabled),
        _ => Err(AccessStoreError::CorruptState),
    }
}

fn parse_organization_role(
    value: Option<&str>,
) -> Result<Option<OrganizationRole>, AccessStoreError> {
    match value {
        None => Ok(None),
        Some("owner") => Ok(Some(OrganizationRole::Owner)),
        Some("admin") => Ok(Some(OrganizationRole::Admin)),
        Some("member") => Ok(Some(OrganizationRole::Member)),
        Some("guest") => Ok(Some(OrganizationRole::Guest)),
        Some(_) => Err(AccessStoreError::CorruptState),
    }
}

fn parse_project_role(value: Option<&str>) -> Result<Option<ProjectRole>, AccessStoreError> {
    match value {
        None => Ok(None),
        Some("manager") => Ok(Some(ProjectRole::Manager)),
        Some("member") => Ok(Some(ProjectRole::Member)),
        Some("viewer") => Ok(Some(ProjectRole::Viewer)),
        Some(_) => Err(AccessStoreError::CorruptState),
    }
}

/// Evaluates a single capability from current account, organization, and
/// project membership. Installation-admin status never grants project access.
pub fn authorize(
    context: &AccessContext,
    capability: AccessCapability,
) -> Result<(), AccessDenied> {
    if context.account_state != AccountState::Active {
        return Err(AccessDenied);
    }

    let is_org_admin = matches!(
        context.organization_role,
        Some(OrganizationRole::Owner | OrganizationRole::Admin)
    );
    let is_project_manager = context.project_role == Some(ProjectRole::Manager);
    let project_is_mutable = !context.project_archived && context.project_has_active_manager;

    let allowed = match capability {
        AccessCapability::ReadOperationalHealth => context.installation_admin,
        AccessCapability::ReadProjectOperationalCatalog => is_org_admin,
        AccessCapability::ConfigureInstallationSettings
        | AccessCapability::BanInstallationUser
        | AccessCapability::ReactivateInstallationUser => context.installation_admin,
        AccessCapability::AnonymizeInstallationAccount => {
            context.installation_admin || context.organization_role == Some(OrganizationRole::Owner)
        }
        AccessCapability::DeactivateOwnAccount => true,
        AccessCapability::ReadOrganizationDirectory => matches!(
            context.organization_role,
            Some(OrganizationRole::Owner | OrganizationRole::Admin | OrganizationRole::Member)
        ),
        AccessCapability::ManageOrganizationMembership | AccessCapability::CreateProject => {
            is_org_admin
        }
        AccessCapability::LeaveOrganization => context.organization_role.is_some(),
        AccessCapability::TransferOrganizationOwnership => {
            context.organization_role == Some(OrganizationRole::Owner)
        }
        AccessCapability::ReadProjectContent => context.project_role.is_some(),
        AccessCapability::ReadProjectMembership => context.project_role.is_some() || is_org_admin,
        AccessCapability::MutateIssues => {
            project_is_mutable
                && matches!(
                    context.project_role,
                    Some(ProjectRole::Manager | ProjectRole::Member)
                )
        }
        AccessCapability::ManageProjectSchema => project_is_mutable && is_project_manager,
        AccessCapability::ManageProjectMembership => {
            !context.project_archived && (is_org_admin || is_project_manager)
        }
        AccessCapability::LeaveProject => project_is_mutable && context.project_role.is_some(),
        AccessCapability::ChangeProjectArchive => is_org_admin || is_project_manager,
        AccessCapability::ExportProject => is_project_manager,
        AccessCapability::DownloadFullBackup => {
            context.installation_admin && context.recent_privileged_proof
        }
    };

    if allowed {
        return Ok(());
    }
    Err(AccessDenied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::Database, projects::ProjectStore};
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn context(
        state: AccountState,
        installation_admin: bool,
        organization_role: Option<OrganizationRole>,
        project_role: Option<ProjectRole>,
        project_archived: bool,
        recent_privileged_proof: bool,
    ) -> AccessContext {
        AccessContext::with_project_manager_state(
            state,
            installation_admin,
            organization_role,
            project_role,
            project_archived,
            true,
            recent_privileged_proof,
        )
        .unwrap()
    }

    fn is_allowed(context: &AccessContext, capability: AccessCapability) -> bool {
        authorize(context, capability).is_ok()
    }

    #[test]
    fn inactive_accounts_are_denied_every_capability() {
        let capabilities = [
            AccessCapability::ConfigureInstallationSettings,
            AccessCapability::ReadOperationalHealth,
            AccessCapability::ReadOrganizationDirectory,
            AccessCapability::ReadProjectOperationalCatalog,
            AccessCapability::ManageOrganizationMembership,
            AccessCapability::LeaveOrganization,
            AccessCapability::TransferOrganizationOwnership,
            AccessCapability::CreateProject,
            AccessCapability::ReadProjectContent,
            AccessCapability::ReadProjectMembership,
            AccessCapability::MutateIssues,
            AccessCapability::ManageProjectSchema,
            AccessCapability::ManageProjectMembership,
            AccessCapability::LeaveProject,
            AccessCapability::ChangeProjectArchive,
            AccessCapability::ExportProject,
            AccessCapability::BanInstallationUser,
            AccessCapability::DeactivateOwnAccount,
            AccessCapability::ReactivateInstallationUser,
            AccessCapability::AnonymizeInstallationAccount,
            AccessCapability::DownloadFullBackup,
        ];

        for state in [AccountState::Banned, AccountState::Disabled] {
            let inactive = context(
                state,
                true,
                Some(OrganizationRole::Admin),
                Some(ProjectRole::Manager),
                false,
                true,
            );
            for capability in capabilities {
                assert!(!is_allowed(&inactive, capability));
            }
        }
    }

    #[test]
    fn organization_owner_and_admin_manage_the_organization_without_project_access() {
        for role in [OrganizationRole::Owner, OrganizationRole::Admin] {
            let principal = context(AccountState::Active, false, Some(role), None, false, false);
            assert!(is_allowed(
                &principal,
                AccessCapability::ManageOrganizationMembership
            ));
            assert!(is_allowed(&principal, AccessCapability::LeaveOrganization));
            assert!(is_allowed(
                &principal,
                AccessCapability::ReadOrganizationDirectory
            ));
            assert!(is_allowed(
                &principal,
                AccessCapability::ReadProjectOperationalCatalog
            ));
            assert_eq!(
                is_allowed(&principal, AccessCapability::TransferOrganizationOwnership),
                role == OrganizationRole::Owner
            );
            assert!(is_allowed(&principal, AccessCapability::CreateProject));
            assert!(is_allowed(
                &principal,
                AccessCapability::ManageProjectMembership
            ));
            assert!(!is_allowed(&principal, AccessCapability::LeaveProject));
            assert!(is_allowed(
                &principal,
                AccessCapability::ChangeProjectArchive
            ));
            assert!(!is_allowed(
                &principal,
                AccessCapability::ReadProjectContent
            ));
            assert!(!is_allowed(&principal, AccessCapability::MutateIssues));
            assert!(!is_allowed(&principal, AccessCapability::ExportProject));
            assert!(!is_allowed(
                &principal,
                AccessCapability::ReadOperationalHealth
            ));
            assert!(!is_allowed(
                &principal,
                AccessCapability::BanInstallationUser
            ));
            assert!(!is_allowed(
                &principal,
                AccessCapability::DownloadFullBackup
            ));
            assert!(!is_allowed(
                &principal,
                AccessCapability::ConfigureInstallationSettings
            ));
            assert_eq!(
                is_allowed(&principal, AccessCapability::AnonymizeInstallationAccount),
                role == OrganizationRole::Owner
            );
        }
    }

    #[test]
    fn operational_project_catalog_does_not_follow_installation_or_project_privileges() {
        for role in [OrganizationRole::Member, OrganizationRole::Guest] {
            let principal = context(
                AccountState::Active,
                role == OrganizationRole::Member,
                Some(role),
                Some(if role == OrganizationRole::Guest {
                    ProjectRole::Member
                } else {
                    ProjectRole::Manager
                }),
                false,
                false,
            );
            assert!(!is_allowed(
                &principal,
                AccessCapability::ReadProjectOperationalCatalog
            ));
        }
    }

    #[test]
    fn active_organization_members_can_request_self_exit() {
        for role in [
            OrganizationRole::Owner,
            OrganizationRole::Admin,
            OrganizationRole::Member,
            OrganizationRole::Guest,
        ] {
            let principal = context(AccountState::Active, false, Some(role), None, false, false);
            assert!(is_allowed(&principal, AccessCapability::LeaveOrganization));
        }

        let installation_admin = context(
            AccountState::Active,
            true,
            Some(OrganizationRole::Member),
            None,
            false,
            false,
        );
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::LeaveOrganization
        ));

        let unaffiliated = context(AccountState::Active, false, None, None, false, false);
        assert!(!is_allowed(
            &unaffiliated,
            AccessCapability::LeaveOrganization
        ));
    }

    #[test]
    fn active_users_can_request_self_deactivation_but_only_installation_admins_can_reactivate() {
        let member = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            None,
            false,
            false,
        );
        assert!(is_allowed(&member, AccessCapability::DeactivateOwnAccount));
        assert!(!is_allowed(
            &member,
            AccessCapability::ReactivateInstallationUser
        ));

        let installation_admin = context(
            AccountState::Active,
            true,
            Some(OrganizationRole::Member),
            None,
            false,
            false,
        );
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::DeactivateOwnAccount
        ));
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::ReactivateInstallationUser
        ));
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::AnonymizeInstallationAccount
        ));
        assert!(!is_allowed(
            &member,
            AccessCapability::AnonymizeInstallationAccount
        ));
    }

    #[test]
    fn project_roles_grant_only_their_project_capabilities() {
        let manager = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            Some(ProjectRole::Manager),
            false,
            false,
        );
        assert!(is_allowed(&manager, AccessCapability::ReadProjectContent));
        assert!(is_allowed(&manager, AccessCapability::MutateIssues));
        assert!(is_allowed(&manager, AccessCapability::ManageProjectSchema));
        assert!(is_allowed(
            &manager,
            AccessCapability::ManageProjectMembership
        ));
        assert!(is_allowed(&manager, AccessCapability::ChangeProjectArchive));
        assert!(is_allowed(&manager, AccessCapability::ExportProject));
        assert!(is_allowed(
            &manager,
            AccessCapability::ReadProjectMembership
        ));
        assert!(is_allowed(&manager, AccessCapability::LeaveProject));
        assert!(!is_allowed(
            &manager,
            AccessCapability::ManageOrganizationMembership
        ));
        assert!(!is_allowed(&manager, AccessCapability::CreateProject));
        assert!(!is_allowed(
            &manager,
            AccessCapability::ConfigureInstallationSettings
        ));

        let member = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            Some(ProjectRole::Member),
            false,
            false,
        );
        assert!(is_allowed(&member, AccessCapability::ReadProjectContent));
        assert!(is_allowed(&member, AccessCapability::MutateIssues));
        assert!(is_allowed(&member, AccessCapability::ReadProjectMembership));
        assert!(is_allowed(&member, AccessCapability::LeaveProject));
        assert!(!is_allowed(&member, AccessCapability::ManageProjectSchema));
        assert!(!is_allowed(
            &member,
            AccessCapability::ManageProjectMembership
        ));
        assert!(!is_allowed(&member, AccessCapability::ExportProject));

        let viewer = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            Some(ProjectRole::Viewer),
            false,
            false,
        );
        assert!(is_allowed(&viewer, AccessCapability::ReadProjectContent));
        assert!(is_allowed(&viewer, AccessCapability::ReadProjectMembership));
        assert!(is_allowed(&viewer, AccessCapability::LeaveProject));
        assert!(!is_allowed(&viewer, AccessCapability::MutateIssues));
        assert!(!is_allowed(&viewer, AccessCapability::ManageProjectSchema));
        assert!(!is_allowed(
            &viewer,
            AccessCapability::ManageProjectMembership
        ));
        assert!(!is_allowed(&viewer, AccessCapability::ExportProject));
    }

    #[test]
    fn managerless_projects_are_read_only_but_organization_admins_can_repair_membership() {
        let member = AccessContext::with_project_manager_state(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            Some(ProjectRole::Member),
            false,
            false,
            false,
        )
        .unwrap();
        assert!(is_allowed(&member, AccessCapability::ReadProjectContent));
        assert!(!is_allowed(&member, AccessCapability::MutateIssues));
        assert!(!is_allowed(&member, AccessCapability::ManageProjectSchema));
        assert!(!is_allowed(&member, AccessCapability::LeaveProject));

        let administrator = AccessContext::with_project_manager_state(
            AccountState::Active,
            false,
            Some(OrganizationRole::Admin),
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(is_allowed(
            &administrator,
            AccessCapability::ManageProjectMembership
        ));
        assert!(is_allowed(
            &administrator,
            AccessCapability::ChangeProjectArchive
        ));
        assert!(!is_allowed(&administrator, AccessCapability::MutateIssues));
    }

    #[test]
    fn organization_admins_can_read_project_roster_without_project_content_access() {
        for organization_role in [OrganizationRole::Owner, OrganizationRole::Admin] {
            let context = context(
                AccountState::Active,
                false,
                Some(organization_role),
                None,
                true,
                false,
            );
            assert!(is_allowed(
                &context,
                AccessCapability::ReadProjectMembership
            ));
            assert!(!is_allowed(&context, AccessCapability::ReadProjectContent));
        }

        let organization_member = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            None,
            false,
            false,
        );
        assert!(!is_allowed(
            &organization_member,
            AccessCapability::ReadProjectMembership
        ));
    }

    #[test]
    fn organization_directory_is_available_to_non_guests_but_not_guests() {
        for organization_role in [
            OrganizationRole::Owner,
            OrganizationRole::Admin,
            OrganizationRole::Member,
        ] {
            let context = context(
                AccountState::Active,
                false,
                Some(organization_role),
                None,
                false,
                false,
            );
            assert!(is_allowed(
                &context,
                AccessCapability::ReadOrganizationDirectory
            ));
        }

        let guest = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Guest),
            None,
            false,
            false,
        );
        assert!(!is_allowed(
            &guest,
            AccessCapability::ReadOrganizationDirectory
        ));
    }

    #[test]
    fn project_members_can_leave_only_while_the_project_is_unarchived() {
        for (organization_role, project_role) in [
            (OrganizationRole::Member, ProjectRole::Manager),
            (OrganizationRole::Member, ProjectRole::Member),
            (OrganizationRole::Guest, ProjectRole::Viewer),
        ] {
            let active = context(
                AccountState::Active,
                false,
                Some(organization_role),
                Some(project_role),
                false,
                false,
            );
            assert!(is_allowed(&active, AccessCapability::LeaveProject));

            let archived = context(
                AccountState::Active,
                false,
                Some(organization_role),
                Some(project_role),
                true,
                false,
            );
            assert!(!is_allowed(&archived, AccessCapability::LeaveProject));
            assert!(is_allowed(
                &archived,
                AccessCapability::ReadProjectMembership
            ));
        }

        let organization_admin = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Admin),
            None,
            false,
            false,
        );
        assert!(!is_allowed(
            &organization_admin,
            AccessCapability::LeaveProject
        ));
    }

    #[test]
    fn guests_are_capped_at_project_member_and_viewer_capabilities() {
        let guest_member = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Guest),
            Some(ProjectRole::Member),
            false,
            false,
        );
        assert!(is_allowed(
            &guest_member,
            AccessCapability::ReadProjectContent
        ));
        assert!(is_allowed(
            &guest_member,
            AccessCapability::ReadProjectMembership
        ));
        assert!(is_allowed(&guest_member, AccessCapability::MutateIssues));
        assert!(!is_allowed(
            &guest_member,
            AccessCapability::ManageProjectSchema
        ));
        assert!(!is_allowed(
            &guest_member,
            AccessCapability::ManageProjectMembership
        ));
        assert!(!is_allowed(&guest_member, AccessCapability::CreateProject));

        let guest_viewer = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Guest),
            Some(ProjectRole::Viewer),
            false,
            false,
        );
        assert!(is_allowed(
            &guest_viewer,
            AccessCapability::ReadProjectContent
        ));
        assert!(is_allowed(
            &guest_viewer,
            AccessCapability::ReadProjectMembership
        ));
        assert!(!is_allowed(&guest_viewer, AccessCapability::MutateIssues));
    }

    #[test]
    fn installation_admin_needs_recent_proof_only_for_backup() {
        let installation_admin = context(
            AccountState::Active,
            true,
            Some(OrganizationRole::Member),
            None,
            false,
            false,
        );
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::ConfigureInstallationSettings
        ));
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::ReadOperationalHealth
        ));
        assert!(is_allowed(
            &installation_admin,
            AccessCapability::BanInstallationUser
        ));
        assert!(!is_allowed(
            &installation_admin,
            AccessCapability::DownloadFullBackup
        ));
        assert!(!is_allowed(
            &installation_admin,
            AccessCapability::ReadProjectContent
        ));
        assert!(!is_allowed(
            &installation_admin,
            AccessCapability::ReadProjectMembership
        ));

        let privileged_admin = context(
            AccountState::Active,
            true,
            Some(OrganizationRole::Member),
            None,
            false,
            true,
        );
        assert!(is_allowed(
            &privileged_admin,
            AccessCapability::DownloadFullBackup
        ));
        assert!(is_allowed(
            &privileged_admin,
            AccessCapability::ConfigureInstallationSettings
        ));
        assert!(is_allowed(
            &privileged_admin,
            AccessCapability::BanInstallationUser
        ));

        let org_admin = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Admin),
            None,
            false,
            true,
        );
        assert!(!is_allowed(
            &org_admin,
            AccessCapability::DownloadFullBackup
        ));
        assert!(!is_allowed(
            &org_admin,
            AccessCapability::ConfigureInstallationSettings
        ));
    }

    #[test]
    fn archived_projects_are_read_only_but_managers_can_export_and_restore_them() {
        let manager = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Member),
            Some(ProjectRole::Manager),
            true,
            false,
        );
        assert!(is_allowed(&manager, AccessCapability::ReadProjectContent));
        assert!(is_allowed(&manager, AccessCapability::ExportProject));
        assert!(is_allowed(&manager, AccessCapability::ChangeProjectArchive));
        assert!(!is_allowed(&manager, AccessCapability::MutateIssues));
        assert!(!is_allowed(&manager, AccessCapability::ManageProjectSchema));
        assert!(!is_allowed(
            &manager,
            AccessCapability::ManageProjectMembership
        ));

        let org_admin = context(
            AccountState::Active,
            false,
            Some(OrganizationRole::Admin),
            None,
            true,
            false,
        );
        assert!(is_allowed(
            &org_admin,
            AccessCapability::ChangeProjectArchive
        ));
        assert!(!is_allowed(
            &org_admin,
            AccessCapability::ReadProjectContent
        ));
        assert!(!is_allowed(
            &org_admin,
            AccessCapability::ManageProjectMembership
        ));
    }

    #[test]
    fn inconsistent_membership_shapes_are_rejected() {
        assert_eq!(
            AccessContext::new(
                AccountState::Active,
                false,
                None,
                Some(ProjectRole::Viewer),
                false,
                false,
            ),
            Err(AccessContextError::ProjectRoleWithoutOrganizationMembership)
        );
        assert_eq!(
            AccessContext::new(
                AccountState::Active,
                false,
                Some(OrganizationRole::Guest),
                Some(ProjectRole::Manager),
                false,
                false,
            ),
            Err(AccessContextError::GuestCannotBeProjectManager)
        );
        assert_eq!(
            AccessContext::new(AccountState::Active, true, None, None, false, false),
            Err(AccessContextError::InstallationAdminMustBeOrganizationMember)
        );
        assert_eq!(
            AccessContext::new(
                AccountState::Active,
                true,
                Some(OrganizationRole::Guest),
                None,
                false,
                false,
            ),
            Err(AccessContextError::GuestCannotBeInstallationAdmin)
        );
    }

    #[tokio::test]
    async fn project_context_can_be_loaded_from_the_callers_write_transaction() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        sqlx::query(
            "INSERT INTO organizations \
             (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'Access transaction test', 'en', 'UTC', \
                     '2026-10-12T00:00:00.000000000Z', \
                     '2026-10-12T00:00:00.000000000Z')",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let user_id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'Access transaction test', 'active', \
                     '2026-10-12T00:00:00.000000000Z', \
                     '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO organization_members \
             (organization_id, user_id, role, created_at) \
             VALUES (1, ?, 'owner', '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-12T13:14:15Z")
            .unwrap()
            .with_timezone(&Utc);
        let project = ProjectStore::new(database.pool().clone())
            .create(&user_id, "TX", "Transaction context", None, now)
            .await
            .unwrap();
        let backup_manager_id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'Backup manager', 'active', \
                     '2026-10-12T00:00:00.000000000Z', \
                     '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&backup_manager_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO organization_members \
             (organization_id, user_id, role, created_at) \
             VALUES (1, ?, 'member', '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&backup_manager_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO project_members \
             (project_id, organization_id, user_id, role, created_at) \
             VALUES (?, 1, ?, 'manager', '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&project.id)
        .bind(&backup_manager_id)
        .execute(database.pool())
        .await
        .unwrap();

        let mut transaction = crate::db::admitted_write(database.pool()).await.unwrap();
        sqlx::query(
            "UPDATE project_members SET role = 'viewer' \
             WHERE project_id = ? AND user_id = ?",
        )
        .bind(&project.id)
        .bind(&user_id)
        .execute(&mut **transaction)
        .await
        .unwrap();
        let transaction_context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            &user_id,
            &project.id,
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(is_allowed(
            &transaction_context,
            AccessCapability::ReadProjectContent
        ));
        assert!(!is_allowed(
            &transaction_context,
            AccessCapability::MutateIssues
        ));
        transaction.rollback().await.unwrap();

        let persisted_context = AccessStore::new(database.pool().clone())
            .load_project_context(&user_id, &project.id, false)
            .await
            .unwrap()
            .unwrap();
        assert!(is_allowed(
            &persisted_context,
            AccessCapability::MutateIssues
        ));
    }
}
