use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService,
        password_reset::{PasswordResetPolicy, PasswordResetStore},
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

static PASSWORD_RESET_KDF_LOCK: Mutex<()> = Mutex::const_new(());

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_account(
    database: &Database,
    email: &str,
    state: &str,
    confirmed: bool,
    with_password: bool,
) -> (String, String) {
    let user_id = Uuid::now_v7().to_string();
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Password reset test', ?, \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 0, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&email_id)
    .bind(&user_id)
    .bind(email)
    .bind(email.to_ascii_lowercase())
    .bind(if confirmed {
        Some("2026-10-10T00:00:00.000000000Z")
    } else {
        None
    })
    .execute(database.pool())
    .await
    .unwrap();
    if with_password {
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, '$argon2id$test-fixture', '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
    }
    (user_id, email_id)
}

fn store(database: &Database, password_service: PasswordService) -> PasswordResetStore {
    PasswordResetStore::new(
        database.pool().clone(),
        password_service,
        PasswordResetPolicy::new(TimeDelta::hours(1)).unwrap(),
    )
}

#[test]
fn reset_policy_requires_a_positive_lifetime() {
    assert!(PasswordResetPolicy::new(TimeDelta::zero()).is_err());
    assert!(PasswordResetPolicy::new(TimeDelta::seconds(-1)).is_err());
    assert!(PasswordResetPolicy::new(TimeDelta::seconds(1)).is_ok());
}

