use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use puxiri::{
    auth::{
        PasswordService,
        credentials::CredentialStore,
        master_key::MasterKeyStore,
        mfa::{
            TotpSecretBox,
            recovery::{
                MfaRecoveryAccess, MfaRecoveryAuthentication, MfaRecoveryService, MfaRecoveryToken,
            },
            store::TotpCredentialStore,
        },
        rate_limit::{AuthRateLimitPolicies, AuthRateLimitPolicy, AuthRateLimiter},
        recovery::{RecoveryCodePolicy, RecoveryCodeStore},
    },
    db::Database,
};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

static KDF_TEST_LOCK: Mutex<()> = Mutex::const_new(());
const EMAIL: &str = "recovery@example.test";
const PASSWORD: &str = "recovery test password with enough length";
const CLIENT_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 81));

struct Fixture {
    _directory: TempDir,
    database: Database,
    service: MfaRecoveryService,
    user_id: String,
    password: String,
    first_code: Zeroizing<String>,
    second_code: Zeroizing<String>,
    now: DateTime<Utc>,
}

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn totp_code(secret: &str, step: u64) -> String {
    let decoded = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
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
    format!("{:06}", binary % 1_000_000)
}

async fn fixture(with_totp: bool, installation_admin: bool) -> Fixture {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = Uuid::now_v7().to_string();
    let email_id = Uuid::now_v7().to_string();
    let now = at("2026-10-13T12:00:00Z");
    let now_text = "2026-10-13T12:00:00.000000000Z";
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'MFA recovery test', 'active', ?, ?)",
    )
    .bind(&user_id)
    .bind(now_text)
    .bind(now_text)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(email_id)
    .bind(&user_id)
    .bind(EMAIL)
    .bind(EMAIL)
    .bind(now_text)
    .bind(now_text)
    .execute(database.pool())
    .await
    .unwrap();
    if installation_admin {
        sqlx::query(
            "INSERT INTO organizations \
             (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'MFA recovery test', 'en', 'UTC', ?, ?)",
        )
        .bind(now_text)
        .bind(now_text)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO organization_members \
             (organization_id, user_id, role, created_at) VALUES (1, ?, 'admin', ?)",
        )
        .bind(&user_id)
        .bind(now_text)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, ?)")
            .bind(&user_id)
            .bind(now_text)
            .execute(database.pool())
            .await
            .unwrap();
    }

    let password_service = PasswordService::default();
    let password_hash = password_service.hash(PASSWORD.to_owned()).await.unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, ?)",
    )
    .bind(&user_id)
    .bind(&password_hash)
    .bind(now_text)
    .execute(database.pool())
    .await
    .unwrap();

    let master_key = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
    let credential_store = CredentialStore::new(database.pool().clone(), password_service);
    let totp_store = TotpCredentialStore::new(
        database.pool().clone(),
        TotpSecretBox::new(master_key.clone()),
    );
    if with_totp {
        let enrollment = totp_store
            .begin_enrollment(Uuid::parse_str(&user_id).unwrap(), now)
            .await
            .unwrap();
        let step = u64::try_from(now.timestamp()).unwrap() / 30;
        let code = totp_code(enrollment.secret_for_display(), step);
        assert!(
            totp_store
                .confirm_enrollment(Uuid::parse_str(&user_id).unwrap(), &code, now)
                .await
                .unwrap()
        );
    }

    let recovery_codes = RecoveryCodeStore::new(database.pool().clone());
    let batch = recovery_codes
        .issue(&user_id, RecoveryCodePolicy::new(2).unwrap(), now)
        .await
        .unwrap();
    let mut codes = batch.codes();
    let first_code = Zeroizing::new(codes.next().unwrap().to_owned());
    let second_code = Zeroizing::new(codes.next().unwrap().to_owned());

    let rate_policy = AuthRateLimitPolicy::new(
        TimeDelta::minutes(10),
        20,
        TimeDelta::seconds(1),
        TimeDelta::minutes(1),
    )
    .unwrap();
    let secret_box = TotpSecretBox::new(master_key.clone());
    let rate_limiter = AuthRateLimiter::open(
        database.pool().clone(),
        master_key,
        AuthRateLimitPolicies::new(rate_policy, rate_policy, rate_policy),
    )
    .await
    .unwrap();
    let service = MfaRecoveryService::new(
        database.pool().clone(),
        credential_store,
        recovery_codes.clone(),
        rate_limiter,
        password_hash,
        secret_box,
    );

    Fixture {
        _directory: directory,
        database,
        service,
        user_id,
        password: PASSWORD.to_owned(),
        first_code,
        second_code,
        now,
    }
}

