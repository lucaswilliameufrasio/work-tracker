use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use puxiri::{
    auth::{
        PasswordService,
        authentication::{PasswordAuthentication, PasswordAuthenticator},
        credentials::CredentialStore,
        master_key::MasterKeyStore,
        mfa::{
            TotpSecretBox,
            login::{MfaLoginAuthentication, MfaLoginService, MfaLoginToken},
            store::TotpCredentialStore,
        },
        rate_limit::{AuthRateLimitPolicies, AuthRateLimitPolicy, AuthRateLimiter},
        session::{SessionPolicy, SessionStore},
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
const EMAIL: &str = "mfa-login@example.test";
const PASSWORD: &str = "mfa login integration password";
const CLIENT_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 93));
const LOGIN_TOKEN_DOMAIN: &[u8] = b"puxiri mfa login challenge v1\0";

struct Fixture {
    _directory: TempDir,
    database: Database,
    authenticator: PasswordAuthenticator,
    user_id: String,
    password: String,
    now: DateTime<Utc>,
    totp_secret: Option<Zeroizing<String>>,
}

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

async fn fixture(installation_admin: bool, with_totp: bool, rate_threshold: u32) -> Fixture {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = Uuid::now_v7().to_string();
    let email_id = Uuid::now_v7().to_string();
    let now = at("2026-10-13T12:00:00Z");
    let now_text = now.to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'MFA login test', 'active', ?, ?)",
    )
    .bind(&user_id)
    .bind(&now_text)
    .bind(&now_text)
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
    .bind(&now_text)
    .bind(&now_text)
    .execute(database.pool())
    .await
    .unwrap();
    if installation_admin {
        sqlx::query(
            "INSERT INTO organizations \
             (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'MFA login test', 'en', 'UTC', ?, ?)",
        )
        .bind(&now_text)
        .bind(&now_text)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
             VALUES (1, ?, 'admin', ?)",
        )
        .bind(&user_id)
        .bind(&now_text)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, ?)")
            .bind(&user_id)
            .bind(&now_text)
            .execute(database.pool())
            .await
            .unwrap();
    }

    let password = String::from(PASSWORD);
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(password.clone()).await.unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, ?)",
    )
    .bind(&user_id)
    .bind(&password_hash)
    .bind(&now_text)
    .execute(database.pool())
    .await
    .unwrap();

    let master_key = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
    let rate_policy = AuthRateLimitPolicy::new(
        TimeDelta::minutes(10),
        rate_threshold,
        TimeDelta::seconds(5),
        TimeDelta::seconds(20),
    )
    .unwrap();
    let rate_limiter = AuthRateLimiter::open(
        database.pool().clone(),
        master_key.clone(),
        AuthRateLimitPolicies::new(rate_policy, rate_policy, rate_policy),
    )
    .await
    .unwrap();
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let totp_credentials =
        TotpCredentialStore::new(database.pool().clone(), TotpSecretBox::new(master_key));
    let totp_secret = if with_totp {
        let enrollment = totp_credentials
            .begin_enrollment(Uuid::parse_str(&user_id).unwrap(), now)
            .await
            .unwrap();
        let code = code_for_secret(enrollment.secret_for_display(), now);
        assert!(
            totp_credentials
                .confirm_enrollment(Uuid::parse_str(&user_id).unwrap(), &code, now)
                .await
                .unwrap()
        );
        Some(Zeroizing::new(enrollment.secret_for_display().to_owned()))
    } else {
        None
    };
    let mfa_login = MfaLoginService::new(
        database.pool().clone(),
        sessions,
        totp_credentials,
        rate_limiter.clone(),
    );
    let authenticator = PasswordAuthenticator::new(
        CredentialStore::new(database.pool().clone(), password_service),
        rate_limiter,
        mfa_login,
        password_hash,
    );

    Fixture {
        _directory: directory,
        database,
        authenticator,
        user_id,
        password,
        now,
        totp_secret,
    }
}

fn code_for_secret(secret: &str, now: DateTime<Utc>) -> String {
    let step = u64::try_from(now.timestamp()).unwrap() / 30;
    let decoded = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
    let mut mac = Hmac::<Sha1>::new_from_slice(&decoded).unwrap();
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    format!("{:06}", binary % 1_000_000)
}

fn wrong_code(code: &str) -> String {
    let mut wrong = code.to_owned();
    let last = wrong.pop().unwrap();
    wrong.push(if last == '0' { '1' } else { '0' });
    wrong
}

