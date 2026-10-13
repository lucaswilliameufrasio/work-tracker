use chrono::{DateTime, Utc};
use puxiri::{
    access::ProjectRole,
    db::Database,
    projects::{
        ProjectStore,
        membership::{ProjectMembershipChange, ProjectMembershipError, ProjectMembershipStore},
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
         VALUES (1, 'Membership test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Membership test user', 'active', 'now', 'now')",
    )
    .bind(&user_id)
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

async fn create_unaffiliated_user(database: &Database) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Unaffiliated test user', 'active', 'now', 'now')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

#[tokio::test]
async fn organization_admin_can_add_existing_member_with_project_scoped_audit() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let member = create_user(&database, "member").await;
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

    let change = memberships
        .set_role(
            &owner,
            &project.id,
            &member,
            ProjectRole::Member,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();

    assert_eq!(change, ProjectMembershipChange::Added);
    let role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(role, "member");

    let audit = sqlx::query(
        "SELECT event_kind, actor_user_id, target_kind, target_id, subject_user_id, \
                project_role_before, project_role_after, occurred_at \
         FROM audit_events WHERE event_kind = 'project.member.added'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit.try_get::<String, _>("actor_user_id").unwrap(), owner);
    assert_eq!(
        audit.try_get::<String, _>("target_kind").unwrap(),
        "project"
    );
    assert_eq!(audit.try_get::<String, _>("target_id").unwrap(), project.id);
    assert_eq!(
        audit.try_get::<String, _>("subject_user_id").unwrap(),
        member
    );
    assert_eq!(
        audit
            .try_get::<Option<String>, _>("project_role_before")
            .unwrap(),
        None
    );
    assert_eq!(
        audit.try_get::<String, _>("project_role_after").unwrap(),
        "member"
    );
    assert_eq!(
        audit.try_get::<String, _>("occurred_at").unwrap(),
        "2026-10-13T12:01:00.000000000Z"
    );
}

#[tokio::test]
async fn organization_admin_self_grant_is_recorded_with_project_and_role() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let admin = create_user(&database, "admin").await;
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

    ProjectMembershipStore::new(database.pool().clone())
        .set_role(
            &admin,
            &project.id,
            &admin,
            ProjectRole::Manager,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();

    let audit = sqlx::query(
        "SELECT actor_user_id, target_kind, target_id, subject_user_id, \
                project_role_before, project_role_after \
         FROM audit_events WHERE event_kind = 'project.member.added'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit.try_get::<String, _>("actor_user_id").unwrap(), admin);
    assert_eq!(
        audit.try_get::<String, _>("target_kind").unwrap(),
        "project"
    );
    assert_eq!(audit.try_get::<String, _>("target_id").unwrap(), project.id);
    assert_eq!(
        audit.try_get::<String, _>("subject_user_id").unwrap(),
        admin
    );
    assert_eq!(
        audit
            .try_get::<Option<String>, _>("project_role_before")
            .unwrap(),
        None
    );
    assert_eq!(
        audit.try_get::<String, _>("project_role_after").unwrap(),
        "manager"
    );
}

#[tokio::test]
async fn project_manager_can_manage_members_but_cannot_change_manager_membership() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let manager = create_user(&database, "member").await;
    let peer_manager = create_user(&database, "member").await;
    let member = create_user(&database, "member").await;
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
            &peer_manager,
            ProjectRole::Manager,
            at("2026-10-13T12:01:30Z"),
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

    assert_eq!(
        memberships
            .set_role(
                &manager,
                &project.id,
                &member,
                ProjectRole::Member,
                at("2026-10-13T12:02:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::Added
    );
    assert!(matches!(
        memberships
            .set_role(
                &manager,
                &project.id,
                &member,
                ProjectRole::Manager,
                at("2026-10-13T12:03:00Z"),
            )
            .await,
        Err(ProjectMembershipError::ManagerMembershipChangeForbidden)
    ));
    assert!(matches!(
        memberships
            .set_role(
                &manager,
                &project.id,
                &peer_manager,
                ProjectRole::Member,
                at("2026-10-13T12:03:30Z"),
            )
            .await,
        Err(ProjectMembershipError::ManagerMembershipChangeForbidden)
    ));
    assert!(matches!(
        memberships
            .remove(
                &manager,
                &project.id,
                &peer_manager,
                at("2026-10-13T12:03:45Z"),
            )
            .await,
        Err(ProjectMembershipError::ManagerMembershipChangeForbidden)
    ));
    assert_eq!(
        memberships
            .set_role(
                &manager,
                &project.id,
                &member,
                ProjectRole::Viewer,
                at("2026-10-13T12:04:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::RoleChanged
    );
    assert_eq!(
        memberships
            .remove(&manager, &project.id, &member, at("2026-10-13T12:05:00Z"),)
            .await
            .unwrap(),
        ProjectMembershipChange::Removed
    );

    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&member)
            .fetch_optional(database.pool())
            .await
            .unwrap();
    assert!(role.is_none());
    let peer_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&peer_manager)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(peer_role, "manager");
}

#[tokio::test]
async fn organization_guests_are_limited_to_member_or_viewer_roles() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let guest = create_user(&database, "guest").await;
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

    assert_eq!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &guest,
                ProjectRole::Viewer,
                at("2026-10-13T12:01:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::Added
    );
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &guest,
                ProjectRole::Manager,
                at("2026-10-13T12:02:00Z"),
            )
            .await,
        Err(ProjectMembershipError::GuestCannotBeManager)
    ));

    let role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&guest)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(role, "viewer");
}

