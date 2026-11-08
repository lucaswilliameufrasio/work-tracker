use chrono::{DateTime, Utc};
use puxiri::{
    access::ProjectRole,
    db::Database,
    projects::{
        ProjectStore,
        membership::{ProjectMembershipError, ProjectMembershipStore},
    },
};
use tempfile::TempDir;
use uuid::Uuid;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Roster test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, name: &str, role: &str) -> String {
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
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, 'now')",
    )
    .bind(&user_id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
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
async fn project_members_see_names_and_roles_but_emails_are_limited_to_managers_and_org_admins() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let manager = create_user(&database, "Manager", "member").await;
    let member = create_user(&database, "Member", "member").await;
    let pending = create_user(&database, "Pending", "member").await;
    let guest = create_user(&database, "Guest", "guest").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(
            &owner,
            "TEAM",
            "Team project",
            None,
            at("2026-10-13T12:00:00Z"),
        )
        .await
        .unwrap();
    let memberships = ProjectMembershipStore::new(database.pool().clone());
    memberships
        .set_role(
            &owner,
            &project.id,
            &member,
            ProjectRole::Member,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();
    memberships
        .set_role(
            &owner,
            &project.id,
            &manager,
            ProjectRole::Manager,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();
    memberships
        .set_role(
            &owner,
            &project.id,
            &pending,
            ProjectRole::Member,
            at("2026-10-13T12:01:30Z"),
        )
        .await
        .unwrap();
    memberships
        .set_role(
            &owner,
            &project.id,
            &guest,
            ProjectRole::Viewer,
            at("2026-10-13T12:02:00Z"),
        )
        .await
        .unwrap();
    add_primary_email(&database, &owner, "owner@example.test").await;
    add_primary_email(&database, &admin, "admin@example.test").await;
    add_primary_email(&database, &manager, "manager@example.test").await;
    add_primary_email(&database, &member, "member@example.test").await;
    add_pending_email(&database, &pending, "pending@example.test").await;
    add_primary_email(&database, &guest, "guest@example.test").await;

    let manager_roster = memberships.list(&owner, &project.id).await.unwrap();
    assert_eq!(manager_roster.len(), 5);
    for (user_id, expected_name, expected_role, expected_email) in [
        (
            &manager,
            "Manager",
            ProjectRole::Manager,
            Some("manager@example.test"),
        ),
        (
            &owner,
            "Owner",
            ProjectRole::Manager,
            Some("owner@example.test"),
        ),
        (
            &member,
            "Member",
            ProjectRole::Member,
            Some("member@example.test"),
        ),
        (&pending, "Pending", ProjectRole::Member, None),
        (
            &guest,
            "Guest",
            ProjectRole::Viewer,
            Some("guest@example.test"),
        ),
    ] {
        let record = manager_roster
            .iter()
            .find(|record| record.user_id == *user_id)
            .unwrap();
        assert_eq!(record.role, expected_role);
        assert_eq!(record.display_name, expected_name);
        assert_eq!(record.primary_email.as_deref(), expected_email);
    }

    for reader in [&manager, &member, &guest] {
        let roster = memberships.list(reader, &project.id).await.unwrap();
        assert_eq!(roster.len(), 5);
        if reader == &manager {
            assert_eq!(
                roster
                    .iter()
                    .filter(|record| record.primary_email.is_some())
                    .count(),
                4
            );
        } else {
            assert!(roster.iter().all(|record| record.primary_email.is_none()));
        }
    }

    let admin_roster = memberships.list(&admin, &project.id).await.unwrap();
    assert_eq!(admin_roster.len(), 5);
    assert_eq!(
        admin_roster
            .iter()
            .find(|record| record.user_id == pending)
            .unwrap()
            .primary_email,
        None
    );
    assert_eq!(
        admin_roster
            .iter()
            .filter(|record| record.primary_email.is_some())
            .count(),
        4
    );
}

#[tokio::test]
async fn roster_access_is_current_and_remains_readable_for_archived_projects() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    let outsider = create_user(&database, "Outsider", "member").await;
    let guest_without_project = create_user(&database, "Guest", "guest").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(
            &owner,
            "TEAM",
            "Team project",
            None,
            at("2026-10-13T12:00:00Z"),
        )
        .await
        .unwrap();
    let memberships = ProjectMembershipStore::new(database.pool().clone());
    memberships
        .set_role(
            &owner,
            &project.id,
            &member,
            ProjectRole::Member,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();

    assert_eq!(
        memberships.list(&member, &project.id).await.unwrap().len(),
        2
    );
    assert!(matches!(
        memberships.list(&outsider, &project.id).await,
        Err(ProjectMembershipError::Forbidden)
    ));
    assert!(matches!(
        memberships.list(&guest_without_project, &project.id).await,
        Err(ProjectMembershipError::Forbidden)
    ));

    sqlx::query("UPDATE projects SET archived_at = '2026-10-13T12:02:00Z' WHERE id = ?")
        .bind(&project.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert_eq!(
        memberships.list(&member, &project.id).await.unwrap().len(),
        2
    );

    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        memberships.list(&member, &project.id).await,
        Err(ProjectMembershipError::Forbidden)
    ));
}
