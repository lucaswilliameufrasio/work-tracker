use puxiri::{
    access::{AccessCapability, AccessStore, AccessStoreError, OrganizationRole, authorize},
    db::Database,
    projects::ProjectStore,
};
use sqlx::SqliteConnection;
use tempfile::TempDir;
use uuid::Uuid;

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Access test', 'en', 'UTC', \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, state: &str, role: Option<&str>) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Access test user', ?, \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();

    if let Some(role) = role {
        sqlx::query(
            "INSERT INTO organization_members \
             (organization_id, user_id, role, created_at) \
             VALUES (1, ?, ?, '2026-10-12T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .bind(role)
        .execute(database.pool())
        .await
        .unwrap();
    }

    user_id
}

async fn create_project(database: &Database, owner_id: &str, project_key: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            project_key,
            "Access project",
            None,
            chrono::Utc::now(),
        )
        .await
        .unwrap()
        .id
}

async fn add_project_member(database: &Database, project_id: &str, user_id: &str, role: &str) {
    sqlx::query(
        "INSERT INTO project_members \
         (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, ?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
}

async fn set_ignore_check_constraints(connection: &mut SqliteConnection) {
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(connection)
        .await
        .unwrap();
}

#[tokio::test]
async fn current_organization_roles_are_loaded_but_never_imply_project_access() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let store = AccessStore::new(database.pool().clone());

    for (role, expected_role) in [
        ("owner", OrganizationRole::Owner),
        ("admin", OrganizationRole::Admin),
        ("member", OrganizationRole::Member),
        ("guest", OrganizationRole::Guest),
    ] {
        let user_id = create_user(&database, "active", Some(role)).await;
        let context = store
            .load_organization_context(&user_id, false)
            .await
            .unwrap()
            .unwrap();

        assert!(authorize(&context, AccessCapability::ReadProjectContent).is_err());
        assert!(authorize(&context, AccessCapability::ExportProject).is_err());
        assert_eq!(
            authorize(&context, AccessCapability::TransferOrganizationOwnership).is_ok(),
            expected_role == OrganizationRole::Owner
        );
        assert_eq!(
            authorize(&context, AccessCapability::ManageOrganizationMembership).is_ok(),
            matches!(
                expected_role,
                OrganizationRole::Owner | OrganizationRole::Admin
            )
        );
    }
}

#[tokio::test]
async fn organization_role_and_account_changes_are_visible_on_the_next_lookup() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let user_id = create_user(&database, "active", Some("member")).await;
    let store = AccessStore::new(database.pool().clone());

    let member_context = store
        .load_organization_context(&user_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &member_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_err()
    );

    sqlx::query("UPDATE organization_members SET role = 'admin' WHERE user_id = ?")
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
    let admin_context = store
        .load_organization_context(&user_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &admin_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_ok()
    );

    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
    let banned_context = store
        .load_organization_context(&user_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &banned_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_err()
    );
}

#[tokio::test]
async fn installation_admin_is_loaded_separately_and_privileged_proof_is_caller_bound() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let user_id = create_user(&database, "active", Some("member")).await;
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    let store = AccessStore::new(database.pool().clone());

    let without_proof = store
        .load_organization_context(&user_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &without_proof,
            AccessCapability::ConfigureInstallationSettings
        )
        .is_ok()
    );
    assert!(authorize(&without_proof, AccessCapability::BanInstallationUser).is_ok());
    assert!(authorize(&without_proof, AccessCapability::ReadOperationalHealth).is_ok());
    assert!(authorize(&without_proof, AccessCapability::DownloadFullBackup).is_err());
    assert!(authorize(&without_proof, AccessCapability::ReadProjectContent).is_err());

    let with_proof = store
        .load_organization_context(&user_id, true)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&with_proof, AccessCapability::DownloadFullBackup).is_ok());
}

