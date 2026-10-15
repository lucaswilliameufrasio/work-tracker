use chrono::{DateTime, Utc};
use puxiri::{
    access::{OrganizationRole, ProjectRole},
    db::Database,
    organizations::{
        OrganizationMembershipChange, OrganizationMembershipError, OrganizationMembershipStore,
    },
    projects::{
        ProjectStore,
        membership::{ProjectMembershipChange, ProjectMembershipStore},
    },
};
use sqlx::Row;
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
         VALUES (1, 'Organization membership test', 'en', 'UTC', 'now', 'now')",
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

async fn create_project(database: &Database, owner_id: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            "ORG",
            "Organization test project",
            None,
            at("2026-10-14T12:00:00Z"),
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn organization_roles_change_with_guest_project_guards_and_typed_audit() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let member = create_user(&database, "Member", "member").await;
    let project_id = create_project(&database, &owner).await;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    assert_eq!(
        project_memberships
            .set_role(
                &owner,
                &project_id,
                &member,
                ProjectRole::Viewer,
                at("2026-10-14T12:01:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::Added
    );
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    project_memberships
        .set_role(
            &owner,
            &project_id,
            &member,
            ProjectRole::Manager,
            at("2026-10-14T12:01:30Z"),
        )
        .await
        .unwrap();
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &member,
                OrganizationRole::Guest,
                at("2026-10-14T12:01:45Z"),
            )
            .await,
        Err(OrganizationMembershipError::GuestCannotBeProjectManager)
    ));
    project_memberships
        .set_role(
            &owner,
            &project_id,
            &member,
            ProjectRole::Viewer,
            at("2026-10-14T12:01:50Z"),
        )
        .await
        .unwrap();

    assert_eq!(
        memberships
            .set_role(
                &owner,
                &member,
                OrganizationRole::Guest,
                at("2026-10-14T12:02:00Z"),
            )
            .await
            .unwrap(),
        OrganizationMembershipChange::RoleChanged
    );
    assert_eq!(
        memberships
            .set_role(
                &admin,
                &member,
                OrganizationRole::Member,
                at("2026-10-14T12:03:00Z"),
            )
            .await
            .unwrap(),
        OrganizationMembershipChange::RoleChanged
    );
    assert_eq!(
        project_memberships
            .remove(&owner, &project_id, &member, at("2026-10-14T12:03:30Z"),)
            .await
            .unwrap(),
        ProjectMembershipChange::Removed
    );
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &member,
                OrganizationRole::Guest,
                at("2026-10-14T12:04:00Z"),
            )
            .await,
        Err(OrganizationMembershipError::GuestRequiresProject)
    ));

    let member_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(member_role, "member");
    let events = sqlx::query(
        "SELECT actor_user_id, target_id, subject_user_id, \
                organization_role_before, organization_role_after \
         FROM audit_events WHERE event_kind = 'organization.member.role_changed' \
         ORDER BY occurred_at, id",
    )
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].try_get::<String, _>("actor_user_id").unwrap(),
        owner
    );
    assert_eq!(events[0].try_get::<String, _>("target_id").unwrap(), member);
    assert_eq!(
        events[0]
            .try_get::<String, _>("organization_role_before")
            .unwrap(),
        "member"
    );
    assert_eq!(
        events[0]
            .try_get::<String, _>("organization_role_after")
            .unwrap(),
        "guest"
    );
    assert_eq!(
        events[1]
            .try_get::<String, _>("organization_role_before")
            .unwrap(),
        "guest"
    );
    assert_eq!(
        events[1]
            .try_get::<String, _>("organization_role_after")
            .unwrap(),
        "member"
    );
}

