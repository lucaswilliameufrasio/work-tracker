use chrono::{DateTime, Utc};
use puxiri::{
    audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget},
    auth::{PasswordService, credentials::CredentialStore, session::SessionStore},
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

static KDF_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_user(database: &Database) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Audit event test', 'active', \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

#[tokio::test]
async fn append_records_only_actor_operation_opaque_target_and_time() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let actor = create_user(&database).await;
    let email_id = Uuid::now_v7().to_string();
    let now = at("2026-10-10T12:00:00Z");
    let mut transaction = database.pool().begin().await.unwrap();

    let event_id = audit::append_in_transaction(
        &mut transaction,
        Some(&actor),
        AuditEventKind::EmailAddressPrimaryChanged,
        AuditEventResult::Success,
        &AuditEventTarget::email_address(&email_id),
        now,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();

    let row = sqlx::query(
        "SELECT id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at \
         FROM audit_events WHERE id = ?",
    )
    .bind(&event_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("id").unwrap(), event_id);
    assert_eq!(
        row.try_get::<String, _>("event_kind").unwrap(),
        "auth.email.primary_changed"
    );
    assert_eq!(row.try_get::<String, _>("result").unwrap(), "success");
    assert_eq!(row.try_get::<String, _>("actor_user_id").unwrap(), actor);
    assert_eq!(
        row.try_get::<String, _>("target_kind").unwrap(),
        "email_address"
    );
    assert_eq!(row.try_get::<String, _>("target_id").unwrap(), email_id);
    assert_eq!(
        row.try_get::<String, _>("occurred_at").unwrap(),
        "2026-10-10T12:00:00.000000000Z"
    );

    let columns = sqlx::query("PRAGMA table_info(audit_events)")
        .fetch_all(database.pool())
        .await
        .unwrap();
    let mut column_names = Vec::with_capacity(columns.len());
    for column in columns {
        column_names.push(column.try_get::<String, _>("name").unwrap());
    }
    assert_eq!(
        column_names,
        [
            "id",
            "event_kind",
            "result",
            "actor_user_id",
            "target_kind",
            "target_id",
            "occurred_at"
        ]
    );
    assert!(
        sqlx::query("UPDATE audit_events SET event_kind = 'auth.email.address_removed'")
            .execute(database.pool())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn event_and_protected_write_share_the_callers_transaction() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let actor = create_user(&database).await;
    let target = Uuid::now_v7().to_string();
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    audit::append_in_transaction(
        &mut transaction,
        Some(&actor),
        AuditEventKind::EmailAddressAdded,
        AuditEventResult::Success,
        &AuditEventTarget::email_address(&target),
        at("2026-10-10T12:00:00Z"),
    )
    .await
    .unwrap();
    transaction.rollback().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn audit_result_records_failure_as_a_distinct_bounded_outcome() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let actor = create_user(&database).await;
    let target = Uuid::now_v7().to_string();
    let mut transaction = database.pool().begin().await.unwrap();
    let event_id = audit::append_in_transaction(
        &mut transaction,
        Some(&actor),
        AuditEventKind::EmailAddressAdded,
        AuditEventResult::Failure,
        &AuditEventTarget::email_address(&target),
        at("2026-10-10T12:00:00Z"),
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();

    let result: String = sqlx::query_scalar("SELECT result FROM audit_events WHERE id = ?")
        .bind(event_id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(result, "failure");
}

#[tokio::test]
async fn event_kinds_targets_and_actor_references_are_database_constrained() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database).await;

    assert!(
        sqlx::query(
            "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.untrusted.arbitrary', 'success', ?, 'user', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&user_id)
        .bind(Uuid::now_v7().to_string())
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.email.address_added', 'success', ?, 'secret', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&user_id)
        .bind(Uuid::now_v7().to_string())
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.email.address_added', 'success', ?, 'email_address', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind("missing-user")
        .bind(Uuid::now_v7().to_string())
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.email.address_added', 'unknown', ?, 'email_address', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&user_id)
        .bind(Uuid::now_v7().to_string())
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn password_change_audit_commits_with_the_change_and_rolls_back_on_audit_failure() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database).await;
    let password_service = PasswordService::default();
    let original_hash = password_service
        .hash(String::from("a sufficiently long original password"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(&original_hash)
    .execute(database.pool())
    .await
    .unwrap();

    let now = at("2026-10-10T12:00:00Z");
    let sessions = SessionStore::new(
        database.pool().clone(),
        puxiri::auth::session::SessionPolicy::initial(chrono::TimeDelta::minutes(5)).unwrap(),
    );
    let first_session = sessions.issue(&user_id, now).await.unwrap();
    let credentials = CredentialStore::new(database.pool().clone(), password_service);
    assert_eq!(
        credentials
            .set_password(
                Some(&user_id),
                &user_id,
                String::from("a sufficiently long replacement password"),
                now,
                None,
            )
            .await
            .unwrap(),
        1
    );

    let event = sqlx::query(
        "SELECT event_kind, result, actor_user_id, target_kind, target_id \
         FROM audit_events WHERE event_kind = 'auth.password.changed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        event.try_get::<String, _>("event_kind").unwrap(),
        "auth.password.changed"
    );
    assert_eq!(event.try_get::<String, _>("result").unwrap(), "success");
    assert_eq!(
        event.try_get::<String, _>("actor_user_id").unwrap(),
        user_id
    );
    assert_eq!(event.try_get::<String, _>("target_kind").unwrap(), "user");
    assert_eq!(event.try_get::<String, _>("target_id").unwrap(), user_id);
    assert!(
        sessions
            .validate(&first_session.token, now)
            .await
            .unwrap()
            .is_none()
    );

    let second_session = sessions.issue(&user_id, now).await.unwrap();
    let hash_before_failure = credentials.password_hash(&user_id).await.unwrap().unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_password_change_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.password.changed' \
         BEGIN SELECT RAISE(ABORT, 'test audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        credentials
            .set_password(
                Some(&user_id),
                &user_id,
                String::from("a sufficiently long later password"),
                now + chrono::TimeDelta::seconds(1),
                None,
            )
            .await
            .is_err()
    );

    let stored_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(stored_hash, hash_before_failure);
    assert!(
        sessions
            .validate(&second_session.token, now)
            .await
            .unwrap()
            .is_some()
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE event_kind = 'auth.password.changed'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 1);
}
