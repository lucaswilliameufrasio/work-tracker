use chrono::{DateTime, Utc};
use puxiri::{
    db::Database,
    issues::{IssueCreationError, IssueReadError, IssueStore, NewIssue},
    projects::{ProjectArchiveChange, ProjectArchiveError, ProjectStore},
};
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};
use tempfile::TempDir;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

async fn organization(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Archive tests', 'en', 'UTC', 'now', 'now')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn user(pool: &SqlitePool, role: &str, state: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Archive user', ?, 'now', 'now')",
    )
    .bind(&id)
    .bind(state)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, 'now')",
    )
    .bind(&id)
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn member(pool: &SqlitePool, project: &str, user: &str, role: &str) {
    sqlx::query(
        "INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, ?, 'now')",
    )
    .bind(project)
    .bind(user)
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
}

fn issue_input() -> NewIssue<'static> {
    NewIssue {
        title: "Preserved issue",
        description: Some("Private content never copied into archive audit"),
        assignee_user_id: None,
    }
}

#[tokio::test]
async fn archive_is_audited_idempotent_read_only_and_preserves_content_and_memberships() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let pool = database.pool();
    organization(pool).await;
    let owner = user(pool, "owner", "active").await;
    let manager = user(pool, "member", "active").await;
    let reader = user(pool, "guest", "active").await;
    let store = ProjectStore::new(pool.clone());
    let project = store
        .create(&owner, "ARC", "Archive", None, now())
        .await
        .unwrap();
    member(pool, &project.id, &manager, "manager").await;
    member(pool, &project.id, &reader, "viewer").await;
    let issues = IssueStore::new(pool.clone());
    let original = issues
        .create(&manager, &project.id, issue_input(), now())
        .await
        .unwrap();
    assert_eq!(
        store
            .set_archived(&manager, &project.id, true, now())
            .await
            .unwrap(),
        ProjectArchiveChange::Changed
    );
    let archived_timestamp: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM projects WHERE id = ?")
            .bind(&project.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        archived_timestamp.as_deref(),
        Some("2026-10-15T12:00:00.000000000Z")
    );
    assert_eq!(
        store
            .set_archived(
                &manager,
                &project.id,
                true,
                now() + chrono::TimeDelta::minutes(1)
            )
            .await
            .unwrap(),
        ProjectArchiveChange::Unchanged
    );
    let unchanged_timestamp: String =
        sqlx::query_scalar("SELECT updated_at FROM projects WHERE id = ?")
            .bind(&project.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(unchanged_timestamp, "2026-10-15T12:00:00.000000000Z");
    assert!(matches!(
        issues
            .create(&manager, &project.id, issue_input(), now())
            .await,
        Err(IssueCreationError::Forbidden)
    ));
    let detail = issues
        .get_project_issue(&reader, &project.id, original.issue_number)
        .await
        .unwrap();
    assert_eq!(detail.issue, original);
    assert_eq!(
        store
            .set_archived(&manager, &project.id, false, now())
            .await
            .unwrap(),
        ProjectArchiveChange::Changed
    );
    assert_eq!(
        store
            .set_archived(&owner, &project.id, false, now())
            .await
            .unwrap(),
        ProjectArchiveChange::Unchanged
    );
    let next = issues
        .create(&manager, &project.id, issue_input(), now())
        .await
        .unwrap();
    assert_eq!(next.issue_number, original.issue_number + 1);
    let membership_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_members WHERE project_id = ?")
            .bind(&project.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(membership_count, 3);
    let audit = sqlx::query("SELECT event_kind, actor_user_id, target_kind FROM audit_events WHERE event_kind IN ('project.archived', 'project.reopened') ORDER BY id")
        .fetch_all(pool).await.unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(
        audit[0].try_get::<String, _>("event_kind").unwrap(),
        "project.archived"
    );
    assert_eq!(
        audit[1].try_get::<String, _>("event_kind").unwrap(),
        "project.reopened"
    );
    for row in audit {
        assert_eq!(row.try_get::<String, _>("actor_user_id").unwrap(), manager);
        assert_eq!(row.try_get::<String, _>("target_kind").unwrap(), "project");
    }
}

#[tokio::test]
async fn only_current_active_managers_or_org_administrators_can_change_archive_state() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let pool = database.pool();
    organization(pool).await;
    let creator = user(pool, "owner", "active").await;
    let owner = user(pool, "owner", "active").await;
    let admin = user(pool, "admin", "active").await;
    let manager = user(pool, "member", "active").await;
    let store = ProjectStore::new(pool.clone());
    let project = store
        .create(&creator, "AUTH", "Archive authorization", None, now())
        .await
        .unwrap();
    IssueStore::new(pool.clone())
        .create(&creator, &project.id, issue_input(), now())
        .await
        .unwrap();
    member(pool, &project.id, &manager, "manager").await;
    for (role, project_role, state, installation_admin) in [
        ("member", Some("member"), "active", false),
        ("member", Some("viewer"), "active", false),
        ("guest", Some("member"), "active", false),
        ("member", None, "active", true),
        ("owner", None, "banned", false),
        ("admin", None, "disabled", false),
    ] {
        let actor = user(pool, role, state).await;
        if let Some(project_role) = project_role {
            member(pool, &project.id, &actor, project_role).await;
        }
        if installation_admin {
            sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
                .bind(&actor)
                .execute(pool)
                .await
                .unwrap();
        }
        for desired in [false, true] {
            assert!(matches!(
                store
                    .set_archived(&actor, &project.id, desired, now())
                    .await,
                Err(ProjectArchiveError::Unavailable)
            ));
        }
    }
    for actor in [&owner, &admin] {
        assert_eq!(
            store
                .set_archived(actor, &project.id, true, now())
                .await
                .unwrap(),
            ProjectArchiveChange::Changed
        );
        assert!(matches!(
            IssueStore::new(pool.clone())
                .get_project_issue(actor, &project.id, 1)
                .await,
            Err(IssueReadError::Unavailable)
        ));
        assert_eq!(
            store
                .set_archived(actor, &project.id, false, now())
                .await
                .unwrap(),
            ProjectArchiveChange::Changed
        );
    }
    sqlx::query("UPDATE project_members SET role = 'member' WHERE project_id = ? AND user_id = ?")
        .bind(&project.id)
        .bind(&manager)
        .execute(pool)
        .await
        .unwrap();
    assert!(matches!(
        store.set_archived(&manager, &project.id, true, now()).await,
        Err(ProjectArchiveError::Unavailable)
    ));
    assert!(matches!(
        store
            .set_archived(&owner, &Uuid::now_v7().to_string(), true, now())
            .await,
        Err(ProjectArchiveError::Unavailable)
    ));
    assert!(matches!(
        store
            .set_archived(&Uuid::now_v7().to_string(), &project.id, true, now())
            .await,
        Err(ProjectArchiveError::Unavailable)
    ));
}