#[tokio::test]
async fn owner_transfer_replaces_actor_and_preserves_independent_installation_admin_grant() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    memberships
        .transfer_ownership(&owner, &member, at("2026-10-14T12:05:00Z"))
        .await
        .unwrap();

    let old_owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let new_owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(old_owner_role, "admin");
    assert_eq!(new_owner_role, "owner");
    let old_owner_admin: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM installation_admins WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let new_owner_admin: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM installation_admins WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(old_owner_admin, 1);
    assert_eq!(new_owner_admin, 0);

    let audit_rows = sqlx::query(
        "SELECT actor_user_id, subject_user_id, organization_role_before, \
                organization_role_after FROM audit_events \
         WHERE event_kind = 'organization.member.role_changed' ORDER BY id",
    )
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_rows.len(), 2);
    let mut transitions = Vec::new();
    for row in audit_rows {
        assert_eq!(row.try_get::<String, _>("actor_user_id").unwrap(), owner);
        transitions.push((
            row.try_get::<String, _>("subject_user_id").unwrap(),
            row.try_get::<String, _>("organization_role_before")
                .unwrap(),
            row.try_get::<String, _>("organization_role_after").unwrap(),
        ));
    }
    transitions.sort();
    let mut expected = vec![
        (member, String::from("member"), String::from("owner")),
        (owner, String::from("owner"), String::from("admin")),
    ];
    expected.sort();
    assert_eq!(transitions, expected);
}

#[tokio::test]
async fn only_an_active_owner_can_transfer_to_an_active_non_guest_member() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let guest = create_user(&database, "Guest", "guest").await;
    let member = create_user(&database, "Member", "member").await;
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    assert!(matches!(
        memberships
            .remove(&owner, &owner, at("2026-10-14T12:05:30Z"))
            .await,
        Err(OrganizationMembershipError::OwnerMustTransferOwnership)
    ));
    assert!(matches!(
        memberships
            .transfer_ownership(&admin, &member, at("2026-10-14T12:06:00Z"))
            .await,
        Err(OrganizationMembershipError::Forbidden)
    ));
    assert!(matches!(
        memberships
            .transfer_ownership(&owner, &guest, at("2026-10-14T12:07:00Z"))
            .await,
        Err(OrganizationMembershipError::GuestCannotOwnOrganization)
    ));

    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&member)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        memberships
            .transfer_ownership(&owner, &member, at("2026-10-14T12:08:00Z"))
            .await,
        Err(OrganizationMembershipError::TargetUnavailable)
    ));
}

#[tokio::test]
async fn role_and_ownership_audit_failures_roll_back_all_role_changes() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    sqlx::query(
        "CREATE TRIGGER fail_organization_role_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'organization.member.role_changed' \
         BEGIN SELECT RAISE(ABORT, 'forced organization role audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &member,
                OrganizationRole::Admin,
                at("2026-10-14T12:09:00Z"),
            )
            .await,
        Err(OrganizationMembershipError::Audit(_))
    ));
    assert!(matches!(
        memberships
            .transfer_ownership(&owner, &member, at("2026-10-14T12:10:00Z"))
            .await,
        Err(OrganizationMembershipError::Audit(_))
    ));

    sqlx::query("DROP TRIGGER fail_organization_role_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let member_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(owner_role, "owner");
    assert_eq!(member_role, "member");
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'organization.member.role_changed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 0);
}

