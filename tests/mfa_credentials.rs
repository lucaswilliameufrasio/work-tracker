use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use puxiri::{
    auth::{
        SessionToken,
        email_addresses::EmailAddressStore,
        master_key::MasterKeyStore,
        mfa::{
            TotpSecretBox,
            step_up::MfaStepUpService,
            store::{TotpCredentialError, TotpCredentialStore},
        },
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
};
use sha1::Sha1;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
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
    let expiry_after_wrong_code: String =
        sqlx::query_scalar("SELECT expires_at FROM mfa_credentials WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(expiry_after_wrong_code, "2026-10-10T12:10:00.000000000Z");
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

#[tokio::test]
async fn mfa_step_up_rotates_session_and_allows_admin_email_change_with_recent_proof() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'MFA step-up test', 'en', 'UTC', \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, 'admin', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(user_id.to_string())
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(user_id.to_string())
    .execute(database.pool())
    .await
    .unwrap();

    let secret_store = credential_store(&database, &directory.path().join("master.key"));
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = secret_store.begin_enrollment(user_id, now).await.unwrap();
    let enrollment_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        secret_store
            .confirm_enrollment(user_id, enrollment_code.as_str(), now)
            .await
            .unwrap()
    );
    let original_session = sessions.issue(&user_id.to_string(), now).await.unwrap();
    let original_token = SessionToken::parse(original_session.token.expose()).unwrap();
    let step_up = MfaStepUpService::new(sessions.clone(), secret_store.clone());
    let step_up_time = now + TimeDelta::minutes(2);
    let code = code_for_step(enrollment.secret_for_display(), current_step(step_up_time));

    assert!(
        step_up
            .elevate(&original_token, "not-an-otp", step_up_time)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&original_token, step_up_time)
            .await
            .unwrap()
            .is_some()
    );

    let elevation = step_up
        .elevate(&original_token, code.as_str(), step_up_time)
        .await
        .unwrap()
        .unwrap();
    let rotated_token = SessionToken::parse(elevation.session().token.expose()).unwrap();
    assert_ne!(elevation.session().id, original_session.id);
    assert!(
        sessions
            .validate(&original_token, step_up_time)
            .await
            .unwrap()
            .is_none()
    );
    let authenticated = sessions
        .validate(&rotated_token, step_up_time)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(authenticated.user_id, user_id.to_string());

    let session_rows = sqlx::query(
        "SELECT id, created_at, reauthenticated_at, mfa_authenticated_at, revoked_at \
         FROM sessions ORDER BY created_at, id",
    )
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(session_rows.len(), 2);
    assert_eq!(
        session_rows[0]
            .try_get::<Option<String>, _>("mfa_authenticated_at")
            .unwrap(),
        None
    );
    assert_eq!(
        session_rows[0]
            .try_get::<Option<String>, _>("revoked_at")
            .unwrap(),
        Some("2026-10-10T12:02:00.000000000Z".to_owned())
    );
    assert_eq!(
        session_rows[1].try_get::<String, _>("created_at").unwrap(),
        "2026-10-10T12:00:00.000000000Z"
    );
    assert_eq!(
        session_rows[1]
            .try_get::<Option<String>, _>("reauthenticated_at")
            .unwrap(),
        None
    );
    assert_eq!(
        session_rows[1]
            .try_get::<Option<String>, _>("mfa_authenticated_at")
            .unwrap(),
        Some("2026-10-10T12:02:00.000000000Z".to_owned())
    );

    let email_store = EmailAddressStore::new(database.pool().clone());
    let first_email = email_store
        .add_pending_with_mfa(elevation.proof(), "admin-first@example.test", step_up_time)
        .await
        .unwrap();
    let persisted_proof = step_up
        .recent_proof(&rotated_token, step_up_time + TimeDelta::minutes(1))
        .await
        .unwrap()
        .unwrap();
    let second_email = email_store
        .add_pending_with_mfa(
            &persisted_proof,
            "admin-second@example.test",
            step_up_time + TimeDelta::minutes(1),
        )
        .await
        .unwrap();
    email_store
        .add_pending_with_mfa(
            &persisted_proof,
            "admin-third@example.test",
            step_up_time + TimeDelta::minutes(1),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE user_emails SET confirmed_at = ? WHERE user_id = ?")
        .bind("2026-10-10T12:03:00.000000000Z")
        .bind(user_id.to_string())
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        email_store
            .set_primary_with_mfa(
                &persisted_proof,
                &second_email.id,
                step_up_time + TimeDelta::minutes(1),
            )
            .await
            .unwrap()
    );
    assert!(
        email_store
            .remove_with_mfa(
                &persisted_proof,
                &first_email.id,
                step_up_time + TimeDelta::minutes(1),
            )
            .await
            .unwrap()
    );
    assert!(
        step_up
            .recent_proof(&rotated_token, step_up_time + TimeDelta::minutes(10))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        step_up
            .recent_proof(
                &rotated_token,
                step_up_time + TimeDelta::minutes(10) + TimeDelta::nanoseconds(1),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        step_up
            .elevate(&rotated_token, code.as_str(), step_up_time)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&rotated_token, step_up_time)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions
            .revoke(
                &user_id.to_string(),
                &elevation.session().id,
                step_up_time + TimeDelta::minutes(2),
            )
            .await
            .unwrap()
    );
    assert!(matches!(
        email_store
            .add_pending_with_mfa(
                &persisted_proof,
                "admin-revoked-session@example.test",
                step_up_time + TimeDelta::minutes(2),
            )
            .await,
        Err(puxiri::auth::email_addresses::EmailAddressManagementError::ReauthenticationRequired)
    ));

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE event_kind IN ('auth.mfa.totp_verified', 'auth.session.mfa_elevated')",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 2);
}

