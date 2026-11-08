use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::{MasterKey, MasterKeyStore},
        rate_limit::{
            AuthRateLimitError, AuthRateLimitPolicies, AuthRateLimitPolicy, AuthRateLimiter,
        },
    },
    db::Database,
};
use sqlx::Row;
use std::net::IpAddr;
use tempfile::TempDir;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

fn policy(
    window: TimeDelta,
    threshold: u32,
    base_backoff: TimeDelta,
    maximum_backoff: TimeDelta,
) -> AuthRateLimitPolicy {
    AuthRateLimitPolicy::new(window, threshold, base_backoff, maximum_backoff).unwrap()
}

async fn new_limiter(
    database: &Database,
    master_key: MasterKey,
    policy: AuthRateLimitPolicy,
) -> AuthRateLimiter {
    new_limiter_with_policies(
        database,
        master_key,
        AuthRateLimitPolicies::new(policy, policy, policy),
    )
    .await
}

async fn new_limiter_with_policies(
    database: &Database,
    master_key: MasterKey,
    policies: AuthRateLimitPolicies,
) -> AuthRateLimiter {
    AuthRateLimiter::open(database.pool().clone(), master_key, policies)
        .await
        .unwrap()
}

async fn open_key(directory: &TempDir) -> MasterKey {
    MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap()
}

#[test]
fn rate_limit_policy_requires_positive_bounded_values() {
    assert!(matches!(
        AuthRateLimitPolicy::new(
            TimeDelta::zero(),
            1,
            TimeDelta::seconds(1),
            TimeDelta::seconds(1),
        ),
        Err(AuthRateLimitError::InvalidPolicy)
    ));
    assert!(matches!(
        AuthRateLimitPolicy::new(
            TimeDelta::minutes(1),
            0,
            TimeDelta::seconds(1),
            TimeDelta::seconds(1),
        ),
        Err(AuthRateLimitError::InvalidPolicy)
    ));
    assert!(matches!(
        AuthRateLimitPolicy::new(
            TimeDelta::minutes(1),
            1,
            TimeDelta::seconds(2),
            TimeDelta::seconds(1),
        ),
        Err(AuthRateLimitError::InvalidPolicy)
    ));
}

#[tokio::test]
async fn failure_thresholds_apply_across_ip_identity_and_installation() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(
        &database,
        open_key(&directory).await,
        policy(
            TimeDelta::minutes(10),
            2,
            TimeDelta::seconds(10),
            TimeDelta::seconds(40),
        ),
    )
    .await;
    let ip_one: IpAddr = "192.0.2.10".parse().unwrap();
    let ip_two: IpAddr = "192.0.2.11".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");

    assert!(
        limiter
            .record_failure(ip_one, "user@example.test", started)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        limiter
            .blocked_until(ip_two, "other@example.test", started)
            .await
            .unwrap()
            .is_none()
    );

    let blocked = limiter
        .record_failure(ip_one, "user@example.test", started + TimeDelta::seconds(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked, started + TimeDelta::seconds(11));
    assert_eq!(
        limiter
            .blocked_until(
                ip_two,
                "other@example.test",
                started + TimeDelta::seconds(2)
            )
            .await
            .unwrap(),
        Some(blocked),
        "the installation bucket blocks a different IP and identity"
    );
    assert_eq!(
        limiter
            .blocked_until(ip_two, "user@example.test", started + TimeDelta::seconds(2))
            .await
            .unwrap(),
        Some(blocked),
        "the identity bucket applies across IP addresses"
    );
}

