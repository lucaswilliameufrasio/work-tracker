use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService,
        authentication::{PasswordAuthentication, PasswordAuthenticator},
        credentials::CredentialStore,
        master_key::MasterKeyStore,
        rate_limit::{AuthRateLimitPolicies, AuthRateLimitPolicy, AuthRateLimiter},
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

static KDF_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

fn policy(threshold: u32) -> AuthRateLimitPolicy {
    AuthRateLimitPolicy::new(
        TimeDelta::minutes(10),
        threshold,
        TimeDelta::seconds(5),
        TimeDelta::seconds(20),
    )
    .unwrap()
}

async fn create_user(database: &Database, state: &str, email: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Password login test', ?, \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    let confirmed_at = if state == "pending_email" {
        None
    } else {
        Some("2026-10-10T00:00:00.000000000Z")
    };
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 0, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(email_id)
    .bind(&user_id)
    .bind(email)
    .bind(email.to_ascii_lowercase())
    .bind(confirmed_at)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn authenticator(
    database: &Database,
    directory: &TempDir,
    password_service: PasswordService,
    dummy_hash: &str,
    threshold: u32,
) -> PasswordAuthenticator {
    let master_key = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
    let scope_policy = policy(threshold);
    let limiter = AuthRateLimiter::open(
        database.pool().clone(),
        master_key,
        AuthRateLimitPolicies::new(scope_policy, scope_policy, scope_policy),
    )
    .await
    .unwrap();
    PasswordAuthenticator::new(
        CredentialStore::new(database.pool().clone(), password_service),
        SessionStore::new(
            database.pool().clone(),
            SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
        ),
        limiter,
        dummy_hash.to_owned(),
    )
}

#[tokio::test]
async fn confirmed_active_email_with_correct_password_issues_a_valid_opaque_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active", "alice@example.test").await;
    let password = String::from("a sufficiently long password");
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(password.clone()).await.unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(&password_hash)
    .execute(database.pool())
    .await
    .unwrap();
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let authenticator =
        authenticator(&database, &directory, password_service, &password_hash, 5).await;
    let now = at("2026-10-10T12:00:00Z");

    let outcome = authenticator
        .authenticate(
            "  ALICE@EXAMPLE.TEST ",
            password,
            "192.0.2.17".parse().unwrap(),
            now,
        )
        .await
        .unwrap();
    let PasswordAuthentication::Authenticated(issued) = outcome else {
        panic!("expected a successful authentication");
    };
    let persisted_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM sessions WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(persisted_hash.as_slice(), issued.token.digest());
    assert_eq!(
        sessions
            .validate(&issued.token, now)
            .await
            .unwrap()
            .unwrap()
            .user_id,
        user_id
    );
}

#[tokio::test]
async fn wrong_unknown_unconfirmed_and_malformed_credentials_share_rejection_shape() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let active_user = create_user(&database, "active", "known@example.test").await;
    create_user(&database, "active", "pending@example.test").await;
    sqlx::query(
        "UPDATE user_emails SET confirmed_at = NULL WHERE address = 'pending@example.test'",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let password_service = PasswordService::default();
    let correct_password = String::from("another sufficiently long password");
    let password_hash = password_service
        .hash(correct_password.clone())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(active_user)
    .bind(&password_hash)
    .execute(database.pool())
    .await
    .unwrap();
    let authenticator =
        authenticator(&database, &directory, password_service, &password_hash, 10).await;
    let ip: IpAddr = "198.51.100.45".parse().unwrap();

    for (email, password) in [
        ("known@example.test", String::from("incorrect password")),
        ("missing@example.test", correct_password.clone()),
        ("pending@example.test", correct_password.clone()),
        ("not an email", correct_password.clone()),
    ] {
        let outcome = authenticator
            .authenticate(email, password, ip, at("2026-10-10T12:00:00Z"))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            PasswordAuthentication::Rejected {
                blocked_until: None
            }
        ));
    }

    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

#[tokio::test]
async fn a_rate_limited_identity_is_rejected_without_creating_a_session() {
    let _guard = KDF_TEST_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active", "locked@example.test").await;
    let password = String::from("rate limited strong password");
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(password.clone()).await.unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(user_id)
    .bind(&password_hash)
    .execute(database.pool())
    .await
    .unwrap();
    let authenticator =
        authenticator(&database, &directory, password_service, &password_hash, 1).await;
    let now = at("2026-10-10T12:00:00Z");
    let ip: IpAddr = "203.0.113.47".parse().unwrap();

    let first = authenticator
        .authenticate(
            "locked@example.test",
            String::from("wrong password here"),
            ip,
            now,
        )
        .await
        .unwrap();
    assert!(matches!(
        first,
        PasswordAuthentication::Rejected {
            blocked_until: Some(_)
        }
    ));
    let second = authenticator
        .authenticate("locked@example.test", password, ip, now)
        .await
        .unwrap();
    assert!(matches!(
        second,
        PasswordAuthentication::Rejected {
            blocked_until: Some(_)
        }
    ));
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}
