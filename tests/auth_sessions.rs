use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService,
        credentials::{CredentialError, CredentialStore},
        session::{SessionError, SessionPolicy, SessionStore},
    },
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::Zeroizing;

static PASSWORD_KDF_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

#[tokio::test]
async fn validation_touches_only_after_interval_and_expires_at_idle_boundary() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, issued_at).await.unwrap();

    let before_touch = issued_at + TimeDelta::minutes(4) + TimeDelta::seconds(59);
    assert!(
        store
            .validate(&issued.token, before_touch)
            .await
            .unwrap()
            .is_some()
    );
    let last_activity: String =
        sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(last_activity, "2026-10-09T12:00:00.000000000Z");

    let touch_at = issued_at + TimeDelta::minutes(5);
    assert!(
        store
            .validate(&issued.token, touch_at)
            .await
            .unwrap()
            .is_some()
    );
    let last_activity: String =
        sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(last_activity, "2026-10-09T12:05:00.000000000Z");

    let idle_deadline = touch_at + TimeDelta::hours(24);
    assert!(
        store
            .validate(&issued.token, idle_deadline)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn validation_rejects_unknown_tokens_absolute_expiry_and_inactive_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, issued_at).await.unwrap();
    let unknown = puxiri::auth::SessionToken::generate().unwrap();

    assert!(store.validate(&unknown, issued_at).await.unwrap().is_none());
    let absolute_deadline = issued_at + TimeDelta::days(7);
    assert!(
        store
            .validate(&issued.token, absolute_deadline)
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .validate(&issued.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn validation_fails_closed_on_malformed_persisted_timestamps() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, issued_at).await.unwrap();

    sqlx::query("UPDATE sessions SET last_activity_at = 'not-a-timestamp' WHERE id = ?")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .validate(&issued.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("UPDATE sessions SET last_activity_at = ? WHERE id = ?")
        .bind("2026-10-09T12:00:00.000000000Z")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = 'not-a-timestamp' WHERE id = ?")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .validate(&issued.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("UPDATE sessions SET expires_at = ? WHERE id = ?")
        .bind("2026-10-16T12:00:00.000000000Z")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET created_at = 'not-a-timestamp' WHERE id = ?")
        .bind(&issued.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .validate(&issued.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn issued_session_and_revocation_survive_database_reopen() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let store = SessionStore::new(database.pool().clone(), policy);
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, issued_at).await.unwrap();
    let session_id = issued.id.clone();
    let token_value = Zeroizing::new(issued.token.expose().to_owned());

    drop(issued);
    drop(store);
    database.close().await;
    drop(database);

    let reopened = Database::open(directory.path()).await.unwrap();
    let reopened_store = SessionStore::new(reopened.pool().clone(), policy);
    let token = puxiri::auth::SessionToken::parse(token_value.as_str()).unwrap();
    assert!(
        reopened_store
            .validate(&token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        reopened_store
            .revoke(&user_id, &session_id, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
    );

    drop(token);
    drop(reopened_store);
    reopened.close().await;
    drop(reopened);

    let recovered = Database::open(directory.path()).await.unwrap();
    let recovered_store = SessionStore::new(recovered.pool().clone(), policy);
    let token = puxiri::auth::SessionToken::parse(token_value.as_str()).unwrap();
    assert!(
        recovered_store
            .validate(&token, issued_at + TimeDelta::minutes(3))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn revocation_is_owner_scoped_idempotent_and_revokes_all_remaining_sessions() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let first = store.issue(&user_id, issued_at).await.unwrap();
    let second = store.issue(&user_id, issued_at).await.unwrap();

    assert!(
        !store
            .revoke("another-user", &first.id, issued_at)
            .await
            .unwrap()
    );
    assert!(
        store
            .validate(&first.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .revoke(&user_id, &first.id, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
    );
    assert!(
        !store
            .revoke(&user_id, &first.id, issued_at + TimeDelta::minutes(3))
            .await
            .unwrap()
    );
    assert!(
        store
            .validate(&first.token, issued_at + TimeDelta::minutes(3))
            .await
            .unwrap()
            .is_none()
    );

    assert_eq!(
        store
            .revoke_all(&user_id, issued_at + TimeDelta::minutes(4))
            .await
            .unwrap(),
        1
    );
    assert!(
        store
            .validate(&second.token, issued_at + TimeDelta::minutes(4))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .revoke_all(&user_id, issued_at + TimeDelta::minutes(5))
            .await
            .unwrap(),
        0
    );
    let audit_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE event_kind IN ('auth.session.revoked', 'auth.sessions.revoked')",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_events, 2);
}

#[tokio::test]
async fn active_session_listing_is_owner_scoped_filters_invalid_rows_and_marks_current() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let other_user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = at("2026-10-10T12:00:00Z");
    let first = store
        .issue(&user_id, now - TimeDelta::hours(1))
        .await
        .unwrap();
    let current = store
        .issue(&user_id, now - TimeDelta::minutes(10))
        .await
        .unwrap();
    let revoked = store
        .issue(&user_id, now - TimeDelta::minutes(5))
        .await
        .unwrap();
    store
        .revoke(&user_id, &revoked.id, now - TimeDelta::minutes(1))
        .await
        .unwrap();
    let expired = store
        .issue(&user_id, now - TimeDelta::days(8))
        .await
        .unwrap();
    let malformed = store
        .issue(&user_id, now - TimeDelta::minutes(2))
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = 'invalid' WHERE id = ?")
        .bind(&malformed.id)
        .execute(database.pool())
        .await
        .unwrap();
    let other = store
        .issue(&other_user_id, now - TimeDelta::minutes(3))
        .await
        .unwrap();

    let sessions = store.list_active(&user_id, &current.id, now).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, current.id);
    assert!(sessions[0].is_current);
    assert_eq!(sessions[1].id, first.id);
    assert!(!sessions[1].is_current);
    assert_eq!(sessions[0].created_at, now - TimeDelta::minutes(10));
    assert_eq!(
        sessions[0].expires_at,
        now - TimeDelta::minutes(10) + TimeDelta::days(7)
    );
    assert!(!sessions.iter().any(|session| session.id == expired.id));
    assert!(!sessions.iter().any(|session| session.id == revoked.id));
    assert!(!sessions.iter().any(|session| session.id == malformed.id));

    let other_sessions = store
        .list_active(&other_user_id, &other.id, now)
        .await
        .unwrap();
    assert_eq!(other_sessions.len(), 1);
    assert_eq!(other_sessions[0].id, other.id);
    assert!(other_sessions[0].is_current);
}

#[tokio::test]
async fn concurrent_activity_touches_do_not_reject_a_live_session() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, issued_at).await.unwrap();
    let touch_at = issued_at + TimeDelta::minutes(5);

    let (first, second) = tokio::join!(
        store.validate(&issued.token, touch_at),
        store.validate(&issued.token, touch_at),
    );
    assert!(first.unwrap().is_some());
    assert!(second.unwrap().is_some());

    let last_activity: String =
        sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(last_activity, "2026-10-09T12:05:00.000000000Z");
}

#[tokio::test]
async fn setting_password_persists_only_phc_and_revokes_other_sessions() {
    let _kdf_test_guard = PASSWORD_KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy);
    let password_service = PasswordService::default();
    let credentials = CredentialStore::new(database.pool().clone(), password_service.clone());
    let issued_at = at("2026-10-09T12:00:00Z");
    let current = sessions.issue(&user_id, issued_at).await.unwrap();
    let other = sessions.issue(&user_id, issued_at).await.unwrap();
    let password = String::from("another sufficiently strong passphrase");

    assert_eq!(
        credentials
            .set_password(
                Some(&user_id),
                &user_id,
                password.clone(),
                issued_at + TimeDelta::minutes(1),
                Some(&current.id),
            )
            .await
            .unwrap(),
        1
    );
    let encoded_hash = credentials.password_hash(&user_id).await.unwrap().unwrap();
    assert!(encoded_hash.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"));
    let changed_at: String =
        sqlx::query_scalar("SELECT changed_at FROM password_credentials WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(changed_at, "2026-10-09T12:01:00.000000000Z");
    assert!(
        password_service
            .verify(password, encoded_hash.clone())
            .await
            .unwrap()
    );
    assert!(
        !password_service
            .verify(String::from("incorrect password value"), encoded_hash)
            .await
            .unwrap()
    );
    assert!(
        sessions
            .validate(&current.token, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions
            .validate(&other.token, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn inactive_user_cannot_receive_password_credential() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "banned").await;
    let credentials = CredentialStore::new(database.pool().clone(), PasswordService::default());

    assert!(matches!(
        credentials
            .set_password(
                None,
                &user_id,
                String::from("another sufficiently strong passphrase"),
                at("2026-10-09T12:00:00Z"),
                None,
            )
            .await,
        Err(CredentialError::UserNotActive)
    ));
    assert!(credentials.password_hash(&user_id).await.unwrap().is_none());
}

#[tokio::test]
async fn failed_session_revocation_rolls_back_password_replacement() {
    let _kdf_test_guard = PASSWORD_KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let issued_at = at("2026-10-09T12:00:00Z");
    let session = sessions.issue(&user_id, issued_at).await.unwrap();
    let credentials = CredentialStore::new(database.pool().clone(), PasswordService::default());
    sqlx::query(
        "CREATE TRIGGER fail_session_revoke \
         BEFORE UPDATE OF revoked_at ON sessions \
         BEGIN SELECT RAISE(ABORT, 'test revocation failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(matches!(
        credentials
            .set_password(
                Some(&user_id),
                &user_id,
                String::from("another sufficiently strong passphrase"),
                at("2026-10-09T12:00:00Z"),
                None,
            )
            .await,
        Err(CredentialError::Database(_))
    ));
    assert!(credentials.password_hash(&user_id).await.unwrap().is_none());
    assert!(
        sessions
            .validate(&session.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn password_verification_uses_dummy_hash_for_missing_and_inactive_users() {
    let _kdf_test_guard = PASSWORD_KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let active_user_id = create_user(&database, "active").await;
    let inactive_user_id = create_user(&database, "banned").await;
    let password_service = PasswordService::default();
    let credentials = CredentialStore::new(database.pool().clone(), password_service.clone());
    let password = String::from("a distinct strong test passphrase");
    let password_hash = password_service.hash(password.clone()).await.unwrap();

    for user_id in [&active_user_id, &inactive_user_id] {
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, '2026-10-09T12:00:00.000000000Z')",
        )
        .bind(user_id)
        .bind(&password_hash)
        .execute(database.pool())
        .await
        .unwrap();
    }

    assert!(
        credentials
            .verify_password_for_user(&active_user_id, password.clone(), &password_hash)
            .await
            .unwrap()
    );
    assert!(
        !credentials
            .verify_password_for_user(
                &active_user_id,
                String::from("a different test passphrase"),
                &password_hash,
            )
            .await
            .unwrap()
    );
    assert!(
        !credentials
            .verify_password_for_user(&inactive_user_id, password.clone(), &password_hash)
            .await
            .unwrap()
    );
    assert!(
        !credentials
            .verify_password_for_user("missing-user", password, &password_hash)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn password_verification_resolves_only_confirmed_emails_with_ascii_case_folding() {
    let _kdf_test_guard = PASSWORD_KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let active_user_id = create_user(&database, "active").await;
    let banned_user_id = create_user(&database, "banned").await;
    let active_email_id = Uuid::now_v7().to_string();
    let pending_email_id = Uuid::now_v7().to_string();
    let banned_email_id = Uuid::now_v7().to_string();
    let password = String::from("a distinct strong email password");
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(password.clone()).await.unwrap();

    for (email_id, user_id, address, confirmed_at) in [
        (
            &active_email_id,
            &active_user_id,
            "Alice.Example+tag@example.test",
            Some("2026-10-09T12:00:00.000000000Z"),
        ),
        (
            &pending_email_id,
            &active_user_id,
            "pending@example.test",
            None,
        ),
        (
            &banned_email_id,
            &banned_user_id,
            "banned@example.test",
            Some("2026-10-09T12:00:00.000000000Z"),
        ),
    ] {
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES (?, ?, ?, ?, 0, ?, '2026-10-09T12:00:00.000000000Z')",
        )
        .bind(email_id)
        .bind(user_id)
        .bind(address)
        .bind(address.to_ascii_lowercase())
        .bind(confirmed_at)
        .execute(database.pool())
        .await
        .unwrap();
    }
    for user_id in [&active_user_id, &banned_user_id] {
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, '2026-10-09T12:00:00.000000000Z')",
        )
        .bind(user_id)
        .bind(&password_hash)
        .execute(database.pool())
        .await
        .unwrap();
    }

    let credentials = CredentialStore::new(database.pool().clone(), password_service);
    assert!(
        credentials
            .verify_password_for_email(
                "  ALICE.EXAMPLE+TAG@EXAMPLE.TEST ",
                password.clone(),
                &password_hash,
            )
            .await
            .unwrap()
    );
    for email in [
        "pending@example.test",
        "banned@example.test",
        "aliceexample+tag@example.test",
        "alice.example@example.test",
        "not-an-email",
    ] {
        assert!(
            !credentials
                .verify_password_for_email(email, password.clone(), &password_hash)
                .await
                .unwrap()
        );
    }
}