#[tokio::test]
async fn mfa_rotation_preserves_the_original_absolute_session_deadline() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let secret_store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = secret_store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        secret_store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );

    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::new(
            TimeDelta::minutes(5),
            TimeDelta::minutes(4),
            TimeDelta::minutes(1),
        )
        .unwrap(),
    );
    let original = sessions.issue(&user_id.to_string(), now).await.unwrap();
    let original_token = SessionToken::parse(original.token.expose()).unwrap();
    let elevate_at = now + TimeDelta::minutes(2);
    let code = code_for_step(enrollment.secret_for_display(), current_step(elevate_at));
    let elevation = MfaStepUpService::new(sessions, secret_store)
        .elevate(&original_token, code.as_str(), elevate_at)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(original.expires_at, now + TimeDelta::minutes(5));
    assert_eq!(elevation.session().expires_at, original.expires_at);
}

#[tokio::test]
async fn concurrent_mfa_elevation_rotates_the_exact_session_only_once() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let secret_store = credential_store(&database, &directory.path().join("master.key"));
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = secret_store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        secret_store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );
    let original = sessions.issue(&user_id.to_string(), now).await.unwrap();
    let original_token = SessionToken::parse(original.token.expose()).unwrap();
    let step_up = MfaStepUpService::new(sessions.clone(), secret_store);
    let step_up_time = now + TimeDelta::minutes(1);
    let code = code_for_step(enrollment.secret_for_display(), current_step(step_up_time));

    let (first, second) = tokio::join!(
        step_up.elevate(&original_token, code.as_str(), step_up_time),
        step_up.elevate(&original_token, code.as_str(), step_up_time),
    );
    let first_succeeded = first.unwrap().is_some();
    let second_succeeded = second.unwrap().is_some();
    assert_ne!(first_succeeded, second_succeeded);
    assert!(
        sessions
            .validate(&original_token, step_up_time)
            .await
            .unwrap()
            .is_none()
    );
    let live_sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE revoked_at IS NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(live_sessions, 1);
}