#[tokio::test]
async fn scope_policies_are_independent_and_installation_limit_is_configurable() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let policies = AuthRateLimitPolicies::new(
        policy(
            TimeDelta::minutes(10),
            5,
            TimeDelta::seconds(10),
            TimeDelta::seconds(40),
        ),
        policy(
            TimeDelta::minutes(10),
            5,
            TimeDelta::seconds(10),
            TimeDelta::seconds(40),
        ),
        policy(
            TimeDelta::minutes(1),
            2,
            TimeDelta::seconds(3),
            TimeDelta::seconds(12),
        ),
    );
    let limiter = new_limiter_with_policies(&database, open_key(&directory).await, policies).await;
    let ip_one: IpAddr = "192.0.2.90".parse().unwrap();
    let ip_two: IpAddr = "192.0.2.91".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");

    limiter
        .record_failure(ip_one, "first@example.test", started)
        .await
        .unwrap();
    assert_eq!(
        limiter
            .record_failure(
                ip_one,
                "first@example.test",
                started + TimeDelta::seconds(1)
            )
            .await
            .unwrap(),
        Some(started + TimeDelta::seconds(4))
    );
    assert_eq!(
        limiter
            .blocked_until(
                ip_two,
                "second@example.test",
                started + TimeDelta::seconds(2)
            )
            .await
            .unwrap(),
        Some(started + TimeDelta::seconds(4)),
        "installation policy independently fences other identities"
    );
}

#[tokio::test]
async fn exponential_backoff_is_capped_and_window_reset_prevents_permanent_lockout() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(
        &database,
        open_key(&directory).await,
        policy(
            TimeDelta::hours(1),
            1,
            TimeDelta::seconds(10),
            TimeDelta::seconds(25),
        ),
    )
    .await;
    let ip: IpAddr = "198.51.100.4".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");

    assert_eq!(
        limiter
            .record_failure(ip, "alice@example.test", started)
            .await
            .unwrap(),
        Some(started + TimeDelta::seconds(10))
    );
    assert_eq!(
        limiter
            .record_failure(ip, "alice@example.test", started + TimeDelta::seconds(10),)
            .await
            .unwrap(),
        Some(started + TimeDelta::seconds(30))
    );
    assert_eq!(
        limiter
            .record_failure(ip, "alice@example.test", started + TimeDelta::seconds(30),)
            .await
            .unwrap(),
        Some(started + TimeDelta::seconds(55))
    );
    assert!(
        limiter
            .blocked_until(ip, "alice@example.test", started + TimeDelta::seconds(55))
            .await
            .unwrap()
            .is_none()
    );

    assert_eq!(
        limiter
            .record_failure(ip, "alice@example.test", started + TimeDelta::hours(1),)
            .await
            .unwrap(),
        Some(started + TimeDelta::hours(1) + TimeDelta::seconds(10))
    );
}

