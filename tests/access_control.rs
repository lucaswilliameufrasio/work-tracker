use puxiri::{
    access::{AccessCapability, AccessStore, AccessStoreError, OrganizationRole, authorize},
    db::Database,
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