async fn start_recovery(fixture: &Fixture, code: &str, now: DateTime<Utc>) -> MfaRecoveryAccess {
    let outcome = fixture
        .service
        .start(EMAIL, fixture.password.clone(), code, CLIENT_IP, now)
        .await
        .unwrap();
    let MfaRecoveryAuthentication::Started(access) = outcome else {
        panic!("expected restricted recovery access");
    };
    access
}

fn rejected(outcome: MfaRecoveryAuthentication) -> bool {
    matches!(outcome, MfaRecoveryAuthentication::Rejected { .. })
}

async fn recovery_code_is_unused(fixture: &Fixture, code: &str) -> bool {
    let digest = Sha256::digest(code.as_bytes());
    let consumed_at: Option<String> = sqlx::query_scalar(
        "SELECT consumed_at FROM recovery_codes WHERE user_id = ? AND code_hash = ?",
    )
    .bind(&fixture.user_id)
    .bind(digest.as_slice())
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    consumed_at.is_none()
}

#[tokio::test]
async fn password_and_recovery_code_create_only_a_digest_backed_restricted_token() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let outcome = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now,
        )
        .await
        .unwrap();
    let MfaRecoveryAuthentication::Started(access) = outcome else {
        panic!("expected restricted recovery access");
    };

    assert_eq!(access.expires_at(), fixture.now + TimeDelta::minutes(10));
    assert_eq!(access.token().expose().len(), 43);
    let stored_token_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM mfa_recovery_sessions WHERE id = ?")
            .bind(access.id())
            .fetch_one(fixture.database.pool())
            .await
            .unwrap();
    let mut expected_hash = Sha256::new();
    expected_hash.update(b"puxiri mfa recovery token v1\0");
    expected_hash.update(access.token().expose().as_bytes());
    let expected_hash: [u8; 32] = expected_hash.finalize().into();
    assert_eq!(stored_token_hash.as_slice(), expected_hash.as_slice());
    assert_ne!(
        stored_token_hash.as_slice(),
        access.token().expose().as_bytes()
    );

    let normal_session_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(normal_session_count, 0);
    let active_totp_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mfa_credentials WHERE user_id = ? AND activated_at IS NOT NULL",
    )
    .bind(&fixture.user_id)
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(active_totp_count, 1);
    let audit_event: (String, String) = sqlx::query_as(
        "SELECT event_kind, target_kind FROM audit_events \
         WHERE target_id = ? AND event_kind = 'auth.mfa.recovery_started'",
    )
    .bind(access.id())
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(
        audit_event,
        (
            "auth.mfa.recovery_started".to_owned(),
            "mfa_recovery_session".to_owned()
        )
    );

    assert!(
        fixture
            .service
            .validate(
                access.token(),
                fixture.now + TimeDelta::minutes(9) + TimeDelta::seconds(59)
            )
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .service
            .validate(access.token(), access.expires_at())
            .await
            .unwrap()
    );
    let parsed = MfaRecoveryToken::parse(access.token().expose()).unwrap();
    assert!(
        !fixture
            .service
            .validate(&parsed, access.expires_at())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn password_or_recovery_code_failure_is_generic_and_does_not_consume_a_code() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let wrong_password = fixture
        .service
        .start(
            EMAIL,
            String::from("incorrect but sufficiently long password"),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now,
        )
        .await
        .unwrap();
    assert!(rejected(wrong_password));
    assert!(recovery_code_is_unused(&fixture, fixture.first_code.as_str()).await);

    let wrong_code = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            "AAAAAAAAAAAAAAAAAAAAAA",
            CLIENT_IP,
            fixture.now + TimeDelta::seconds(1),
        )
        .await
        .unwrap();
    assert!(rejected(wrong_code));
    assert!(recovery_code_is_unused(&fixture, fixture.first_code.as_str()).await);

    let valid = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now + TimeDelta::seconds(2),
        )
        .await
        .unwrap();
    assert!(matches!(valid, MfaRecoveryAuthentication::Started(_)));
}