#[tokio::test]
async fn archive_and_reopen_audit_failures_roll_back_state_and_timestamps() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let pool = database.pool();
    organization(pool).await;
    let owner = user(pool, "owner", "active").await;
    let store = ProjectStore::new(pool.clone());
    let project = store
        .create(&owner, "ROLL", "Rollback", None, now())
        .await
        .unwrap();
    for (desired, event) in [(true, "project.archived"), (false, "project.reopened")] {
        let before: (Option<String>, String) =
            sqlx::query_as("SELECT archived_at, updated_at FROM projects WHERE id = ?")
                .bind(&project.id)
                .fetch_one(pool)
                .await
                .unwrap();
        let trigger = format!(
            "CREATE TRIGGER reject_archive_audit BEFORE INSERT ON audit_events WHEN NEW.event_kind = '{event}' BEGIN SELECT RAISE(ABORT, 'forced audit failure'); END"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(trigger))
            .execute(pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .set_archived(
                    &owner,
                    &project.id,
                    desired,
                    now() + chrono::TimeDelta::minutes(2)
                )
                .await,
            Err(ProjectArchiveError::Audit(_))
        ));
        let after: (Option<String>, String) =
            sqlx::query_as("SELECT archived_at, updated_at FROM projects WHERE id = ?")
                .bind(&project.id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        sqlx::query("DROP TRIGGER reject_archive_audit")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .set_archived(&owner, &project.id, desired, now())
                .await
                .unwrap(),
            ProjectArchiveChange::Changed
        );
    }
}