#[tokio::test]
async fn organization_removal_atomically_cleans_multiple_projects_and_audits_each_change() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let member = create_user(&database, "Member", "member").await;
    let first_project = create_project(&database, &owner).await;
    let second_project = ProjectStore::new(database.pool().clone())
        .create(
            &owner,
            "SECO",
            "Second organization test project",
            None,
            at("2026-10-14T12:20:00Z"),
        )
        .await
        .unwrap()
        .id;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    project_memberships
        .set_role(
            &admin,
            &first_project,
            &member,
            ProjectRole::Viewer,
            at("2026-10-14T12:21:00Z"),
        )
        .await
        .unwrap();
    project_memberships
        .set_role(
            &admin,
            &second_project,
            &member,
            ProjectRole::Manager,
            at("2026-10-14T12:22:00Z"),
        )
        .await
        .unwrap();

    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    assert_eq!(
        memberships
            .remove(&admin, &member, at("2026-10-14T12:23:00Z"))
            .await
            .unwrap(),
        OrganizationMembershipChange::Removed
    );

    let org_membership_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_membership_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(org_membership_count, 0);
    assert_eq!(project_membership_count, 0);

    let project_events = sqlx::query(
        "SELECT target_id, project_role_before, project_role_after \
         FROM audit_events WHERE event_kind = 'project.member.removed' \
           AND subject_user_id = ? ORDER BY target_id",
    )
    .bind(&member)
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(project_events.len(), 2);
    assert_eq!(
        project_events[0].try_get::<String, _>("target_id").unwrap(),
        first_project
    );
    assert_eq!(
        project_events[0]
            .try_get::<String, _>("project_role_before")
            .unwrap(),
        "viewer"
    );
    assert_eq!(
        project_events[1].try_get::<String, _>("target_id").unwrap(),
        second_project
    );
    assert_eq!(
        project_events[1]
            .try_get::<String, _>("project_role_before")
            .unwrap(),
        "manager"
    );
    let organization_event = sqlx::query(
        "SELECT actor_user_id, target_id, subject_user_id, \
                organization_role_before, organization_role_after \
         FROM audit_events WHERE event_kind = 'organization.member.removed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        organization_event
            .try_get::<String, _>("actor_user_id")
            .unwrap(),
        admin
    );
    assert_eq!(
        organization_event
            .try_get::<String, _>("target_id")
            .unwrap(),
        member
    );
    assert_eq!(
        organization_event
            .try_get::<String, _>("subject_user_id")
            .unwrap(),
        member
    );
    assert_eq!(
        organization_event
            .try_get::<String, _>("organization_role_before")
            .unwrap(),
        "member"
    );
    assert!(
        organization_event
            .try_get::<Option<String>, _>("organization_role_after")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn organization_member_can_leave_and_remove_all_project_memberships_with_self_audit() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    let first_project = create_project(&database, &owner).await;
    let second_project = ProjectStore::new(database.pool().clone())
        .create(
            &owner,
            "EXIT",
            "Organization exit test project",
            None,
            at("2026-10-14T12:23:00Z"),
        )
        .await
        .unwrap()
        .id;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    project_memberships
        .set_role(
            &owner,
            &first_project,
            &member,
            ProjectRole::Viewer,
            at("2026-10-14T12:23:30Z"),
        )
        .await
        .unwrap();
    project_memberships
        .set_role(
            &owner,
            &second_project,
            &member,
            ProjectRole::Member,
            at("2026-10-14T12:23:45Z"),
        )
        .await
        .unwrap();

    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    assert_eq!(
        memberships
            .leave(&member, at("2026-10-14T12:24:00Z"))
            .await
            .unwrap(),
        OrganizationMembershipChange::Left
    );

    let organization_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_count, 0);
    assert_eq!(project_count, 0);

    let events = sqlx::query(
        "SELECT event_kind, actor_user_id, target_id, subject_user_id, \
                project_role_before, organization_role_before \
         FROM audit_events WHERE subject_user_id = ? \
           AND event_kind IN ('project.member.removed', 'organization.member.removed') \
         ORDER BY event_kind, target_id",
    )
    .bind(&member)
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(events.len(), 3);
    let mut removed_projects = Vec::new();
    let mut organization_event_count = 0;
    for event in events {
        assert_eq!(event.try_get::<String, _>("actor_user_id").unwrap(), member);
        assert_eq!(
            event.try_get::<String, _>("subject_user_id").unwrap(),
            member
        );
        let event_kind: String = event.try_get("event_kind").unwrap();
        if event_kind == "project.member.removed" {
            removed_projects.push((
                event.try_get::<String, _>("target_id").unwrap(),
                event.try_get::<String, _>("project_role_before").unwrap(),
            ));
        } else {
            organization_event_count += 1;
            assert_eq!(
                event
                    .try_get::<String, _>("organization_role_before")
                    .unwrap(),
                "member"
            );
        }
    }
    removed_projects.sort();
    let mut expected_projects = vec![
        (first_project, String::from("viewer")),
        (second_project, String::from("member")),
    ];
    expected_projects.sort();
    assert_eq!(removed_projects, expected_projects);
    assert_eq!(organization_event_count, 1);
}