#[tokio::test]
async fn disabling_optional_totp_invalidates_existing_mfa_proofs_without_revoking_login_session() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let secret_store = credential_store(&database, &directory.path().join("master.key"));
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = secret_store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        secret_store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );
    let original = sessions.issue(&user_id.to_string(), now).await.unwrap();
    let original_token = SessionToken::parse(original.token.expose()).unwrap();
    let step_up_time = now + TimeDelta::seconds(30);
    let code = code_for_step(enrollment.secret_for_display(), current_step(step_up_time));
    let step_up = MfaStepUpService::new(sessions.clone(), secret_store.clone());
    let elevation = step_up
        .elevate(&original_token, code.as_str(), step_up_time)
        .await
        .unwrap()
        .unwrap();
    let rotated_token = SessionToken::parse(elevation.session().token.expose()).unwrap();

    assert!(
        secret_store
            .disable_with_mfa_proof(elevation.proof(), step_up_time + TimeDelta::seconds(1))
            .await
            .unwrap()
    );
    assert!(
        step_up
            .recent_proof(&rotated_token, step_up_time + TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .validate(&rotated_token, step_up_time + TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_some()
    );

    let stale_proof_result = EmailAddressStore::new(database.pool().clone())
        .add_pending_with_mfa(
            elevation.proof(),
            "disabled-factor@example.test",
            step_up_time + TimeDelta::seconds(1),
        )
        .await;
    assert!(matches!(
        stale_proof_result,
        Err(puxiri::auth::email_addresses::EmailAddressManagementError::ReauthenticationRequired)
    ));
}

#[tokio::test]
async fn session_elevation_audit_failure_rolls_back_rotation_and_totp_replay_state() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let secret_store = credential_store(&database, &directory.path().join("master.key"));
    let now = at("2026-10-10T12:00:00Z");
    let enrollment = secret_store.begin_enrollment(user_id, now).await.unwrap();
    let activation_code = code_for_step(enrollment.secret_for_display(), current_step(now));
    assert!(
        secret_store
            .confirm_enrollment(user_id, activation_code.as_str(), now)
            .await
            .unwrap()
    );
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let original = sessions.issue(&user_id.to_string(), now).await.unwrap();
    let original_token = SessionToken::parse(original.token.expose()).unwrap();
    let step_up_time = now + TimeDelta::minutes(1);
    let code = code_for_step(enrollment.secret_for_display(), current_step(step_up_time));
    let service = MfaStepUpService::new(sessions.clone(), secret_store.clone());

    sqlx::query(
        "CREATE TRIGGER fail_mfa_session_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.session.mfa_elevated' \
         BEGIN SELECT RAISE(ABORT, 'fixture MFA session audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        service
            .elevate(&original_token, code.as_str(), step_up_time)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_mfa_session_audit")
        .execute(database.pool())
        .await
        .unwrap();

    assert!(
        sessions
            .validate(&original_token, step_up_time)
            .await
            .unwrap()
            .is_some()
    );
    let session_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(session_count, 1);
    let last_step: i64 =
        sqlx::query_scalar("SELECT last_accepted_step FROM mfa_credentials WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(last_step, i64::try_from(current_step(now)).unwrap());

    assert!(
        service
            .elevate(&original_token, code.as_str(), step_up_time)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn mfa_migrations_preserve_audit_history_and_reinstate_immutability() {
    let directory = TempDir::new().unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.path().join("migration-upgrade.sqlite3"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let actor_id = Uuid::now_v7().to_string();
    let target_id = Uuid::now_v7().to_string();
    let event_id = Uuid::now_v7().to_string();

    sqlx::raw_sql(
        "CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL); \
         CREATE TABLE sessions ( \
             id TEXT PRIMARY KEY NOT NULL, token_hash BLOB NOT NULL, \
             user_id TEXT NOT NULL, created_at TEXT NOT NULL, \
             last_activity_at TEXT NOT NULL, expires_at TEXT NOT NULL, \
             reauthenticated_at TEXT, revoked_at TEXT \
         ); \
         CREATE TABLE audit_events ( \
             id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36), \
             event_kind TEXT NOT NULL CHECK (event_kind IN ( \
                 'auth.email.address_added', \
                 'auth.email.address_confirmed', \
                 'auth.email.primary_changed', \
                 'auth.email.address_removed', \
                 'auth.password.reset_completed', \
                 'auth.password.changed', \
                 'auth.session.revoked', \
                 'auth.sessions.revoked', \
                 'project.created' \
             )), \
             result TEXT NOT NULL CHECK (result IN ('success', 'failure')), \
             actor_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT, \
             target_kind TEXT NOT NULL CHECK ( \
                 target_kind IN ('user', 'email_address', 'session', 'project') \
             ), \
             target_id TEXT NOT NULL CHECK (length(target_id) = 36), \
             occurred_at TEXT NOT NULL \
         ); \
         CREATE INDEX audit_events_occurred_idx \
             ON audit_events (occurred_at DESC, id DESC); \
         CREATE INDEX audit_events_actor_idx \
             ON audit_events (actor_user_id, occurred_at DESC, id DESC); \
         CREATE TRIGGER audit_events_no_update BEFORE UPDATE ON audit_events \
             BEGIN SELECT RAISE(ABORT, 'audit events are immutable'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id) VALUES (?)")
        .bind(&actor_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.email.address_added', 'success', ?, 'email_address', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
    )
    .bind(&event_id)
    .bind(&actor_id)
    .bind(&target_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../migrations/0016_totp_credentials.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0017_session_mfa_assurance.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0018_totp_disable_audit.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let preserved = sqlx::query(
        "SELECT event_kind, actor_user_id, target_kind, target_id \
         FROM audit_events WHERE id = ?",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.mfa.totp_disabled', 'success', ?, 'mfa_credential', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&actor_id)
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        preserved.try_get::<String, _>("event_kind").unwrap(),
        "auth.email.address_added"
    );
    assert_eq!(
        preserved.try_get::<String, _>("actor_user_id").unwrap(),
        actor_id
    );
    assert_eq!(
        preserved.try_get::<String, _>("target_kind").unwrap(),
        "email_address"
    );
    assert_eq!(
        preserved.try_get::<String, _>("target_id").unwrap(),
        target_id
    );

    let session_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions \
         (id, token_hash, user_id, created_at, last_activity_at, expires_at, \
          reauthenticated_at, revoked_at) \
         VALUES (?, zeroblob(32), ?, '2026-10-10T12:00:00Z', \
                 '2026-10-10T12:00:00Z', '2026-10-17T12:00:00Z', NULL, NULL)",
    )
    .bind(&session_id)
    .bind(&actor_id)
    .execute(&pool)
    .await
    .unwrap();
    let marker: Option<String> =
        sqlx::query_scalar("SELECT mfa_authenticated_at FROM sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marker, None);

    sqlx::query(
        "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'auth.session.mfa_elevated', 'success', ?, 'session', ?, \
                 '2026-10-10T12:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&actor_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE audit_events SET result = 'failure' WHERE id = ?")
            .bind(event_id)
            .execute(&pool)
            .await
            .is_err()
    );

    pool.close().await;
}
