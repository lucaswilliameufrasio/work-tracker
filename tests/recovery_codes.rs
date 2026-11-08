use chrono::{DateTime, Utc};
use puxiri::{
    auth::recovery::{RecoveryCodePolicy, RecoveryCodeStore},
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::Zeroizing;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_user(database: &Database, state: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Recovery-code test', ?, \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

#[test]
fn caller_selected_code_count_is_bounded() {
    assert!(RecoveryCodePolicy::new(0).is_err());
    assert!(RecoveryCodePolicy::new(21).is_err());
    assert!(RecoveryCodePolicy::new(1).is_ok());
    assert!(RecoveryCodePolicy::new(20).is_ok());
}

#[tokio::test]
async fn issuance_persists_only_digests_and_codes_are_user_scoped_one_time_values() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let other_user_id = create_user(&database, "active").await;
    let store = RecoveryCodeStore::new(database.pool().clone());
    let batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(5).unwrap(),
            at("2026-10-10T12:00:00Z"),
        )
        .await
        .unwrap();
    let codes: Vec<Zeroizing<String>> = batch
        .codes()
        .map(|value| Zeroizing::new(value.to_owned()))
        .collect();
    assert_eq!(codes.len(), 5);
    assert!(codes.iter().all(|code| code.len() == 22));

    let rows = sqlx::query("SELECT code_hash, consumed_at, invalidated_at FROM recovery_codes")
        .fetch_all(database.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
    for row in &rows {
        let digest: Vec<u8> = row.try_get("code_hash").unwrap();
        assert_eq!(digest.len(), 32);
        for code in &codes {
            assert_ne!(digest.as_slice(), code.as_bytes());
        }
        let consumed: Option<String> = row.try_get("consumed_at").unwrap();
        let invalidated: Option<String> = row.try_get("invalidated_at").unwrap();
        assert_eq!(consumed, None);
        assert_eq!(invalidated, None);
    }

    let candidate = codes[0].clone();
    assert!(
        !store
            .consume(
                &other_user_id,
                candidate.as_str(),
                at("2026-10-10T12:01:00Z")
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .consume(&user_id, candidate.as_str(), at("2026-10-10T12:01:00Z"))
            .await
            .unwrap()
    );
    assert!(
        !store
            .consume(&user_id, candidate.as_str(), at("2026-10-10T12:02:00Z"))
            .await
            .unwrap()
    );
    assert!(
        !store
            .consume(&user_id, "not-a-recovery-code", at("2026-10-10T12:02:00Z"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn concurrent_consumers_have_exactly_one_winner() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = RecoveryCodeStore::new(database.pool().clone());
    let batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(1).unwrap(),
            at("2026-10-10T12:00:00Z"),
        )
        .await
        .unwrap();
    let code = Zeroizing::new(batch.codes().next().unwrap().to_owned());

    let (first, second) = tokio::join!(
        store.consume(&user_id, code.as_str(), at("2026-10-10T12:01:00Z")),
        store.consume(&user_id, code.as_str(), at("2026-10-10T12:01:00Z")),
    );
    assert_ne!(first.unwrap(), second.unwrap());
}

#[tokio::test]
async fn consuming_with_recovery_access_rolls_back_as_one_transaction() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = RecoveryCodeStore::new(database.pool().clone());
    let batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(1).unwrap(),
            at("2026-10-10T12:00:00Z"),
        )
        .await
        .unwrap();
    let code = Zeroizing::new(batch.codes().next().unwrap().to_owned());
    sqlx::query(
        "CREATE TRIGGER fail_recovery_session BEFORE INSERT ON sessions \
         BEGIN SELECT RAISE(ABORT, 'fixture recovery session failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        store
            .consume_in_transaction(
                &mut transaction,
                &user_id,
                code.as_str(),
                at("2026-10-10T12:01:00Z"),
            )
            .await
            .unwrap()
    );
    let session_insert = sqlx::query(
        "INSERT INTO sessions \
         (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
         VALUES (?, ?, ?, '2026-10-10T12:01:00.000000000Z', \
                 '2026-10-10T12:01:00.000000000Z', \
                 '2026-10-10T12:06:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind([7_u8; 32].as_slice())
    .bind(&user_id)
    .execute(&mut *transaction)
    .await;
    assert!(session_insert.is_err());
    transaction.rollback().await.unwrap();

    sqlx::query("DROP TRIGGER fail_recovery_session")
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .consume(&user_id, code.as_str(), at("2026-10-10T12:02:00Z"),)
            .await
            .unwrap(),
        "failed access creation must not burn the recovery code"
    );
}

#[tokio::test]
async fn rotation_invalidates_old_codes_and_reissue_failure_rolls_back_invalidation() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = RecoveryCodeStore::new(database.pool().clone());
    let first_batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(2).unwrap(),
            at("2026-10-10T12:00:00Z"),
        )
        .await
        .unwrap();
    let old_code = Zeroizing::new(first_batch.codes().next().unwrap().to_owned());

    sqlx::query(
        "CREATE TRIGGER fail_recovery_code_insert BEFORE INSERT ON recovery_codes \
         BEGIN SELECT RAISE(ABORT, 'fixture insertion failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        store
            .issue(
                &user_id,
                RecoveryCodePolicy::new(2).unwrap(),
                at("2026-10-10T12:01:00Z")
            )
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_recovery_code_insert")
        .execute(database.pool())
        .await
        .unwrap();

    assert!(
        store
            .consume(&user_id, old_code.as_str(), at("2026-10-10T12:02:00Z"))
            .await
            .unwrap(),
        "failed replacement must leave the prior code usable"
    );

    let second_batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(1).unwrap(),
            at("2026-10-10T12:03:00Z"),
        )
        .await
        .unwrap();
    let rotated_code = Zeroizing::new(second_batch.codes().next().unwrap().to_owned());
    let third_batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(1).unwrap(),
            at("2026-10-10T12:04:00Z"),
        )
        .await
        .unwrap();
    let newest_code = Zeroizing::new(third_batch.codes().next().unwrap().to_owned());
    assert!(
        !store
            .consume(&user_id, rotated_code.as_str(), at("2026-10-10T12:05:00Z"))
            .await
            .unwrap()
    );
    assert!(
        store
            .consume(&user_id, newest_code.as_str(), at("2036-10-10T12:05:00Z"))
            .await
            .unwrap(),
        "codes have no implicit TTL; explicit rotation invalidates them"
    );
}

#[tokio::test]
async fn inactive_users_cannot_issue_or_consume_codes() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = create_user(&database, "active").await;
    let store = RecoveryCodeStore::new(database.pool().clone());
    let batch = store
        .issue(
            &user_id,
            RecoveryCodePolicy::new(1).unwrap(),
            at("2026-10-10T12:00:00Z"),
        )
        .await
        .unwrap();
    let code = Zeroizing::new(batch.codes().next().unwrap().to_owned());
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();

    assert!(
        store
            .consume(&user_id, code.as_str(), at("2026-10-10T12:01:00Z"))
            .await
            .is_ok_and(|consumed| !consumed)
    );
    assert!(
        store
            .issue(
                &user_id,
                RecoveryCodePolicy::new(1).unwrap(),
                at("2026-10-10T12:02:00Z")
            )
            .await
            .is_err()
    );
}