#[tokio::test]
async fn state_survives_reopen_and_success_clears_only_identity_scope() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let master_key = open_key(&directory).await;
    let policy = policy(
        TimeDelta::minutes(10),
        2,
        TimeDelta::seconds(10),
        TimeDelta::seconds(40),
    );
    let limiter = new_limiter(&database, master_key.clone(), policy).await;
    let ip_one: IpAddr = "203.0.113.20".parse().unwrap();
    let ip_two: IpAddr = "203.0.113.21".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");
    limiter
        .record_failure(ip_one, "alice@example.test", started)
        .await
        .unwrap();
    limiter.record_success("alice@example.test").await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_rate_limit_buckets")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(rows, 2, "success removes only the identity bucket");
    assert!(
        limiter
            .blocked_until(ip_two, "alice@example.test", started)
            .await
            .unwrap()
            .is_none()
    );

    let blocked = limiter
        .record_failure(ip_one, "bob@example.test", started + TimeDelta::seconds(1))
        .await
        .unwrap();
    assert_eq!(blocked, Some(started + TimeDelta::seconds(11)));
    limiter.record_success("bob@example.test").await.unwrap();
    assert!(
        limiter
            .blocked_until(
                ip_one,
                "charlie@example.test",
                started + TimeDelta::seconds(2)
            )
            .await
            .unwrap()
            .is_some(),
        "IP and installation buckets survive identity success"
    );
    drop(limiter);
    database.close().await;
    drop(database);

    let reopened = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(&reopened, master_key, policy).await;
    assert!(
        limiter
            .blocked_until(
                ip_two,
                "charlie@example.test",
                started + TimeDelta::seconds(2),
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_failures_serialize_and_corrupt_persisted_state_fails_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(
        &database,
        open_key(&directory).await,
        policy(
            TimeDelta::minutes(1),
            1,
            TimeDelta::seconds(5),
            TimeDelta::seconds(20),
        ),
    )
    .await;
    let ip: IpAddr = "192.0.2.44".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");
    let (first, second) = tokio::join!(
        limiter.record_failure(ip, "concurrent@example.test", started),
        limiter.record_failure(ip, "concurrent@example.test", started),
    );
    assert_eq!(first.unwrap(), Some(started + TimeDelta::seconds(5)));
    assert_eq!(second.unwrap(), Some(started + TimeDelta::seconds(5)));

    sqlx::query(
        "UPDATE auth_rate_limit_buckets SET window_started_at = 'not-a-time' \
         WHERE subject_hash = ( \
             SELECT subject_hash FROM auth_rate_limit_buckets \
             ORDER BY subject_hash LIMIT 1 \
         )",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(matches!(
        limiter
            .blocked_until(ip, "concurrent@example.test", started)
            .await,
        Err(AuthRateLimitError::CorruptState)
    ));
}

#[tokio::test]
async fn replacing_master_key_requires_explicit_rate_limit_state_migration() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let one_policy = policy(
        TimeDelta::minutes(1),
        3,
        TimeDelta::seconds(5),
        TimeDelta::seconds(20),
    );
    let original_key = open_key(&directory).await;
    let policies = AuthRateLimitPolicies::new(one_policy, one_policy, one_policy);
    let _limiter = new_limiter_with_policies(&database, original_key, policies).await;
    let alternate_key_path = directory.path().join("alternate.key");
    let alternate_key = MasterKeyStore::load_or_create(&alternate_key_path).unwrap();

    assert!(matches!(
        AuthRateLimiter::open(database.pool().clone(), alternate_key, policies).await,
        Err(AuthRateLimitError::MasterKeyChanged)
    ));
}

#[tokio::test]
async fn expired_buckets_can_be_purged_after_window_and_backoff() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(
        &database,
        open_key(&directory).await,
        policy(
            TimeDelta::minutes(1),
            3,
            TimeDelta::seconds(5),
            TimeDelta::seconds(20),
        ),
    )
    .await;
    let ip: IpAddr = "198.51.100.22".parse().unwrap();
    let started = at("2026-10-10T12:00:00Z");
    limiter
        .record_failure(ip, "purge@example.test", started)
        .await
        .unwrap();
    assert_eq!(
        limiter
            .purge_expired(started + TimeDelta::minutes(1))
            .await
            .unwrap(),
        3
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_rate_limit_buckets")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn bucket_rows_do_not_store_ip_or_identity_strings() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let limiter = new_limiter(
        &database,
        open_key(&directory).await,
        policy(
            TimeDelta::minutes(1),
            3,
            TimeDelta::seconds(5),
            TimeDelta::seconds(20),
        ),
    )
    .await;
    let ip: IpAddr = "203.0.113.33".parse().unwrap();
    limiter
        .record_failure(ip, "private@example.test", at("2026-10-10T12:00:00Z"))
        .await
        .unwrap();

    let row = sqlx::query("SELECT subject_hash FROM auth_rate_limit_buckets LIMIT 1")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let subject_hash: Vec<u8> = row.try_get("subject_hash").unwrap();
    assert_eq!(subject_hash.len(), 32);
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('auth_rate_limit_buckets')")
            .fetch_all(database.pool())
            .await
            .unwrap();
    assert!(
        !columns
            .iter()
            .any(|name| name.contains("email") || name.contains("ip"))
    );
}