#[tokio::test]
async fn installation_admins_and_users_without_active_totp_cannot_start_public_recovery() {
    let _guard = KDF_TEST_LOCK.lock().await;
    for (with_totp, is_admin) in [(true, true), (false, false)] {
        let fixture = fixture(with_totp, is_admin).await;
        let outcome = fixture
            .service
            .start(
                EMAIL,
                fixture.password.clone(),
                fixture.first_code.as_str(),
                CLIENT_IP,
                fixture.now,
            )
            .await
            .unwrap();
        assert!(rejected(outcome));
        assert!(recovery_code_is_unused(&fixture, fixture.first_code.as_str()).await);
        let open_recovery_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mfa_recovery_sessions")
                .fetch_one(fixture.database.pool())
                .await
                .unwrap();
        assert_eq!(open_recovery_count, 0);
    }
}

#[tokio::test]
async fn concurrent_recovery_starts_create_at_most_one_live_restricted_token() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let (first, second) = tokio::join!(
        fixture.service.start(
            EMAIL,
            fixture.password.clone(),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now,
        ),
        fixture.service.start(
            EMAIL,
            fixture.password.clone(),
            fixture.second_code.as_str(),
            CLIENT_IP,
            fixture.now,
        ),
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MfaRecoveryAuthentication::Started(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MfaRecoveryAuthentication::Rejected { .. }))
            .count(),
        1
    );
    let recovery_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mfa_recovery_sessions \
         WHERE user_id = ? AND completed_at IS NULL AND invalidated_at IS NULL",
    )
    .bind(&fixture.user_id)
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(recovery_count, 1);
    let first_code_unused = recovery_code_is_unused(&fixture, fixture.first_code.as_str()).await;
    let second_code_unused = recovery_code_is_unused(&fixture, fixture.second_code.as_str()).await;
    assert_ne!(first_code_unused, second_code_unused);
}

#[tokio::test]
async fn a_new_recovery_flow_can_start_after_the_prior_token_expires() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let first = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now,
        )
        .await
        .unwrap();
    let MfaRecoveryAuthentication::Started(first_access) = first else {
        panic!("expected first recovery flow");
    };
    let second = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            fixture.second_code.as_str(),
            CLIENT_IP,
            first_access.expires_at(),
        )
        .await
        .unwrap();
    let MfaRecoveryAuthentication::Started(second_access) = second else {
        panic!("expected a new flow after expiry");
    };

    assert!(
        !fixture
            .service
            .validate(first_access.token(), first_access.expires_at())
            .await
            .unwrap()
    );
    assert!(
        fixture
            .service
            .validate(
                second_access.token(),
                second_access.expires_at() - TimeDelta::seconds(1)
            )
            .await
            .unwrap()
    );
    let invalidated_first: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM mfa_recovery_sessions WHERE id = ?")
            .bind(first_access.id())
            .fetch_one(fixture.database.pool())
            .await
            .unwrap();
    assert!(invalidated_first.is_some());
}