#[tokio::test]
async fn manager_removal_and_demotion_require_a_replacement() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let second_manager = create_user(&database, "member").await;
    let replacement_manager = create_user(&database, "member").await;
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

    for change in [
        memberships
            .set_role(
                &owner,
                &project.id,
                &owner,
                ProjectRole::Member,
                at("2026-10-13T12:01:00Z"),
            )
            .await,
        memberships
            .remove(&owner, &project.id, &owner, at("2026-10-13T12:02:00Z"))
            .await,
    ] {
        assert!(matches!(change, Err(ProjectMembershipError::LastManager)));
    }

    memberships
        .set_role(
            &owner,
            &project.id,
            &second_manager,
            ProjectRole::Manager,
            at("2026-10-13T12:03:00Z"),
        )
        .await
        .unwrap();
    assert_eq!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &owner,
                ProjectRole::Member,
                at("2026-10-13T12:04:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::RoleChanged
    );
    assert!(matches!(
        memberships
            .remove(
                &owner,
                &project.id,
                &second_manager,
                at("2026-10-13T12:05:00Z"),
            )
            .await,
        Err(ProjectMembershipError::LastManager)
    ));

    memberships
        .set_role(
            &owner,
            &project.id,
            &replacement_manager,
            ProjectRole::Manager,
            at("2026-10-13T12:06:00Z"),
        )
        .await
        .unwrap();
    assert_eq!(
        memberships
            .remove(
                &owner,
                &project.id,
                &second_manager,
                at("2026-10-13T12:07:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::Removed
    );

    assert!(
        sqlx::query(
            "UPDATE project_members SET role = 'member' \
             WHERE project_id = ? AND user_id = ?",
        )
        .bind(&project.id)
        .bind(&replacement_manager)
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&replacement_manager)
            .execute(database.pool())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn role_changes_noops_and_removals_are_audited_only_when_state_changes() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let member = create_user(&database, "member").await;
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
        memberships
            .set_role(
                &owner,
                &project.id,
                &member,
                ProjectRole::Member,
                at("2026-10-13T12:02:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::Unchanged
    );
    assert_eq!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &member,
                ProjectRole::Viewer,
                at("2026-10-13T12:03:00Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::RoleChanged
    );
    assert_eq!(
        memberships
            .remove(&owner, &project.id, &member, at("2026-10-13T12:04:00Z"),)
            .await
            .unwrap(),
        ProjectMembershipChange::Removed
    );

    let events = sqlx::query(
        "SELECT event_kind, subject_user_id, project_role_before, project_role_after \
         FROM audit_events \
         WHERE target_id = ? AND target_kind = 'project' ORDER BY occurred_at, id",
    )
    .bind(&project.id)
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(events.len(), 4);
    let member_events = &events[1..];
    assert_eq!(
        member_events
            .iter()
            .map(|row| row.try_get::<String, _>("event_kind").unwrap())
            .collect::<Vec<_>>(),
        [
            "project.member.added",
            "project.member.role_changed",
            "project.member.removed"
        ]
    );
    assert_eq!(
        member_events
            .iter()
            .map(|row| (
                row.try_get::<Option<String>, _>("project_role_before")
                    .unwrap(),
                row.try_get::<Option<String>, _>("project_role_after")
                    .unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            (None, Some("member".to_owned())),
            (Some("member".to_owned()), Some("viewer".to_owned())),
            (Some("viewer".to_owned()), None),
        ]
    );
    for event in member_events {
        assert_eq!(
            event.try_get::<String, _>("subject_user_id").unwrap(),
            member
        );
    }
}

#[tokio::test]
async fn audit_failure_rolls_back_each_membership_mutation() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let member = create_user(&database, "member").await;
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

    sqlx::query(
        "CREATE TRIGGER fail_project_member_added BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'project.member.added' \
         BEGIN SELECT RAISE(ABORT, 'forced membership audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &member,
                ProjectRole::Member,
                at("2026-10-13T12:01:00Z"),
            )
            .await,
        Err(ProjectMembershipError::Audit(_))
    ));
    sqlx::query("DROP TRIGGER fail_project_member_added")
        .execute(database.pool())
        .await
        .unwrap();
    let memberships_for_target: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_members WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project.id)
    .bind(&member)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(memberships_for_target, 0);

    memberships
        .set_role(
            &owner,
            &project.id,
            &member,
            ProjectRole::Member,
            at("2026-10-13T12:02:00Z"),
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_project_member_role_change BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'project.member.role_changed' \
         BEGIN SELECT RAISE(ABORT, 'forced membership audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(matches!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &member,
                ProjectRole::Viewer,
                at("2026-10-13T12:03:00Z"),
            )
            .await,
        Err(ProjectMembershipError::Audit(_))
    ));
    sqlx::query("DROP TRIGGER fail_project_member_role_change")
        .execute(database.pool())
        .await
        .unwrap();
    let role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(role, "member");

    sqlx::query(
        "CREATE TRIGGER fail_project_member_removed BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'project.member.removed' \
         BEGIN SELECT RAISE(ABORT, 'forced membership audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(matches!(
        memberships
            .remove(&owner, &project.id, &member, at("2026-10-13T12:04:00Z"),)
            .await,
        Err(ProjectMembershipError::Audit(_))
    ));
    sqlx::query("DROP TRIGGER fail_project_member_removed")
        .execute(database.pool())
        .await
        .unwrap();
    let memberships_for_target: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_members WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project.id)
    .bind(&member)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(memberships_for_target, 1);

    let member_audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE subject_user_id = ?")
            .bind(&member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(member_audits, 1);
}