#[tokio::test]
async fn unknown_users_return_none_and_inactive_or_unaffiliated_users_get_no_privileges() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let banned_user = create_user(&database, "banned", Some("owner")).await;
    let disabled_user = create_user(&database, "disabled", Some("admin")).await;
    let unaffiliated_user = create_user(&database, "active", None).await;
    let store = AccessStore::new(database.pool().clone());

    assert!(
        store
            .load_organization_context(&Uuid::now_v7().to_string(), false)
            .await
            .unwrap()
            .is_none()
    );

    let banned_context = store
        .load_organization_context(&banned_user, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &banned_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_err()
    );

    let disabled_context = store
        .load_organization_context(&disabled_user, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &disabled_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_err()
    );

    let unaffiliated_context = store
        .load_organization_context(&unaffiliated_user, false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        authorize(
            &unaffiliated_context,
            AccessCapability::ManageOrganizationMembership
        )
        .is_err()
    );
    assert!(authorize(&unaffiliated_context, AccessCapability::ReadProjectContent).is_err());
}

#[tokio::test]
async fn malformed_persisted_account_or_role_state_fails_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let user_id = create_user(&database, "active", Some("member")).await;
    let store = AccessStore::new(database.pool().clone());

    let mut connection = database.pool().acquire().await.unwrap();
    set_ignore_check_constraints(&mut connection).await;
    sqlx::query("UPDATE users SET state = 'unknown' WHERE id = ?")
        .bind(&user_id)
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    assert!(matches!(
        store.load_organization_context(&user_id, false).await,
        Err(AccessStoreError::CorruptState)
    ));

    let mut connection = database.pool().acquire().await.unwrap();
    set_ignore_check_constraints(&mut connection).await;
    sqlx::query("UPDATE users SET state = 'active' WHERE id = ?")
        .bind(&user_id)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("UPDATE organization_members SET role = 'unexpected' WHERE user_id = ?")
        .bind(&user_id)
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    assert!(matches!(
        store.load_organization_context(&user_id, false).await,
        Err(AccessStoreError::CorruptState)
    ));
}

#[tokio::test]
async fn invalid_installation_admin_membership_invariant_fails_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let user_id = create_user(&database, "active", Some("member")).await;
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER installation_admin_keeps_organization_membership")
        .execute(database.pool())
        .await
        .unwrap();
    let mut connection = database.pool().acquire().await.unwrap();
    set_ignore_check_constraints(&mut connection).await;
    sqlx::query("UPDATE organization_members SET role = 'guest' WHERE user_id = ?")
        .bind(&user_id)
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);

    let store = AccessStore::new(database.pool().clone());
    assert!(matches!(
        store.load_organization_context(&user_id, false).await,
        Err(AccessStoreError::CorruptState)
    ));
}

