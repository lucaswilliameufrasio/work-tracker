use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        SessionToken,
        email_verification::{
            EmailVerificationError, EmailVerificationPolicy, EmailVerificationStore,
        },
    },
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::Zeroizing;

async fn create_user(database: &Database, state: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Email verification test', ?, \
                 '2026-10-09T00:00:00.000000000Z', '2026-10-09T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn create_email(database: &Database, user_id: &str) -> String {
    let email_id = Uuid::now_v7().to_string();
    let address = format!("user-{email_id}@example.test");
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 0, NULL, '2026-10-09T00:00:00.000000000Z')",
    )
    .bind(&email_id)
    .bind(user_id)
    .bind(&address)
    .bind(address.to_ascii_lowercase())
    .execute(database.pool())
    .await
    .unwrap();
    email_id
}

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

fn store(database: &Database, lifetime: TimeDelta) -> EmailVerificationStore {
    EmailVerificationStore::new(
        database.pool().clone(),
        EmailVerificationPolicy::new(lifetime).unwrap(),
    )
}

#[test]
fn email_verification_policy_rejects_non_positive_lifetime() {
    assert!(matches!(
        EmailVerificationPolicy::new(TimeDelta::zero()),
        Err(EmailVerificationError::InvalidPolicy)
    ));
    assert!(matches!(
        EmailVerificationPolicy::new(TimeDelta::seconds(-1)),
        Err(EmailVerificationError::InvalidPolicy)
    ));
}

#[tokio::test]
async fn issue_persists_only_digest_and_reissue_invalidates_previous_token() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let email_id = create_email(&database, &user_id).await;
    let store = store(&database, TimeDelta::hours(1));
    let issued_at = at("2026-10-09T12:00:00Z");
    let first = store.issue(&user_id, &email_id, issued_at).await.unwrap();

    let first_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM email_verification_tokens WHERE id = ?")
            .bind(&first.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(first_hash.len(), 32);
    assert_ne!(first_hash, first.token.expose().as_bytes());
    assert_eq!(first.token.expose().len(), 43);

    let second = store
        .issue(&user_id, &email_id, issued_at + TimeDelta::minutes(1))
        .await
        .unwrap();
    assert!(
        !store
            .confirm(&first.token, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
    );
    assert!(
        store
            .confirm(&second.token, issued_at + TimeDelta::minutes(2))
            .await
            .unwrap()
    );
    assert!(
        !store
            .confirm(&second.token, issued_at + TimeDelta::minutes(3))
            .await
            .unwrap()
    );

    let row = sqlx::query("SELECT confirmed_at, is_primary FROM user_emails WHERE id = ?")
        .bind(&email_id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<String, _>("confirmed_at").unwrap(),
        "2026-10-09T12:02:00.000000000Z"
    );
    assert_eq!(row.try_get::<i64, _>("is_primary").unwrap(), 0);
}

#[tokio::test]
async fn expired_future_and_malformed_timestamps_fail_closed() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let email_id = create_email(&database, &user_id).await;
    let store = store(&database, TimeDelta::minutes(5));
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, &email_id, issued_at).await.unwrap();

    assert!(
        !store
            .confirm(&issued.token, issued.expires_at)
            .await
            .unwrap()
    );

    let future = store
        .issue(&user_id, &email_id, at("2026-10-09T13:00:00Z"))
        .await
        .unwrap();
    assert!(
        !store
            .confirm(&future.token, at("2026-10-09T12:30:00Z"))
            .await
            .unwrap()
    );

    let malformed = store
        .issue(&user_id, &email_id, at("2026-10-09T12:30:00Z"))
        .await
        .unwrap();
    sqlx::query("UPDATE email_verification_tokens SET expires_at = 'invalid' WHERE id = ?")
        .bind(&malformed.id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        !store
            .confirm(&malformed.token, at("2026-10-09T12:31:00Z"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn ban_and_unknown_token_cannot_confirm_email() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let email_id = create_email(&database, &user_id).await;
    let store = store(&database, TimeDelta::hours(1));
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, &email_id, issued_at).await.unwrap();
    let unknown = SessionToken::generate().unwrap();

    assert!(
        !store
            .confirm(
                &puxiri::auth::email_verification::EmailVerificationToken::parse(unknown.expose())
                    .unwrap(),
                issued_at,
            )
            .await
            .unwrap()
    );
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        !store
            .confirm(&issued.token, issued_at + TimeDelta::minutes(1))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn concurrent_confirmations_have_one_winner_and_trigger_failure_rolls_back() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let email_id = create_email(&database, &user_id).await;
    let store = store(&database, TimeDelta::hours(1));
    let issued_at = at("2026-10-09T12:00:00Z");
    let issued = store.issue(&user_id, &email_id, issued_at).await.unwrap();
    let token_value = Zeroizing::new(issued.token.expose().to_owned());
    let first_token =
        puxiri::auth::email_verification::EmailVerificationToken::parse(token_value.as_str())
            .unwrap();
    let second_token =
        puxiri::auth::email_verification::EmailVerificationToken::parse(token_value.as_str())
            .unwrap();

    sqlx::query(
        "CREATE TRIGGER fail_email_confirmation \
         BEFORE UPDATE OF confirmed_at ON user_emails \
         BEGIN SELECT RAISE(ABORT, 'test confirmation failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(matches!(
        store
            .confirm(&first_token, issued_at + TimeDelta::minutes(1))
            .await,
        Err(EmailVerificationError::Database(_))
    ));
    let consumed_at: Option<String> =
        sqlx::query_scalar("SELECT consumed_at FROM email_verification_tokens WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(consumed_at.is_none());

    sqlx::query("DROP TRIGGER fail_email_confirmation")
        .execute(database.pool())
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        store.confirm(&first_token, issued_at + TimeDelta::minutes(1)),
        store.confirm(&second_token, issued_at + TimeDelta::minutes(1)),
    );
    assert_ne!(first.unwrap(), second.unwrap());
}