#[tokio::test]
async fn membership_changes_reject_unauthorized_inactive_and_unaffiliated_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "owner").await;
    let manager = create_user(&database, "member").await;
    let viewer = create_user(&database, "member").await;
    let target = create_user(&database, "member").await;
    let banned_target = create_user(&database, "member").await;
    let banned_existing = create_user(&database, "member").await;
    let unaffiliated = create_unaffiliated_user(&database).await;
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
            &viewer,
            ProjectRole::Viewer,
            at("2026-10-13T12:02:00Z"),
        )
        .await
        .unwrap();
    memberships
        .set_role(
            &owner,
            &project.id,
            &banned_existing,
            ProjectRole::Member,
            at("2026-10-13T12:02:30Z"),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&banned_target)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&banned_existing)
        .execute(database.pool())
        .await
        .unwrap();

    assert!(matches!(
        memberships
            .set_role(
                &viewer,
                &project.id,
                &target,
                ProjectRole::Member,
                at("2026-10-13T12:03:00Z"),
            )
            .await,
        Err(ProjectMembershipError::Forbidden)
    ));
    for inactive_target in [&banned_target, &unaffiliated] {
        assert!(matches!(
            memberships
                .set_role(
                    &owner,
                    &project.id,
                    inactive_target,
                    ProjectRole::Viewer,
                    at("2026-10-13T12:04:00Z"),
                )
                .await,
            Err(ProjectMembershipError::TargetUnavailable)
        ));
    }
    assert_eq!(
        memberships
            .set_role(
                &owner,
                &project.id,
                &banned_existing,
                ProjectRole::Viewer,
                at("2026-10-13T12:04:30Z"),
            )
            .await
            .unwrap(),
        ProjectMembershipChange::RoleChanged
    );

    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&manager)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        memberships
            .set_role(
                &manager,
                &project.id,
                &target,
                ProjectRole::Member,
                at("2026-10-13T12:05:00Z"),
            )
            .await,
        Err(ProjectMembershipError::Forbidden)
    ));

    sqlx::query("UPDATE projects SET archived_at = '2026-10-13T12:06:00Z' WHERE id = ?")
        .bind(&project.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        memberships
            .remove(&owner, &project.id, &viewer, at("2026-10-13T12:07:00Z"),)
            .await,
        Err(ProjectMembershipError::Forbidden)
    ));
}

#[tokio::test]
async fn concurrent_manager_removals_leave_one_manager_and_one_audit_event() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let first_manager = create_user(&database, "owner").await;
    let second_manager = create_user(&database, "admin").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(
            &first_manager,
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
            &first_manager,
            &project.id,
            &second_manager,
            ProjectRole::Manager,
            at("2026-10-13T12:01:00Z"),
        )
        .await
        .unwrap();

    let (remove_first, remove_second) = tokio::join!(
        memberships.remove(
            &first_manager,
            &project.id,
            &second_manager,
            at("2026-10-13T12:02:00Z"),
        ),
        memberships.remove(
            &second_manager,
            &project.id,
            &first_manager,
            at("2026-10-13T12:02:00Z"),
        ),
    );

    assert_eq!(
        [remove_first.is_ok(), remove_second.is_ok()]
            .into_iter()
            .filter(|succeeded| *succeeded)
            .count(),
        1
    );
    let remaining_managers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_members WHERE project_id = ? AND role = 'manager'",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(remaining_managers, 1);
    let removal_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'project.member.removed' \
         AND target_id = ?",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(removal_audits, 1);
}