#[tokio::test]
async fn project_context_loads_explicit_roles_without_implicit_org_or_installation_access() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let creator = create_user(&database, "active", Some("owner")).await;
    let owner = create_user(&database, "active", Some("owner")).await;
    let administrator = create_user(&database, "active", Some("admin")).await;
    let member = create_user(&database, "active", Some("member")).await;
    let viewer = create_user(&database, "active", Some("member")).await;
    let guest_member = create_user(&database, "active", Some("guest")).await;
    let guest_viewer = create_user(&database, "active", Some("guest")).await;
    let installation_admin = create_user(&database, "active", Some("member")).await;
    let project_id = create_project(&database, &creator, "CTX").await;
    add_project_member(&database, &project_id, &member, "member").await;
    add_project_member(&database, &project_id, &viewer, "viewer").await;
    add_project_member(&database, &project_id, &guest_member, "member").await;
    add_project_member(&database, &project_id, &guest_viewer, "viewer").await;
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&installation_admin)
    .execute(database.pool())
    .await
    .unwrap();
    let store = AccessStore::new(database.pool().clone());

    let manager_context = store
        .load_project_context(&creator, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&manager_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&manager_context, AccessCapability::ManageProjectSchema).is_ok());
    assert!(authorize(&manager_context, AccessCapability::ExportProject).is_ok());

    let member_context = store
        .load_project_context(&member, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&member_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&member_context, AccessCapability::MutateIssues).is_ok());
    assert!(authorize(&member_context, AccessCapability::ManageProjectSchema).is_err());
    assert!(authorize(&member_context, AccessCapability::ExportProject).is_err());

    let viewer_context = store
        .load_project_context(&viewer, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&viewer_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&viewer_context, AccessCapability::MutateIssues).is_err());

    let guest_member_context = store
        .load_project_context(&guest_member, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&guest_member_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&guest_member_context, AccessCapability::MutateIssues).is_ok());
    assert!(authorize(&guest_member_context, AccessCapability::ManageProjectSchema).is_err());

    let guest_viewer_context = store
        .load_project_context(&guest_viewer, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&guest_viewer_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&guest_viewer_context, AccessCapability::MutateIssues).is_err());

    for (org_admin, role) in [
        (&owner, OrganizationRole::Owner),
        (&administrator, OrganizationRole::Admin),
    ] {
        let context = store
            .load_project_context(org_admin, &project_id, false)
            .await
            .unwrap()
            .unwrap();
        assert!(authorize(&context, AccessCapability::ReadProjectContent).is_err());
        assert!(authorize(&context, AccessCapability::ChangeProjectArchive).is_ok());
        assert!(authorize(&context, AccessCapability::ManageProjectMembership).is_ok());
        if role == OrganizationRole::Owner {
            assert!(authorize(&context, AccessCapability::TransferOrganizationOwnership).is_ok());
        }
    }

    assert!(
        store
            .load_project_context(&installation_admin, &project_id, false)
            .await
            .unwrap()
            .is_none()
    );
    let unrelated_member = create_user(&database, "active", Some("member")).await;
    assert!(
        store
            .load_project_context(&unrelated_member, &project_id, false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .load_project_context(&owner, &Uuid::now_v7().to_string(), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .load_project_context(&Uuid::now_v7().to_string(), &project_id, false)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn project_context_reloads_membership_account_state_and_archived_state() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let manager = create_user(&database, "active", Some("member")).await;
    let member = create_user(&database, "active", Some("member")).await;
    let creator = create_user(&database, "active", Some("admin")).await;
    let owner = create_user(&database, "active", Some("owner")).await;
    let project_id = create_project(&database, &creator, "UPD").await;
    add_project_member(&database, &project_id, &manager, "manager").await;
    add_project_member(&database, &project_id, &member, "member").await;
    let store = AccessStore::new(database.pool().clone());

    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&project_id)
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    let updated_member = store
        .load_project_context(&member, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&updated_member, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&updated_member, AccessCapability::MutateIssues).is_err());

    sqlx::query("UPDATE projects SET archived_at = '2026-10-12T01:00:00.000000000Z' WHERE id = ?")
        .bind(&project_id)
        .execute(database.pool())
        .await
        .unwrap();
    let archived_manager = store
        .load_project_context(&manager, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&archived_manager, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&archived_manager, AccessCapability::ExportProject).is_ok());
    assert!(authorize(&archived_manager, AccessCapability::ChangeProjectArchive).is_ok());
    assert!(authorize(&archived_manager, AccessCapability::MutateIssues).is_err());
    assert!(authorize(&archived_manager, AccessCapability::ManageProjectSchema).is_err());
    assert!(authorize(&archived_manager, AccessCapability::ManageProjectMembership).is_err());

    let archived_owner = store
        .load_project_context(&owner, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&archived_owner, AccessCapability::ReadProjectContent).is_err());
    assert!(authorize(&archived_owner, AccessCapability::ChangeProjectArchive).is_ok());

    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&manager)
        .execute(database.pool())
        .await
        .unwrap();
    let banned_manager = store
        .load_project_context(&manager, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(authorize(&banned_manager, AccessCapability::ReadProjectContent).is_err());
    assert!(authorize(&banned_manager, AccessCapability::ChangeProjectArchive).is_err());
}

#[tokio::test]
async fn malformed_persisted_project_role_fails_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", Some("owner")).await;
    let backup_manager = create_user(&database, "active", Some("member")).await;
    let project_id = create_project(&database, &owner, "BAD").await;
    add_project_member(&database, &project_id, &backup_manager, "manager").await;
    let mut connection = database.pool().acquire().await.unwrap();
    set_ignore_check_constraints(&mut connection).await;
    sqlx::query(
        "UPDATE project_members SET role = 'unknown' \
         WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project_id)
    .bind(&owner)
    .execute(&mut *connection)
    .await
    .unwrap();
    drop(connection);

    let store = AccessStore::new(database.pool().clone());
    assert!(matches!(
        store.load_project_context(&owner, &project_id, false).await,
        Err(AccessStoreError::CorruptState)
    ));
}