#[tokio::test]
async fn concurrent_archive_requests_change_once_and_reopening_does_not_override_managerless_guard()
{
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let pool = database.pool();
    organization(pool).await;
    let creator = user(pool, "owner", "active").await;
    let admin = user(pool, "admin", "active").await;
    let store = ProjectStore::new(pool.clone());
    let project = store
        .create(&creator, "RACE", "Concurrent archive", None, now())
        .await
        .unwrap();
    for desired in [true, false] {
        let (left, right) = tokio::join!(
            store.set_archived(&admin, &project.id, desired, now()),
            store.set_archived(&admin, &project.id, desired, now())
        );
        let results = [left.unwrap(), right.unwrap()];
        assert!(results.contains(&ProjectArchiveChange::Changed));
        assert!(results.contains(&ProjectArchiveChange::Unchanged));
    }
    store
        .set_archived(&admin, &project.id, true, now())
        .await
        .unwrap();
    let banned_manager = user(pool, "member", "banned").await;
    member(pool, &project.id, &banned_manager, "manager").await;
    sqlx::query("UPDATE project_members SET role = 'member' WHERE project_id = ? AND user_id = ?")
        .bind(&project.id)
        .bind(&creator)
        .execute(pool)
        .await
        .unwrap();
    store
        .set_archived(&admin, &project.id, false, now())
        .await
        .unwrap();
    assert!(matches!(
        IssueStore::new(pool.clone())
            .create(&creator, &project.id, issue_input(), now())
            .await,
        Err(IssueCreationError::Forbidden)
    ));
}

#[tokio::test]
async fn archive_audit_migration_preserves_old_rows_and_enforces_typed_immutable_events() {
    let directory = TempDir::new().unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(directory.path().join("migration.sqlite3"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    puxiri::db::ensure_write_gate(&pool).await.unwrap();
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut paths = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.file_name().unwrap() == "0029_project_archive_audit.sql" {
            break;
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(path).unwrap()))
            .execute(&pool)
            .await
            .unwrap();
    }
    organization(&pool).await;
    let owner = user(&pool, "owner", "active").await;
    let project = ProjectStore::new(pool.clone())
        .create(&owner, "MIG", "Migration", None, now())
        .await
        .unwrap();
    let before = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, event_kind, target_id FROM audit_events ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0029_project_archive_audit.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let after = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, event_kind, target_id FROM audit_events ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    for event in ["project.archived", "project.reopened"] {
        for (actor, target_kind, result, reason) in [
            (Some(owner.as_str()), "project", "success", None),
            (None, "project", "success", None),
            (Some(owner.as_str()), "user", "success", None),
            (Some(owner.as_str()), "project", "failure", None),
            (
                Some(owner.as_str()),
                "project",
                "success",
                Some("not allowed"),
            ),
        ] {
            let insert = sqlx::query("INSERT INTO audit_events (id, event_kind, result, actor_user_id, target_kind, target_id, reason, occurred_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'now')")
                .bind(Uuid::now_v7().to_string()).bind(event).bind(result).bind(actor).bind(target_kind).bind(&project.id).bind(reason).execute(&pool).await;
            assert_eq!(
                insert.is_ok(),
                actor.is_some()
                    && target_kind == "project"
                    && result == "success"
                    && reason.is_none()
            );
        }
    }
    assert!(
        sqlx::query("UPDATE audit_events SET occurred_at = 'later'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM audit_events")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}