async fn begin_login(fixture: &Fixture) -> PasswordAuthentication {
    fixture
        .authenticator
        .authenticate(EMAIL, fixture.password.clone(), CLIENT_IP, fixture.now)
        .await
        .unwrap()
}

fn challenge_from(outcome: PasswordAuthentication) -> (MfaLoginToken, DateTime<Utc>) {
    let PasswordAuthentication::MfaRequired(challenge) = outcome else {
        panic!("expected an MFA login challenge");
    };
    let token = MfaLoginToken::parse(challenge.token().expose()).unwrap();
    (token, challenge.expires_at())
}

#[tokio::test]
async fn active_totp_password_login_returns_digest_only_challenge_without_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let outcome = begin_login(&fixture).await;
    let (token, expires_at) = challenge_from(outcome);
    assert_eq!(expires_at, fixture.now + TimeDelta::minutes(10));

    let expected_hash = {
        let mut digest = Sha256::new();
        digest.update(LOGIN_TOKEN_DOMAIN);
        digest.update(token.expose().as_bytes());
        digest.finalize().to_vec()
    };
    let (stored_hash, stored_user_id, stored_email_key, fingerprint): (
        Vec<u8>,
        String,
        String,
        Vec<u8>,
    ) = sqlx::query_as(
        "SELECT token_hash, user_id, email_key, credential_fingerprint \
         FROM mfa_login_challenges",
    )
    .fetch_one(fixture.database.pool())
    .await
    .unwrap();
    assert_eq!(stored_hash, expected_hash);
    assert_eq!(stored_hash.len(), 32);
    assert_eq!(stored_user_id, fixture.user_id);
    assert_eq!(stored_email_key, EMAIL);
    assert_eq!(fingerprint.len(), 32);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert!(MfaLoginToken::parse("not-a-challenge").is_err());
}

