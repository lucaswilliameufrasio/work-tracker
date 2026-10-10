use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::session::{SessionError, SessionPolicy, SessionStore},
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

async fn create_user(database: &Database, state: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Session test', ?, '2026-10-09T00:00:00.000000000Z', \
                 '2026-10-09T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

#[tokio::test]
async fn issue_persists_only_token_digest_and_uses_configured_absolute_expiry() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let policy = SessionPolicy::new(
        TimeDelta::days(7),
        TimeDelta::hours(24),
        TimeDelta::minutes(5),
    )
    .unwrap();
    let store = SessionStore::new(database.pool().clone(), policy);
    let issued_at = at("2026-10-09T12:00:00Z");

    let issued = store.issue(&user_id, issued_at).await.unwrap();
    assert_eq!(issued.expires_at, issued_at + TimeDelta::days(7));

    let row = sqlx::query(
        "SELECT token_hash, user_id, created_at, expires_at FROM sessions WHERE id = ?",
    )
    .bind(&issued.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let stored_digest: Vec<u8> = row.try_get("token_hash").unwrap();
    assert_eq!(stored_digest, issued.token.digest());
    assert_eq!(row.try_get::<String, _>("user_id").unwrap(), user_id);
    assert_eq!(
        row.try_get::<String, _>("created_at").unwrap(),
        "2026-10-09T12:00:00.000000000Z"
    );
    assert_eq!(
        row.try_get::<String, _>("expires_at").unwrap(),
        "2026-10-16T12:00:00.000000000Z"
    );
    assert_eq!(issued.token.expose().len(), 43);
}

#[tokio::test]
async fn inactive_users_cannot_receive_a_persisted_session() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "banned").await;
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let store = SessionStore::new(database.pool().clone(), policy);

    assert!(matches!(
        store.issue(&user_id, at("2026-10-09T12:00:00Z")).await,
        Err(SessionError::UserNotActive)
    ));
    let session_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(session_count, 0);
}

#[test]
fn session_policy_rejects_non_positive_or_excessive_touch_intervals() {
    assert!(
        SessionPolicy::new(TimeDelta::days(7), TimeDelta::hours(24), TimeDelta::zero(),).is_err()
    );
    assert!(
        SessionPolicy::new(
            TimeDelta::days(7),
            TimeDelta::hours(24),
            TimeDelta::hours(24),
        )
        .is_err()
    );
}
