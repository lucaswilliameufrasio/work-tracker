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
    ManageOrganizationMembership,
    TransferOrganizationOwnership,
    CreateProject,
    ReadProjectContent,
    MutateIssues,
    ManageProjectSchema,
    ManageProjectMembership,
    ChangeProjectArchive,
    ExportProject,
    BanInstallationUser,
    DownloadFullBackup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessContext {
    account_state: AccountState,
    installation_admin: bool,
    organization_role: Option<OrganizationRole>,
    project_role: Option<ProjectRole>,
    project_archived: bool,
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
    pub fn new(
        account_state: AccountState,
        installation_admin: bool,
        organization_role: Option<OrganizationRole>,
        project_role: Option<ProjectRole>,
        project_archived: bool,
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
            recent_privileged_proof,
        })
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

    /// Loads current account and organization roles under the caller's write
    /// transaction so an authorized mutation cannot use a stale role snapshot.
    pub(crate) async fn load_organization_context_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        recent_privileged_proof: bool,
    ) -> Result<Option<AccessContext>, AccessStoreError> {
        load_organization_context_on_connection(transaction, user_id, recent_privileged_proof).await
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

    let context = AccessContext::new(
        account_state,
        installation_admin,
        organization_role,
        None,
        false,
        recent_privileged_proof,
    )
    .map_err(|_| AccessStoreError::CorruptState)?;

    Ok(Some(context))
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
    let project_is_mutable = !context.project_archived;

    let allowed = match capability {
        AccessCapability::ReadOperationalHealth => context.installation_admin,
        AccessCapability::ConfigureInstallationSettings | AccessCapability::BanInstallationUser => {
            context.installation_admin
        }
        AccessCapability::ManageOrganizationMembership | AccessCapability::CreateProject => {
            is_org_admin
        }
        AccessCapability::TransferOrganizationOwnership => {
            context.organization_role == Some(OrganizationRole::Owner)
        }
        AccessCapability::ReadProjectContent => context.project_role.is_some(),
        AccessCapability::MutateIssues => {
            project_is_mutable
                && matches!(
                    context.project_role,
                    Some(ProjectRole::Manager | ProjectRole::Member)
                )
        }
        AccessCapability::ManageProjectSchema => project_is_mutable && is_project_manager,
        AccessCapability::ManageProjectMembership => {
            project_is_mutable && (is_org_admin || is_project_manager)
        }
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

    fn context(
        state: AccountState,
        installation_admin: bool,
        organization_role: Option<OrganizationRole>,
        project_role: Option<ProjectRole>,
        project_archived: bool,
        recent_privileged_proof: bool,
    ) -> AccessContext {
        AccessContext::new(
            state,
            installation_admin,
            organization_role,
            project_role,
            project_archived,
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
            AccessCapability::ManageOrganizationMembership,
            AccessCapability::TransferOrganizationOwnership,
            AccessCapability::CreateProject,
            AccessCapability::ReadProjectContent,
            AccessCapability::MutateIssues,
            AccessCapability::ManageProjectSchema,
            AccessCapability::ManageProjectMembership,
            AccessCapability::ChangeProjectArchive,
            AccessCapability::ExportProject,
            AccessCapability::BanInstallationUser,
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
            assert_eq!(
                is_allowed(&principal, AccessCapability::TransferOrganizationOwnership),
                role == OrganizationRole::Owner
            );
            assert!(is_allowed(&principal, AccessCapability::CreateProject));
            assert!(is_allowed(
                &principal,
                AccessCapability::ManageProjectMembership
            ));
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
        }
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
        assert!(!is_allowed(&viewer, AccessCapability::MutateIssues));
        assert!(!is_allowed(&viewer, AccessCapability::ManageProjectSchema));
        assert!(!is_allowed(
            &viewer,
            AccessCapability::ManageProjectMembership
        ));
        assert!(!is_allowed(&viewer, AccessCapability::ExportProject));
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
}