#[tokio::test]
async fn organization_leave_requires_owner_transfer_separate_admin_change_and_manager_replacement()
{
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    let manager = create_user(&database, "Manager", "member").await;
    let project_id = create_project(&database, &owner).await;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    project_memberships
        .set_role(
            &owner,
            &project_id,
            &manager,
            ProjectRole::Manager,
            at("2026-10-14T12:24:30Z"),
        )
        .await
        .unwrap();
    project_memberships
        .remove(&owner, &project_id, &owner, at("2026-10-14T12:24:45Z"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&installation_admin)
        .execute(database.pool())
        .await
        .unwrap();
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    assert!(matches!(
        memberships.leave(&owner, at("2026-10-14T12:25:00Z")).await,
        Err(OrganizationMembershipError::OwnerMustTransferOwnership)
    ));
    assert!(matches!(
        memberships
            .leave(&installation_admin, at("2026-10-14T12:25:15Z"))
            .await,
        Err(OrganizationMembershipError::InstallationAdminMustBeChangedFirst)
    ));
    assert!(matches!(
        memberships
            .leave(&manager, at("2026-10-14T12:25:30Z"))
            .await,
        Err(OrganizationMembershipError::LastProjectManager)
    ));

    let member_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id IN (?, ?, ?)")
            .bind(&owner)
            .bind(&installation_admin)
            .bind(&manager)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(member_count, 3);
    let project_membership_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_members WHERE project_id = ?")
            .bind(project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(project_membership_count, 1);
    let removal_audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'organization.member.removed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(removal_audit_count, 0);
}

#[tokio::test]
async fn organization_leave_audit_failure_rolls_back_project_and_org_deletions() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    let project_id = create_project(&database, &owner).await;
    ProjectMembershipStore::new(database.pool().clone())
        .set_role(
            &owner,
            &project_id,
            &member,
            ProjectRole::Viewer,
            at("2026-10-14T12:26:00Z"),
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_organization_leave_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'organization.member.removed' \
         BEGIN SELECT RAISE(ABORT, 'forced organization leave audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    assert!(matches!(
        memberships.leave(&member, at("2026-10-14T12:26:30Z")).await,
        Err(OrganizationMembershipError::Audit(_))
    ));

    let organization_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "member");
    assert_eq!(project_role, "viewer");
    let removal_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE subject_user_id = ? \
           AND event_kind IN ('project.member.removed', 'organization.member.removed')",
    )
    .bind(&member)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(removal_event_count, 0);
}

#[tokio::test]
async fn concurrent_organization_manager_exits_leave_one_manager_and_one_membership() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let first_manager = create_user(&database, "First manager", "member").await;
    let second_manager = create_user(&database, "Second manager", "member").await;
    let project_id = create_project(&database, &owner).await;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    project_memberships
        .set_role(
            &owner,
            &project_id,
            &first_manager,
            ProjectRole::Manager,
            at("2026-10-14T12:27:00Z"),
        )
        .await
        .unwrap();
    project_memberships
        .set_role(
            &owner,
            &project_id,
            &second_manager,
            ProjectRole::Manager,
            at("2026-10-14T12:27:15Z"),
        )
        .await
        .unwrap();
    project_memberships
        .remove(&owner, &project_id, &owner, at("2026-10-14T12:27:30Z"))
        .await
        .unwrap();

    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    let first = memberships.leave(&first_manager, at("2026-10-14T12:28:00Z"));
    let second = memberships.leave(&second_manager, at("2026-10-14T12:28:15Z"));
    let (first, second) = tokio::join!(first, second);
    match (first, second) {
        (
            Ok(OrganizationMembershipChange::Left),
            Err(OrganizationMembershipError::LastProjectManager),
        )
        | (
            Err(OrganizationMembershipError::LastProjectManager),
            Ok(OrganizationMembershipChange::Left),
        ) => {}
        results => panic!("unexpected concurrent organization exits: {results:?}"),
    }
    let project_manager_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_members WHERE project_id = ? AND role = 'manager'",
    )
    .bind(&project_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let remaining_membership_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id IN (?, ?)")
            .bind(&first_manager)
            .bind(&second_manager)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(project_manager_count, 1);
    assert_eq!(remaining_membership_count, 1);
}

#[tokio::test]
async fn organization_removal_refuses_to_leave_a_project_without_a_manager() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let member = create_user(&database, "Member", "member").await;
    let project_id = create_project(&database, &owner).await;
    let project_memberships = ProjectMembershipStore::new(database.pool().clone());
    project_memberships
        .set_role(
            &owner,
            &project_id,
            &member,
            ProjectRole::Manager,
            at("2026-10-14T12:23:30Z"),
        )
        .await
        .unwrap();
    project_memberships
        .remove(&owner, &project_id, &owner, at("2026-10-14T12:23:45Z"))
        .await
        .unwrap();
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    assert!(matches!(
        memberships
            .remove(&admin, &member, at("2026-10-14T12:24:00Z"))
            .await,
        Err(OrganizationMembershipError::LastProjectManager)
    ));

    let organization_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "member");
    assert_eq!(project_role, "manager");
    let owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(owner_role, "owner");
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE subject_user_id = ? \
           AND event_kind IN ('project.member.removed', 'organization.member.removed')",
    )
    .bind(&member)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 0);
}

