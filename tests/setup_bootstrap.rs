use chrono::{DateTime, Utc};
use puxiri::{
    auth::setup::{SetupToken, SetupTokenError, SetupTokenStore},
    db::Database,
};
use tempfile::TempDir;
use zeroize::Zeroizing;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

#[tokio::test]
async fn local_token_is_digest_only_and_reissue_invalidates_the_previous_value() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let store = SetupTokenStore::new(database.pool().clone());
    let first = store.issue(at("2026-10-09T12:00:00Z")).await.unwrap();
    let first_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM setup_bootstrap_tokens WHERE singleton = 1")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(first_hash.len(), 32);
    assert_ne!(first_hash, first.expose().as_bytes());
    assert!(store.is_current(&first).await.unwrap());

    let second = store.issue(at("2026-10-09T12:01:00Z")).await.unwrap();
    assert!(!store.is_current(&first).await.unwrap());
    assert!(store.is_current(&second).await.unwrap());
}

#[tokio::test]
async fn pending_token_survives_database_reopen_and_is_invalidated_after_setup() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let store = SetupTokenStore::new(database.pool().clone());
    let issued = store.issue(at("2026-10-09T12:00:00Z")).await.unwrap();
    let token_value = Zeroizing::new(issued.expose().to_owned());

    drop(issued);
    drop(store);
    database.close().await;
    drop(database);

    let reopened = Database::open(directory.path()).await.unwrap();
    let store = SetupTokenStore::new(reopened.pool().clone());
    let token = SetupToken::parse(token_value.as_str()).unwrap();
    assert!(store.is_current(&token).await.unwrap());

    sqlx::query(
        "UPDATE installation_state SET setup_completed_at = \
         '2026-10-09T12:30:00.000000000Z' WHERE singleton = 1",
    )
    .execute(reopened.pool())
    .await
    .unwrap();
    assert!(!store.is_current(&token).await.unwrap());
    assert!(matches!(
        store.issue(at("2026-10-09T12:31:00Z")).await,
        Err(SetupTokenError::SetupComplete)
    ));
}
