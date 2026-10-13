use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        mfa::{
            TotpSecretBox,
            store::{TotpCredentialError, TotpCredentialStore},
        },
    },
    db::Database,
};
use sha1::Sha1;
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::Zeroizing;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_user(database: &Database, state: &str) -> Uuid {
    let user_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'TOTP credential test', ?, \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(user_id.to_string())
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

fn credential_store(database: &Database, key_path: &Path) -> TotpCredentialStore {
    let master_key = MasterKeyStore::load_or_create(key_path).unwrap();
    TotpCredentialStore::new(database.pool().clone(), TotpSecretBox::new(master_key))
}

fn code_for_step(secret_base32: &str, step: u64) -> Zeroizing<String> {
    let decoded = Zeroizing::new(
        base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_base32).unwrap(),
    );
    let mut mac = Hmac::<Sha1>::new_from_slice(&decoded).unwrap();
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    Zeroizing::new(format!("{:06}", binary % 1_000_000))
}

fn current_step(now: DateTime<Utc>) -> u64 {
    u64::try_from(now.timestamp()).unwrap() / 30
}

fn different_code(code: &str) -> Zeroizing<String> {
    let value = code.parse::<u32>().unwrap();
    Zeroizing::new(format!("{:06}", (value + 1) % 1_000_000))
}

async fn accept_step(
    store: &TotpCredentialStore,
    database: &Database,
    user_id: Uuid,
    code: &str,
    now: DateTime<Utc>,
) -> bool {
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    let accepted = store
        .verify_in_transaction(&mut transaction, user_id, code, now)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    accepted
}

#[tokio::test]
async fn enrollment_persists_only_ciphertext_and_activates_atomically() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = store.begin_enrollment(user_id, now).await.unwrap();

    assert_eq!(enrollment.expires_at(), now + TimeDelta::minutes(10));
    let row = sqlx::query(
        "SELECT id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step \
         FROM mfa_credentials WHERE user_id = ?",
    )
    .bind(user_id.to_string())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<String, _>("id").unwrap(),
        enrollment.credential_id().to_string()
    );
    let envelope: Vec<u8> = row.try_get("secret_envelope").unwrap();
    assert_eq!(envelope.len(), 65);
    let seed = Zeroizing::new(
        base32::decode(
            base32::Alphabet::Rfc4648 { padding: false },
            enrollment.secret_for_display(),
        )
        .unwrap(),
    );
    assert!(
        !envelope
            .windows(seed.len())
            .any(|window| window == seed.as_slice())
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("activated_at").unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("last_accepted_step").unwrap(),
        None
    );

    let step = current_step(now);
    let correct_code = code_for_step(enrollment.secret_for_display(), step);
    let wrong_code = different_code(correct_code.as_str());
    assert!(
        !store
            .confirm_enrollment(user_id, wrong_code.as_str(), now)
            .await
            .unwrap()
    );
    assert!(
        store
            .confirm_enrollment(user_id, correct_code.as_str(), now)
            .await
            .unwrap()
    );

    let active_row = sqlx::query(
        "SELECT expires_at, activated_at, last_accepted_step \
         FROM mfa_credentials WHERE user_id = ?",
    )
    .bind(user_id.to_string())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        active_row
            .try_get::<Option<String>, _>("expires_at")
            .unwrap(),
        None
    );
    assert_eq!(
        active_row
            .try_get::<Option<String>, _>("activated_at")
            .unwrap(),
        Some("2026-10-10T12:00:00.000000000Z".to_owned())
    );
    assert_eq!(
        active_row.try_get::<i64, _>("last_accepted_step").unwrap(),
        i64::try_from(step).unwrap()
    );
    assert!(matches!(
        store.begin_enrollment(user_id, now).await,
        Err(TotpCredentialError::AlreadyEnabled)
    ));

    let mut audit_kinds: Vec<String> = sqlx::query_scalar("SELECT event_kind FROM audit_events")
        .fetch_all(database.pool())
        .await
        .unwrap();
    audit_kinds.sort();
    assert_eq!(
        audit_kinds,
        [
            "auth.mfa.totp_enrollment_confirmed",
            "auth.mfa.totp_enrollment_started"
        ]
    );
}