#[tokio::test]
async fn recovery_audit_failure_rolls_back_code_consumption_and_token_creation() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    sqlx::query(
        "CREATE TRIGGER fail_mfa_recovery_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.recovery_started' \
         BEGIN SELECT RAISE(ABORT, 'fixture MFA recovery audit failure'); END",
    )
    .execute(fixture.database.pool())
    .await
    .unwrap();
    assert!(
        fixture
            .service
            .start(
                EMAIL,
                fixture.password.clone(),
                fixture.first_code.as_str(),
                CLIENT_IP,
                fixture.now,
            )
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_mfa_recovery_audit")
        .execute(fixture.database.pool())
        .await
        .unwrap();

    let recovery_count: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_recovery_sessions")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(recovery_count, 0);
    assert!(recovery_code_is_unused(&fixture, fixture.first_code.as_str()).await);
    let retry = fixture
        .service
        .start(
            EMAIL,
            fixture.password.clone(),
            fixture.first_code.as_str(),
            CLIENT_IP,
            fixture.now + TimeDelta::seconds(1),
        )
        .await
        .unwrap();
    assert!(matches!(retry, MfaRecoveryAuthentication::Started(_)));
}

#[tokio::test]
async fn recovery_token_starts_one_encrypted_replacement_and_preserves_active_totp() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let access = start_recovery(&fixture, fixture.first_code.as_str(), fixture.now).await;

    let enrollment = fixture
        .service
        .begin_totp_replacement(access.token(), fixture.now + TimeDelta::seconds(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(enrollment.expires_at(), access.expires_at());
    assert_eq!(enrollment.secret_for_display().len(), 32);

    let active_totp_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mfa_credentials \
         WHERE user_id = ? AND activated_at IS NOT NULL",
    )
    .bind(&fixture.user_id)
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(active_totp_count, 1);

    let stored_envelope: Vec<u8> = sqlx::query_scalar(
        "SELECT secret_envelope FROM mfa_recovery_enrollments \
         WHERE recovery_session_id = ?",
    )
    .bind(access.id())
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(stored_envelope.len(), 65);
    assert!(!String::from_utf8_lossy(&stored_envelope).contains(enrollment.secret_for_display()));

    let duplicate = fixture
        .service
        .begin_totp_replacement(access.token(), fixture.now + TimeDelta::seconds(2))
        .await
        .unwrap();
    assert!(duplicate.is_none());
    let enrollment_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mfa_recovery_enrollments \
         WHERE recovery_session_id = ?",
    )
    .bind(access.id())
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(enrollment_count, 1);
    assert!(
        fixture
            .service
            .validate(access.token(), fixture.now + TimeDelta::seconds(2))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn recovery_enrollment_audit_failure_rolls_back_encrypted_secret_persistence() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let access = start_recovery(&fixture, fixture.first_code.as_str(), fixture.now).await;
    sqlx::query(
        "CREATE TRIGGER fail_mfa_recovery_enrollment_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.recovery_enrollment_started' \
         BEGIN SELECT RAISE(ABORT, 'fixture MFA recovery enrollment audit failure'); END",
    )
    .execute(fixture.database.pool())
    .await
    .unwrap();

    assert!(
        fixture
            .service
            .begin_totp_replacement(access.token(), fixture.now + TimeDelta::seconds(1))
            .await
            .is_err()
    );
    let pending_count: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_recovery_enrollments")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(pending_count, 0);

    sqlx::query("DROP TRIGGER fail_mfa_recovery_enrollment_audit")
        .execute(fixture.database.pool())
        .await
        .unwrap();
    assert!(
        fixture
            .service
            .begin_totp_replacement(access.token(), fixture.now + TimeDelta::seconds(2))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn expired_recovery_token_cannot_start_replacement_totp() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false).await;
    let access = start_recovery(&fixture, fixture.first_code.as_str(), fixture.now).await;

    assert!(
        fixture
            .service
            .begin_totp_replacement(access.token(), access.expires_at())
            .await
            .unwrap()
            .is_none()
    );
    let pending_count: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_recovery_enrollments")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(pending_count, 0);
}
