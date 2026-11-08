use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        SessionToken,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    projects::{
        ProjectListOptions, ProjectReadError, ProjectStore,
        session::{SessionProjectReadError, SessionProjectReader},
    },
};
use tempfile::TempDir;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-16T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

async fn setup(database: &Database) {
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Principal tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool()).await.unwrap();
}

async fn user(database: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Principal user', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')")
        .bind(&id).bind(role).execute(database.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn session_bound_project_queries_never_take_actor_identity_from_the_caller() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    setup(&database).await;
    let owner = user(&database, "owner").await;
    let other = user(&database, "owner").await;
    let projects = ProjectStore::new(database.pool().clone());
    let own_project = projects
        .create(&owner, "OWN", "Own project", None, now())
        .await
        .unwrap();
    let other_project = projects
        .create(&other, "OTHER", "Other project", None, now())
        .await
        .unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let own_session = sessions.issue(&owner, now()).await.unwrap();
    let other_session = sessions.issue(&other, now()).await.unwrap();
    let reader = SessionProjectReader::new(database.pool().clone(), policy());
    for (session, expected) in [
        (&own_session, &own_project),
        (&other_session, &other_project),
    ] {
        let page = reader
            .list_accessible(&session.token, ProjectListOptions::default(), now())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, expected.id);
    }
    // Organization oversight remains separate even with an authenticated owner.
    let catalog = reader
        .list_operational_catalog(&own_session.token, ProjectListOptions::default(), now())
        .await
        .unwrap();
    assert_eq!(catalog.items.len(), 2);
    sqlx::query("UPDATE organization_members SET role = 'member' WHERE user_id = ?")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .list_operational_catalog(&own_session.token, ProjectListOptions::default(), now())
            .await,
        Err(SessionProjectReadError::Project(
            ProjectReadError::Unavailable
        ))
    ));
    assert_eq!(
        reader
            .list_accessible(&own_session.token, ProjectListOptions::default(), now())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    sessions
        .revoke(&owner, &own_session.id, now())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .list_accessible(&own_session.token, ProjectListOptions::default(), now())
            .await,
        Err(SessionProjectReadError::Unauthenticated)
    ));
    assert_eq!(
        reader
            .list_accessible(&other_session.token, ProjectListOptions::default(), now())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn principal_validation_uses_callers_transaction_and_does_not_touch_activity() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    setup(&database).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, now()).await.unwrap();
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    let principal = sessions
        .authenticate_in_transaction(
            &mut transaction,
            &issued.token,
            now() + TimeDelta::minutes(10),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.user_id(), actor);
    assert_eq!(principal.session_id(), issued.id);
    let last_activity: String =
        sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(last_activity, "2026-10-16T09:00:00.000000000Z");
    sqlx::query("UPDATE sessions SET revoked_at = '2026-10-16T09:01:00Z' WHERE id = ?")
        .bind(&issued.id)
        .execute(&mut *transaction)
        .await
        .unwrap();
    assert!(
        sessions
            .authenticate_in_transaction(&mut transaction, &issued.token, now())
            .await
            .unwrap()
            .is_none()
    );
    transaction.rollback().await.unwrap();
    let mut transaction = database.pool().begin().await.unwrap();
    assert!(
        sessions
            .authenticate_in_transaction(&mut transaction, &issued.token, now())
            .await
            .unwrap()
            .is_some()
    );
    transaction.commit().await.unwrap();
}

#[tokio::test]
async fn read_snapshot_has_defined_revocation_order_and_new_snapshots_fail_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    setup(&database).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, now()).await.unwrap();
    let mut snapshot = database.pool().begin().await.unwrap();
    assert!(
        sessions
            .authenticate_in_transaction(&mut snapshot, &issued.token, now())
            .await
            .unwrap()
            .is_some()
    );
    sessions.revoke(&actor, &issued.id, now()).await.unwrap();
    // The existing read is linearized before the concurrent writer's commit.
    assert!(
        sessions
            .authenticate_in_transaction(&mut snapshot, &issued.token, now())
            .await
            .unwrap()
            .is_some()
    );
    snapshot.commit().await.unwrap();
    let mut next = database.pool().begin().await.unwrap();
    assert!(
        sessions
            .authenticate_in_transaction(&mut next, &issued.token, now())
            .await
            .unwrap()
            .is_none()
    );
    next.commit().await.unwrap();
}

#[tokio::test]
async fn unknown_inactive_expired_future_and_malformed_sessions_never_become_principals() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    setup(&database).await;
    let actor = user(&database, "member").await;
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let issued = sessions.issue(&actor, now()).await.unwrap();
    let reader = SessionProjectReader::new(database.pool().clone(), policy());
    let unknown = SessionToken::generate().unwrap();
    assert!(matches!(
        reader
            .list_accessible(&unknown, ProjectListOptions::default(), now())
            .await,
        Err(SessionProjectReadError::Unauthenticated)
    ));
    for at in [
        now() - TimeDelta::nanoseconds(1),
        now() + TimeDelta::hours(24),
        now() + TimeDelta::days(7),
    ] {
        assert!(matches!(
            reader
                .list_accessible(&issued.token, ProjectListOptions::default(), at)
                .await,
            Err(SessionProjectReadError::Unauthenticated)
        ));
    }
    for state in ["banned", "disabled"] {
        sqlx::query("UPDATE users SET state = ? WHERE id = ?")
            .bind(state)
            .bind(&actor)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(matches!(
            reader
                .list_accessible(&issued.token, ProjectListOptions::default(), now())
                .await,
            Err(SessionProjectReadError::Unauthenticated)
        ));
    }
    sqlx::query("UPDATE users SET state = 'active' WHERE id = ?")
        .bind(&actor)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = 'malformed' WHERE id = ?")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .list_accessible(&issued.token, ProjectListOptions::default(), now())
            .await,
        Err(SessionProjectReadError::Unauthenticated)
    ));
}

#[tokio::test]
async fn session_bound_queries_report_database_failure_instead_of_anonymous_or_empty_success() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let reader = SessionProjectReader::new(database.pool().clone(), policy());
    database.pool().close().await;
    assert!(matches!(
        reader
            .list_accessible(
                &SessionToken::generate().unwrap(),
                ProjectListOptions::default(),
                now()
            )
            .await,
        Err(SessionProjectReadError::Database(_))
    ));
}