#[tokio::test]
async fn pending_enrollment_replacement_invalidates_old_credential_and_secret() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let old_enrollment = store.begin_enrollment(user_id, now).await.unwrap();
    let new_enrollment = store
        .begin_enrollment(user_id, now + TimeDelta::seconds(1))
        .await
        .unwrap();

    assert_ne!(
        old_enrollment.credential_id(),
        new_enrollment.credential_id()
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_credentials WHERE user_id = ?")
        .bind(user_id.to_string())
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let persisted_id: String =
        sqlx::query_scalar("SELECT id FROM mfa_credentials WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(persisted_id, new_enrollment.credential_id().to_string());
    let new_code = code_for_step(
        new_enrollment.secret_for_display(),
        current_step(now + TimeDelta::seconds(1)),
    );
    assert!(
        store
            .confirm_enrollment(user_id, new_code.as_str(), now + TimeDelta::seconds(1))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn inactive_users_cannot_enroll_and_expiry_boundary_deletes_pending_secret() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let inactive_user_id = create_user(&database, "disabled").await;
    let active_user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");

    assert!(matches!(
        store.begin_enrollment(inactive_user_id, now).await,
        Err(TotpCredentialError::UserNotActive)
    ));
    let enrollment = store.begin_enrollment(active_user_id, now).await.unwrap();
    let code = code_for_step(
        enrollment.secret_for_display(),
        current_step(enrollment.expires_at()),
    );
    assert!(matches!(
        store
            .confirm_enrollment(active_user_id, code.as_str(), enrollment.expires_at())
            .await,
        Err(TotpCredentialError::EnrollmentExpired)
    ));
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_credentials")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    let expired_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE event_kind = 'auth.mfa.totp_enrollment_expired'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(expired_events, 1);
}

#[tokio::test]
async fn verification_is_replay_safe_and_counter_rolls_back_with_caller_transaction() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );

    assert!(!accept_step(&store, &database, user_id, activation_code.as_str(), now).await);
    assert!(!accept_step(&store, &database, Uuid::now_v7(), "123456", now).await);

    let next_time = now + TimeDelta::seconds(30);
    let next_step = code_for_step(enrollment.secret_for_display(), current_step(next_time));
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        store
            .verify_in_transaction(&mut transaction, user_id, next_step.as_str(), next_time)
            .await
            .unwrap()
    );
    transaction.rollback().await.unwrap();
    assert!(accept_step(&store, &database, user_id, next_step.as_str(), next_time).await);
    assert!(!accept_step(&store, &database, user_id, next_step.as_str(), next_time).await);

    let last_step: i64 =
        sqlx::query_scalar("SELECT last_accepted_step FROM mfa_credentials WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(last_step, i64::try_from(current_step(next_time)).unwrap());

    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(user_id.to_string())
        .execute(database.pool())
        .await
        .unwrap();
    let later_time = next_time + TimeDelta::seconds(30);
    let later_code = code_for_step(enrollment.secret_for_display(), current_step(later_time));
    assert!(!accept_step(&store, &database, user_id, later_code.as_str(), later_time).await);
}

#[tokio::test]
async fn concurrent_verifiers_have_exactly_one_winner() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );
    let next_time = now + TimeDelta::seconds(30);
    let next_code = code_for_step(enrollment.secret_for_display(), current_step(next_time));

    let (first, second) = tokio::join!(
        accept_step(&store, &database, user_id, next_code.as_str(), next_time),
        accept_step(&store, &database, user_id, next_code.as_str(), next_time),
    );
    assert_ne!(first, second);
}