#[tokio::test]
async fn reset_issue_is_confirmed_account_scoped_digest_only_and_reissue_invalidates_old_token() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_account(&database, "active@example.test", "active", true, true).await;
    create_account(&database, "pending@example.test", "active", false, true).await;
    create_account(&database, "banned@example.test", "banned", true, true).await;
    create_account(
        &database,
        "passwordless@example.test",
        "active",
        true,
        false,
    )
    .await;
    let store = store(&database, PasswordService::default());
    let now = at("2026-10-10T12:00:00Z");

    let first = store
        .issue_for_email(" ACTIVE@EXAMPLE.TEST ", now)
        .await
        .unwrap()
        .unwrap();
    let digest: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM password_reset_tokens WHERE id = ?")
            .bind(&first.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(digest.len(), 32);
    assert_ne!(digest.as_slice(), first.token.expose().as_bytes());
    assert_eq!(first.expires_at, now + TimeDelta::hours(1));

    let second = store
        .issue_for_email("active@example.test", now + TimeDelta::minutes(1))
        .await
        .unwrap()
        .unwrap();
    let invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM password_reset_tokens WHERE id = ?")
            .bind(&first.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(invalidated.is_some());
    assert!(
        store
            .reset_password(
                &first.token,
                String::from("a new long password phrase"),
                now + TimeDelta::minutes(2),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .issue_for_email("missing@example.test", now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .issue_for_email("pending@example.test", now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .issue_for_email("banned@example.test", now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .issue_for_email("passwordless@example.test", now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .issue_for_email("not an email", now)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(second.token.expose().len(), 43);
}

#[tokio::test]
async fn valid_reset_changes_password_and_revokes_sessions_and_other_reset_tokens_atomically() {
    let _guard = PASSWORD_RESET_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let (user_id, _email_id) =
        create_account(&database, "reset@example.test", "active", true, true).await;
    let (other_user_id, _other_email_id) =
        create_account(&database, "unused-id@example.test", "active", true, false).await;
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, 'secondary@example.test', 'secondary@example.test', 0, \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    let password_service = PasswordService::default();
    let reset_store = store(&database, password_service.clone());
    let now = at("2026-10-10T12:00:00Z");
    let reset = reset_store
        .issue_for_email("reset@example.test", now)
        .await
        .unwrap()
        .unwrap();
    let other_reset = reset_store
        .issue_for_email("secondary@example.test", now)
        .await
        .unwrap()
        .unwrap();
    let session_store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let session_one = session_store.issue(&user_id, now).await.unwrap();
    let session_two = session_store.issue(&user_id, now).await.unwrap();
    let other_session = session_store.issue(&other_user_id, now).await.unwrap();

    let result = reset_store
        .reset_password(
            &reset.token,
            String::from("new strong recovery password"),
            now + TimeDelta::minutes(5),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.revoked_sessions, 2);
    assert!(
        session_store
            .validate(&session_one.token, now + TimeDelta::minutes(6))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session_store
            .validate(&session_two.token, now + TimeDelta::minutes(6))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session_store
            .validate(&other_session.token, now + TimeDelta::minutes(6))
            .await
            .unwrap()
            .is_some()
    );
    let stored_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(
        password_service
            .verify(String::from("new strong recovery password"), stored_hash,)
            .await
            .unwrap()
    );
    let reset_audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE event_kind = 'auth.password.reset_completed' \
           AND target_kind = 'user' AND target_id = ?",
    )
    .bind(&user_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(reset_audit_count, 1);
    let second_result = reset_store
        .reset_password(
            &other_reset.token,
            String::from("another strong recovery phrase"),
            now + TimeDelta::minutes(6),
        )
        .await
        .unwrap();
    assert!(second_result.is_none());
}

#[tokio::test]
async fn malformed_future_and_expired_persisted_reset_times_fail_closed_without_hashing() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_account(&database, "timestamp@example.test", "active", true, true).await;
    let store = store(&database, PasswordService::default());
    let now = at("2026-10-10T12:00:00Z");

    let expired = store
        .issue_for_email("timestamp@example.test", now)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE password_reset_tokens SET expires_at = ? WHERE id = ?")
        .bind("2026-10-10T12:00:00.000000000Z")
        .bind(&expired.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .reset_password(
                &expired.token,
                String::from("new valid password phrase"),
                now,
            )
            .await
            .unwrap()
            .is_none()
    );

    let future = store
        .issue_for_email("timestamp@example.test", now)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE password_reset_tokens SET created_at = ? WHERE id = ?")
        .bind("2026-10-10T12:01:00.000000000Z")
        .bind(&future.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .reset_password(
                &future.token,
                String::from("new valid password phrase"),
                now,
            )
            .await
            .unwrap()
            .is_none()
    );

    let malformed = store
        .issue_for_email("timestamp@example.test", now)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE password_reset_tokens SET expires_at = 'bad' WHERE id = ?")
        .bind(&malformed.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .reset_password(
                &malformed.token,
                String::from("new valid password phrase"),
                now,
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn password_reset_and_session_revocation_roll_back_together() {
    let _guard = PASSWORD_RESET_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let (user_id, _) = create_account(&database, "atomic@example.test", "active", true, true).await;
    let password_service = PasswordService::default();
    let store = store(&database, password_service);
    let now = at("2026-10-10T12:00:00Z");
    let reset = store
        .issue_for_email("atomic@example.test", now)
        .await
        .unwrap()
        .unwrap();
    let session_store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    session_store.issue(&user_id, now).await.unwrap();
    let previous_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_password_reset_revoke BEFORE UPDATE OF revoked_at ON sessions \
         BEGIN SELECT RAISE(ABORT, 'fixture revoke failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(
        store
            .reset_password(
                &reset.token,
                String::from("atomic new password phrase"),
                now + TimeDelta::minutes(1),
            )
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_password_reset_revoke")
        .execute(database.pool())
        .await
        .unwrap();
    let current_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM password_credentials WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(current_hash, previous_hash);
    let revoked_at: Option<String> = sqlx::query_scalar("SELECT revoked_at FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert!(revoked_at.is_none());

    let success = store
        .reset_password(
            &reset.token,
            String::from("atomic new password phrase"),
            now + TimeDelta::minutes(2),
        )
        .await
        .unwrap();
    assert!(
        success.is_some(),
        "failed transaction must not consume token"
    );
}

#[tokio::test]
async fn concurrent_reset_attempts_with_one_token_have_one_winner() {
    let _guard = PASSWORD_RESET_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_account(&database, "race@example.test", "active", true, true).await;
    let store = store(&database, PasswordService::default());
    let now = at("2026-10-10T12:00:00Z");
    let reset = store
        .issue_for_email("race@example.test", now)
        .await
        .unwrap()
        .unwrap();

    let (first, second) = tokio::join!(
        store.reset_password(
            &reset.token,
            String::from("first concurrent reset password"),
            now + TimeDelta::minutes(1),
        ),
        store.reset_password(
            &reset.token,
            String::from("second concurrent reset password"),
            now + TimeDelta::minutes(1),
        ),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.is_some(), second.is_some());
}