#[tokio::test]
async fn organization_removal_audit_failure_rolls_back_project_and_org_deletions() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let admin = create_user(&database, "Admin", "admin").await;
    let member = create_user(&database, "Member", "member").await;
    let project_id = create_project(&database, &owner).await;
    ProjectMembershipStore::new(database.pool().clone())
        .set_role(
            &admin,
            &project_id,
            &member,
            ProjectRole::Viewer,
            at("2026-10-14T12:25:00Z"),
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_organization_removal_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'organization.member.removed' \
         BEGIN SELECT RAISE(ABORT, 'forced organization removal audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let memberships = OrganizationMembershipStore::new(database.pool().clone());
    assert!(matches!(
        memberships
            .remove(&admin, &member, at("2026-10-14T12:26:00Z"))
            .await,
        Err(OrganizationMembershipError::Audit(_))
    ));

    let organization_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "member");
    assert_eq!(project_role, "viewer");
    let removal_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE event_kind IN ('project.member.removed', 'organization.member.removed')",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(removal_event_count, 0);
}

#[tokio::test]
async fn ordinary_role_changes_cannot_assign_owner_or_escalate_inactive_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let member = create_user(&database, "Member", "member").await;
    let disabled_admin = create_user(&database, "Disabled admin", "admin").await;
    let disabled_member = create_user(&database, "Disabled member", "member").await;
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled_admin)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled_member)
        .execute(database.pool())
        .await
        .unwrap();
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &member,
                OrganizationRole::Owner,
                at("2026-10-14T12:27:00Z"),
            )
            .await,
        Err(OrganizationMembershipError::OwnerAssignmentRequiresTransfer)
    ));
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &disabled_member,
                OrganizationRole::Admin,
                at("2026-10-14T12:28:00Z"),
            )
            .await,
        Err(OrganizationMembershipError::TargetUnavailable)
    ));
    assert_eq!(
        memberships
            .set_role(
                &owner,
                &disabled_admin,
                OrganizationRole::Member,
                at("2026-10-14T12:29:00Z"),
            )
            .await
            .unwrap(),
        OrganizationMembershipChange::RoleChanged
    );
}

#[tokio::test]
async fn installation_admin_grant_must_be_changed_separately_before_guest_or_removal() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    let project_id = create_project(&database, &owner).await;
    ProjectMembershipStore::new(database.pool().clone())
        .set_role(
            &owner,
            &project_id,
            &installation_admin,
            ProjectRole::Viewer,
            at("2026-10-14T12:30:00Z"),
        )
        .await
        .unwrap();
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&installation_admin)
        .execute(database.pool())
        .await
        .unwrap();
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &installation_admin,
                OrganizationRole::Guest,
                at("2026-10-14T12:31:00Z"),
            )
            .await,
        Err(OrganizationMembershipError::InstallationAdminMustBeChangedFirst)
    ));
    assert!(matches!(
        memberships
            .remove(&owner, &installation_admin, at("2026-10-14T12:32:00Z"))
            .await,
        Err(OrganizationMembershipError::InstallationAdminMustBeChangedFirst)
    ));
    let grant_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM installation_admins WHERE user_id = ?")
            .bind(&installation_admin)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(grant_count, 1);
}

#[tokio::test]
async fn only_one_concurrent_ownership_transfer_from_the_same_owner_can_commit() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let first_target = create_user(&database, "First", "member").await;
    let second_target = create_user(&database, "Second", "member").await;
    let memberships = OrganizationMembershipStore::new(database.pool().clone());

    let first = memberships.transfer_ownership(&owner, &first_target, at("2026-10-14T12:11:00Z"));
    let second = memberships.transfer_ownership(&owner, &second_target, at("2026-10-14T12:12:00Z"));
    let (first, second) = tokio::join!(first, second);
    if first.is_ok() {
        assert!(matches!(
            second,
            Err(OrganizationMembershipError::Forbidden)
        ));
    } else {
        assert!(matches!(first, Err(OrganizationMembershipError::Forbidden)));
        assert!(second.is_ok());
    }
    let owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(owner_role, "admin");
    let owner_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE role = 'owner'")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(owner_count, 1);
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'organization.member.role_changed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(event_count, 2);
}