#[tokio::test]
async fn audit_failure_rolls_back_totp_replay_counter() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );
    let next_time = now + TimeDelta::seconds(30);
    let next_code = code_for_step(enrollment.secret_for_display(), current_step(next_time));

    sqlx::query(
        "CREATE TRIGGER fail_totp_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.totp_verified' \
         BEGIN SELECT RAISE(ABORT, 'fixture TOTP audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        store
            .verify_in_transaction(&mut transaction, user_id, next_code.as_str(), next_time)
            .await
            .is_err()
    );
    transaction.rollback().await.unwrap();
    sqlx::query("DROP TRIGGER fail_totp_audit")
        .execute(database.pool())
        .await
        .unwrap();

    assert!(accept_step(&store, &database, user_id, next_code.as_str(), next_time).await);
}

#[tokio::test]
async fn migration_rejects_invalid_credential_state_combinations() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let id = Uuid::now_v7().to_string();
    let created_at = "2026-10-10T12:00:00.000000000Z";

    assert!(
        sqlx::query(
            "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
         VALUES (?, ?, zeroblob(65), ?, NULL, NULL, NULL)",
        )
        .bind(&id)
        .bind(user_id.to_string())
        .bind(created_at)
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
         VALUES (?, ?, ?, ?, '2026-10-10T12:10:00.000000000Z', NULL, NULL)",
        )
        .bind(&id)
        .bind(user_id.to_string())
        .bind("x".repeat(65))
        .bind(created_at)
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn concurrent_enrollment_writes_are_serialized_and_replace_pending_secret() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let first_store = store.clone();
    let second_store = store.clone();

    let (first, second) = tokio::join!(
        async move {
            first_store
                .begin_enrollment(user_id, now)
                .await
                .map(|enrollment| enrollment.credential_id())
        },
        async move {
            second_store
                .begin_enrollment(user_id, now)
                .await
                .map(|enrollment| enrollment.credential_id())
        },
    );
    let first_id = first.unwrap();
    let second_id = second.unwrap();
    assert_ne!(first_id, second_id);
    let persisted_id: String =
        sqlx::query_scalar("SELECT id FROM mfa_credentials WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(persisted_id == first_id.to_string() || persisted_id == second_id.to_string());
}

#[tokio::test]
async fn enrollment_and_activation_audit_failures_roll_back_credential_changes() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");

    sqlx::query(
        "CREATE TRIGGER fail_totp_enrollment_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.totp_enrollment_started' \
         BEGIN SELECT RAISE(ABORT, 'fixture TOTP enrollment audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(store.begin_enrollment(user_id, now).await.is_err());
    sqlx::query("DROP TRIGGER fail_totp_enrollment_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let pending_count: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_credentials")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(pending_count, 0);

    let enrollment = store.begin_enrollment(user_id, now).await.unwrap();
    let code = code_for_step(enrollment.secret_for_display(), current_step(now));
    sqlx::query(
        "CREATE TRIGGER fail_totp_confirmation_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.totp_enrollment_confirmed' \
         BEGIN SELECT RAISE(ABORT, 'fixture TOTP confirmation audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        store
            .confirm_enrollment(user_id, code.as_str(), now)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_totp_confirmation_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let pending_state = sqlx::query(
        "SELECT expires_at, activated_at, last_accepted_step \
         FROM mfa_credentials WHERE user_id = ?",
    )
    .bind(user_id.to_string())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert!(
        pending_state
            .try_get::<Option<String>, _>("expires_at")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        pending_state
            .try_get::<Option<String>, _>("activated_at")
            .unwrap(),
        None
    );
    assert_eq!(
        pending_state
            .try_get::<Option<i64>, _>("last_accepted_step")
            .unwrap(),
        None
    );
    assert!(
        store
            .confirm_enrollment(user_id, code.as_str(), now)
            .await
            .unwrap()
    );
}