#[tokio::test]
async fn valid_totp_consumes_challenge_and_issues_mfa_assured_session_once() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let failed_password = fixture
        .authenticator
        .authenticate(
            EMAIL,
            String::from("incorrect password for the account"),
            CLIENT_IP,
            fixture.now,
        )
        .await
        .unwrap();
    assert!(matches!(
        failed_password,
        PasswordAuthentication::Rejected { .. }
    ));
    let (token, _) = challenge_from(begin_login(&fixture).await);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM auth_rate_limit_buckets")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        3,
        "password success must not clear failures before MFA succeeds"
    );
    let secret = fixture.totp_secret.as_ref().unwrap();
    let authenticated_at = fixture.now + TimeDelta::seconds(30);
    let code = code_for_secret(secret, authenticated_at);

    let outcome = fixture
        .authenticator
        .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at)
        .await
        .unwrap();
    let MfaLoginAuthentication::Authenticated(issued) = outcome else {
        panic!("expected an authenticated session after TOTP");
    };
    let persisted: (String, Option<String>) =
        sqlx::query_as("SELECT user_id, mfa_authenticated_at FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(fixture.database.pool())
            .await
            .unwrap();
    assert_eq!(persisted.0, fixture.user_id);
    assert_eq!(
        persisted.1,
        Some(authenticated_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT last_accepted_step FROM mfa_credentials WHERE user_id = ?"
        )
        .bind(&fixture.user_id)
        .fetch_one(fixture.database.pool())
        .await
        .unwrap(),
        authenticated_at.timestamp() / 30
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM auth_rate_limit_buckets")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        2,
        "successful MFA clears only the identity bucket"
    );

    let replay = fixture
        .authenticator
        .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at)
        .await
        .unwrap();
    assert!(matches!(replay, MfaLoginAuthentication::Rejected { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn invalid_totp_preserves_challenge_and_rate_limits_subsequent_attempts() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 2).await;
    let (token, _) = challenge_from(begin_login(&fixture).await);
    let secret = fixture.totp_secret.as_ref().unwrap();
    let attempt_at = fixture.now + TimeDelta::seconds(30);
    let invalid = wrong_code(&code_for_secret(secret, attempt_at));

    let first = fixture
        .authenticator
        .complete_mfa_login(&token, &invalid, CLIENT_IP, attempt_at)
        .await
        .unwrap();
    assert!(matches!(
        first,
        MfaLoginAuthentication::Rejected {
            blocked_until: None
        }
    ));
    let second = fixture
        .authenticator
        .complete_mfa_login(&token, &invalid, CLIENT_IP, attempt_at)
        .await
        .unwrap();
    assert!(matches!(
        second,
        MfaLoginAuthentication::Rejected {
            blocked_until: Some(_)
        }
    ));
    let third = fixture
        .authenticator
        .complete_mfa_login(&token, &invalid, CLIENT_IP, attempt_at)
        .await
        .unwrap();
    assert!(matches!(
        third,
        MfaLoginAuthentication::Rejected {
            blocked_until: Some(_)
        }
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT last_accepted_step FROM mfa_credentials WHERE user_id = ?"
        )
        .bind(&fixture.user_id)
        .fetch_one(fixture.database.pool())
        .await
        .unwrap(),
        fixture.now.timestamp() / 30
    );
}

#[tokio::test]
async fn installation_admin_without_active_totp_never_gets_password_only_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, false, 5).await;

    assert!(matches!(
        begin_login(&fixture).await,
        PasswordAuthentication::MfaSetupRequired
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn installation_admin_with_active_totp_must_complete_challenge_before_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(true, true, 5).await;

    assert!(matches!(
        begin_login(&fixture).await,
        PasswordAuthentication::MfaRequired(_)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn totp_audit_failure_rolls_back_challenge_session_and_replay_counter() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let (token, _) = challenge_from(begin_login(&fixture).await);
    let secret = fixture.totp_secret.as_ref().unwrap();
    let authenticated_at = fixture.now + TimeDelta::seconds(30);
    let code = code_for_secret(secret, authenticated_at);
    sqlx::query(
        "CREATE TRIGGER fail_mfa_login_totp_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.mfa.totp_verified' \
         BEGIN SELECT RAISE(ABORT, 'fixture MFA login audit failure'); END",
    )
    .execute(fixture.database.pool())
    .await
    .unwrap();

    assert!(
        fixture
            .authenticator
            .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT last_accepted_step FROM mfa_credentials WHERE user_id = ?"
        )
        .bind(&fixture.user_id)
        .fetch_one(fixture.database.pool())
        .await
        .unwrap(),
        fixture.now.timestamp() / 30
    );

    sqlx::query("DROP TRIGGER fail_mfa_login_totp_audit")
        .execute(fixture.database.pool())
        .await
        .unwrap();
    let retry = fixture
        .authenticator
        .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at)
        .await
        .unwrap();
    assert!(matches!(retry, MfaLoginAuthentication::Authenticated(_)));
}

#[tokio::test]
async fn password_change_invalidates_an_outstanding_mfa_login_challenge() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let (token, _) = challenge_from(begin_login(&fixture).await);
    let replacement_password = String::from("a replacement password for login tests");
    let replacement_hash = PasswordService::default()
        .hash(replacement_password)
        .await
        .unwrap();
    sqlx::query("UPDATE password_credentials SET password_hash = ? WHERE user_id = ?")
        .bind(replacement_hash)
        .bind(&fixture.user_id)
        .execute(fixture.database.pool())
        .await
        .unwrap();

    let secret = fixture.totp_secret.as_ref().unwrap();
    let attempt_at = fixture.now + TimeDelta::seconds(30);
    let outcome = fixture
        .authenticator
        .complete_mfa_login(
            &token,
            &code_for_secret(secret, attempt_at),
            CLIENT_IP,
            attempt_at,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, MfaLoginAuthentication::Rejected { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn concurrent_completion_creates_exactly_one_mfa_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let (token, _) = challenge_from(begin_login(&fixture).await);
    let secret = fixture.totp_secret.as_ref().unwrap();
    let authenticated_at = fixture.now + TimeDelta::seconds(30);
    let code = code_for_secret(secret, authenticated_at);
    let (first, second) = tokio::join!(
        fixture
            .authenticator
            .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at),
        fixture
            .authenticator
            .complete_mfa_login(&token, &code, CLIENT_IP, authenticated_at),
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MfaLoginAuthentication::Authenticated(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MfaLoginAuthentication::Rejected { .. }))
            .count(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn expired_challenge_is_rejected_at_the_exact_expiry_boundary() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let fixture = fixture(false, true, 5).await;
    let (token, expires_at) = challenge_from(begin_login(&fixture).await);
    let secret = fixture.totp_secret.as_ref().unwrap();

    let outcome = fixture
        .authenticator
        .complete_mfa_login(
            &token,
            &code_for_secret(secret, expires_at),
            CLIENT_IP,
            expires_at,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, MfaLoginAuthentication::Rejected { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mfa_login_challenges")
            .fetch_one(fixture.database.pool())
            .await
            .unwrap(),
        0
    );
}
