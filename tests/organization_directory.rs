use puxiri::{
    access::OrganizationRole,
    db::Database,
    organizations::{OrganizationDirectoryError, OrganizationDirectoryStore},
};
use tempfile::TempDir;
use uuid::Uuid;

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Directory test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, name: &str, role: Option<&str>) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', 'now', 'now')",
    )
    .bind(&user_id)
    .bind(name)
    .execute(database.pool())
    .await
    .unwrap();
    if let Some(role) = role {
        sqlx::query(
            "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
             VALUES (1, ?, ?, 'now')",
        )
        .bind(&user_id)
        .bind(role)
        .execute(database.pool())
        .await
        .unwrap();
    }
    user_id
}

async fn add_primary_email(database: &Database, user_id: &str, address: &str) {
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 1, '2026-10-13T12:00:00Z', '2026-10-13T12:00:00Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(address)
    .bind(address.to_ascii_lowercase())
    .execute(database.pool())
    .await
    .unwrap();
}

async fn add_pending_email(database: &Database, user_id: &str, address: &str) {
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 0, NULL, '2026-10-13T12:00:00Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(address)
    .bind(address.to_ascii_lowercase())
    .execute(database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn organization_members_see_the_directory_but_only_owner_and_admin_see_confirmed_emails() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner")).await;
    let admin = create_user(&database, "Admin", Some("admin")).await;
    let member = create_user(&database, "Member", Some("member")).await;
    let guest = create_user(&database, "Guest", Some("guest")).await;
    let unaffiliated = create_user(&database, "Unaffiliated", None).await;
    let store = OrganizationDirectoryStore::new(database.pool().clone());
    add_primary_email(&database, &owner, "owner@example.test").await;
    add_primary_email(&database, &admin, "admin@example.test").await;
    add_primary_email(&database, &member, "member@example.test").await;
    add_pending_email(&database, &guest, "guest-pending@example.test").await;

    for reader in [&owner, &admin] {
        let entries = store.list(reader).await.unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.primary_email.is_some())
                .count(),
            3
        );
        let guest_entry = entries.iter().find(|entry| entry.user_id == guest).unwrap();
        assert_eq!(guest_entry.role, OrganizationRole::Guest);
        assert_eq!(guest_entry.primary_email, None);
    }

    let member_entries = store.list(&member).await.unwrap();
    assert_eq!(member_entries.len(), 4);
    assert!(
        member_entries
            .iter()
            .all(|entry| entry.primary_email.is_none())
    );
    assert!(member_entries.iter().any(|entry| {
        entry.user_id == guest
            && entry.display_name == "Guest"
            && entry.role == OrganizationRole::Guest
    }));

    sqlx::query("UPDATE organization_members SET role = 'guest' WHERE user_id = ?")
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.list(&member).await,
        Err(OrganizationDirectoryError::Forbidden)
    ));

    assert!(matches!(
        store.list(&guest).await,
        Err(OrganizationDirectoryError::Forbidden)
    ));
    assert!(matches!(
        store.list(&unaffiliated).await,
        Err(OrganizationDirectoryError::Forbidden)
    ));
}

#[tokio::test]
async fn installation_admin_status_does_not_expand_email_visibility() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let installation_admin_member = create_user(&database, "Member", Some("member")).await;
    let guest = create_user(&database, "Guest", Some("guest")).await;
    let inactive_member = create_user(&database, "Inactive", Some("member")).await;
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&inactive_member)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&installation_admin_member)
        .execute(database.pool())
        .await
        .unwrap();
    add_primary_email(
        &database,
        &installation_admin_member,
        "install-admin@example.test",
    )
    .await;
    add_primary_email(&database, &guest, "guest@example.test").await;

    let store = OrganizationDirectoryStore::new(database.pool().clone());
    let entries = store.list(&installation_admin_member).await.unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|entry| entry.primary_email.is_none()));
    assert!(matches!(
        store.list(&guest).await,
        Err(OrganizationDirectoryError::Forbidden)
    ));
    assert!(matches!(
        store.list(&inactive_member).await,
        Err(OrganizationDirectoryError::Forbidden)
    ));
}
