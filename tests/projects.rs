use chrono::{DateTime, Utc};
use puxiri::{
    db::Database,
    projects::{ProjectCreationError, ProjectDefaultView, ProjectStore},
};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
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
         VALUES (1, 'Project test', 'en', 'UTC', \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, state: &str, role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Project test user', ?, \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
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
    user_id
}

#[tokio::test]
async fn creation_atomically_persists_project_creator_manager_and_opaque_audit() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15.123456789Z");

    let project = store
        .create(
            &owner,
            "DEV",
            "  Development  ",
            Some("A project description"),
            now,
        )
        .await
        .unwrap();

    assert_eq!(project.project_key, "DEV");
    assert_eq!(project.name, "Development");
    assert_eq!(
        project.description.as_deref(),
        Some("A project description")
    );
    assert_eq!(project.default_view, ProjectDefaultView::Board);

    let row = sqlx::query(
        "SELECT organization_id, project_key, name, description, default_view, \
                archived_at, next_issue_number, created_at, updated_at \
         FROM projects WHERE id = ?",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("organization_id").unwrap(), 1);
    assert_eq!(row.try_get::<String, _>("project_key").unwrap(), "DEV");
    assert_eq!(row.try_get::<String, _>("name").unwrap(), "Development");
    assert_eq!(
        row.try_get::<Option<String>, _>("description")
            .unwrap()
            .as_deref(),
        Some("A project description")
    );
    assert_eq!(row.try_get::<String, _>("default_view").unwrap(), "board");
    assert_eq!(
        row.try_get::<Option<String>, _>("archived_at").unwrap(),
        None
    );
    assert_eq!(row.try_get::<i64, _>("next_issue_number").unwrap(), 1);
    assert_eq!(
        row.try_get::<String, _>("created_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );
    assert_eq!(
        row.try_get::<String, _>("updated_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );

    let member = sqlx::query(
        "SELECT organization_id, user_id, role, created_at \
         FROM project_members WHERE project_id = ?",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(member.try_get::<i64, _>("organization_id").unwrap(), 1);
    assert_eq!(member.try_get::<String, _>("user_id").unwrap(), owner);
    assert_eq!(member.try_get::<String, _>("role").unwrap(), "manager");

    let audit = sqlx::query(
        "SELECT event_kind, result, actor_user_id, target_kind, target_id, occurred_at \
         FROM audit_events WHERE target_id = ?",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        audit.try_get::<String, _>("event_kind").unwrap(),
        "project.created"
    );
    assert_eq!(audit.try_get::<String, _>("result").unwrap(), "success");
    assert_eq!(audit.try_get::<String, _>("actor_user_id").unwrap(), owner);
    assert_eq!(
        audit.try_get::<String, _>("target_kind").unwrap(),
        "project"
    );
    assert_eq!(audit.try_get::<String, _>("target_id").unwrap(), project.id);
    assert_eq!(
        audit.try_get::<String, _>("occurred_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );
}

#[tokio::test]
async fn only_active_organization_owners_and_admins_can_create_projects() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let store = ProjectStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    for (role, key) in [("owner", "OWN"), ("admin", "ADM")] {
        let actor = create_user(&database, "active", role).await;
        assert!(
            store
                .create(&actor, key, "Allowed", None, now)
                .await
                .is_ok()
        );
    }

    let member = create_user(&database, "active", "member").await;
    let guest = create_user(&database, "active", "guest").await;
    let banned_owner = create_user(&database, "banned", "owner").await;
    let disabled_admin = create_user(&database, "disabled", "admin").await;
    let installation_admin_member = create_user(&database, "active", "member").await;
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&installation_admin_member)
    .execute(database.pool())
    .await
    .unwrap();

    for (actor, key) in [
        (member, "MEM"),
        (guest, "GST"),
        (banned_owner, "BAN"),
        (disabled_admin, "DIS"),
        (installation_admin_member, "OPS"),
        (Uuid::now_v7().to_string(), "UNK"),
    ] {
        assert!(matches!(
            store.create(&actor, key, "Denied", None, now).await,
            Err(ProjectCreationError::Forbidden | ProjectCreationError::UnknownActor)
        ));
    }

    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(projects, 2);
    assert_eq!(audits, 2);
}

#[tokio::test]
async fn invalid_inputs_and_duplicate_keys_leave_no_partial_rows() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    for project_key in ["A", "dev", "A1", "ABCDEFG"] {
        assert!(matches!(
            store
                .create(&owner, project_key, "Valid name", None, now)
                .await,
            Err(ProjectCreationError::InvalidProjectKey)
        ));
    }
    assert!(matches!(
        store.create(&owner, "EMPTY", "  ", None, now).await,
        Err(ProjectCreationError::InvalidProjectName)
    ));

    store
        .create(&owner, "DUP", "First", None, now)
        .await
        .unwrap();
    assert!(matches!(
        store.create(&owner, "DUP", "Second", None, now).await,
        Err(ProjectCreationError::ProjectKeyUnavailable)
    ));

    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM project_members")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(projects, 1);
    assert_eq!(members, 1);
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn concurrent_creation_with_the_same_key_has_one_atomic_winner() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    let (first, second) = tokio::join!(
        store.create(&owner, "RACE", "First", None, now),
        store.create(&owner, "RACE", "Second", None, now)
    );
    let successful = [first, second].into_iter().filter(Result::is_ok).count();
    assert_eq!(successful, 1);

    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM project_members")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(projects, 1);
    assert_eq!(members, 1);
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn audit_failure_rolls_back_project_and_initial_membership() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    sqlx::query(
        "CREATE TRIGGER reject_project_creation_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'project.created' \
         BEGIN SELECT RAISE(ABORT, 'forced audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let store = ProjectStore::new(database.pool().clone());

    assert!(matches!(
        store
            .create(&owner, "ROLL", "Rollback", None, at("2026-10-12T13:14:15Z"))
            .await,
        Err(ProjectCreationError::Audit(_))
    ));

    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM project_members")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(projects, 0);
    assert_eq!(members, 0);
    assert_eq!(audits, 0);
}

#[tokio::test]
async fn database_constraints_preserve_explicit_membership_and_guest_manager_rules() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let guest = create_user(&database, "active", "guest").await;
    let store = ProjectStore::new(database.pool().clone());
    let project = store
        .create(
            &owner,
            "RULE",
            "Membership rules",
            None,
            at("2026-10-12T13:14:15Z"),
        )
        .await
        .unwrap();

    assert!(
        sqlx::query("UPDATE projects SET project_key = 'OTHER' WHERE id = ?")
            .bind(&project.id)
            .execute(database.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(&project.id)
            .execute(database.pool())
            .await
            .is_err()
    );

    assert!(
        sqlx::query(
            "INSERT INTO project_members \
         (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, 'manager', '2026-10-12T13:14:15.000000000Z')",
        )
        .bind(&project.id)
        .bind(&guest)
        .execute(database.pool())
        .await
        .is_err()
    );

    assert!(
        sqlx::query("UPDATE organization_members SET role = 'guest' WHERE user_id = ?")
            .bind(&owner)
            .execute(database.pool())
            .await
            .is_err()
    );

    let owner_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(owner_role, "owner");
}

#[tokio::test]
async fn project_migration_preserves_existing_audit_rows_and_reinstates_immutability() {
    let directory = TempDir::new().unwrap();
    let database_path = directory.path().join("migration-upgrade.sqlite3");
    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(database_path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();

    sqlx::raw_sql(
        "CREATE TABLE organizations (id INTEGER PRIMARY KEY); \
         CREATE TABLE users (id TEXT PRIMARY KEY); \
         CREATE TABLE organization_members ( \
             organization_id INTEGER NOT NULL, user_id TEXT NOT NULL, role TEXT NOT NULL, \
             PRIMARY KEY (organization_id, user_id), \
             FOREIGN KEY (organization_id) REFERENCES organizations (id), \
             FOREIGN KEY (user_id) REFERENCES users (id) \
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organizations (id) VALUES (1)")
        .execute(&pool)
        .await
        .unwrap();
    let actor = Uuid::now_v7().to_string();
    let event_id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id) VALUES (?)")
        .bind(&actor)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0012_audit_events.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.email.address_added', 'success', ?, 'email_address', ?, \
                 '2026-10-12T13:14:15.000000000Z')",
    )
    .bind(&event_id)
    .bind(&actor)
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!(
        "../migrations/0013_projects_and_project_audit.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let copied_event = sqlx::query(
        "SELECT event_kind, result, actor_user_id, target_kind, occurred_at \
         FROM audit_events WHERE id = ?",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        copied_event.try_get::<String, _>("event_kind").unwrap(),
        "auth.email.address_added"
    );
    assert_eq!(
        copied_event.try_get::<String, _>("result").unwrap(),
        "success"
    );
    assert_eq!(
        copied_event.try_get::<String, _>("actor_user_id").unwrap(),
        actor
    );
    assert_eq!(
        copied_event.try_get::<String, _>("target_kind").unwrap(),
        "email_address"
    );
    assert_eq!(
        copied_event.try_get::<String, _>("occurred_at").unwrap(),
        "2026-10-12T13:14:15.000000000Z"
    );
    assert!(
        sqlx::query("UPDATE audit_events SET result = 'failure' WHERE id = ?")
            .bind(&event_id)
            .execute(&pool)
            .await
            .is_err()
    );

    pool.close().await;
}
