use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService, SessionToken,
        credentials::CredentialStore,
        email_addresses::{EmailAddressManagementError, EmailAddressStore},
        reauthentication::{ReauthenticationService, RecentReauthentication},
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
};
use tempfile::TempDir;
use uuid::Uuid;

const PASSWORD: &str = "correct password for email tests";
static EMAIL_TEST_KDF_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestAuth {
    service: ReauthenticationService,
    session_token: SessionToken,
}

impl TestAuth {
    async fn proof(&self, now: DateTime<Utc>) -> RecentReauthentication {
        self.service
            .reauthenticate_password(&self.session_token, PASSWORD.to_owned(), now)
            .await
            .unwrap()
            .expect("test password should reauthenticate")
    }
}

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn password_fixture() -> (PasswordService, String) {
    let service = PasswordService::default();
    let hash = service.hash(PASSWORD.to_owned()).await.unwrap();
    (service, hash)
}

async fn create_user(database: &Database, password_hash: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Email address test', 'active', \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(password_hash)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn create_auth(
    database: &Database,
    user_id: &str,
    password_service: PasswordService,
    dummy_hash: String,
    now: DateTime<Utc>,
) -> TestAuth {
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let session = sessions.issue(user_id, now).await.unwrap();
    TestAuth {
        service: ReauthenticationService::new(
            CredentialStore::new(database.pool().clone(), password_service),
            sessions,
            dummy_hash,
        ),
        session_token: session.token,
    }
}

async fn confirmed_address(
    database: &Database,
    auth: &TestAuth,
    address: &str,
    is_primary: bool,
    now: DateTime<Utc>,
) -> String {
    let store = EmailAddressStore::new(database.pool().clone());
    let proof = auth.proof(now).await;
    let record = store.add_pending(&proof, address, now).await.unwrap();
    sqlx::query("UPDATE user_emails SET confirmed_at = ? WHERE id = ?")
        .bind((now + TimeDelta::seconds(1)).to_rfc3339())
        .bind(&record.id)
        .execute(database.pool())
        .await
        .unwrap();
    if is_primary {
        let primary_at = now + TimeDelta::seconds(2);
        let primary_proof = auth.proof(primary_at).await;
        assert!(
            store
                .set_primary(&primary_proof, &record.id, primary_at)
                .await
                .unwrap()
        );
    }
    record.id
}

#[tokio::test]
async fn adding_addresses_trims_outer_whitespace_and_reserves_case_insensitive_keys() {
    let _guard = EMAIL_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let now = at("2026-10-10T12:00:00Z");
    let (password_service, password_hash) = password_fixture().await;
    let user_id = create_user(&database, &password_hash).await;
    let other_user_id = create_user(&database, &password_hash).await;
    let auth = create_auth(
        &database,
        &user_id,
        password_service.clone(),
        password_hash.clone(),
        now,
    )
    .await;
    let other_auth = create_auth(
        &database,
        &other_user_id,
        password_service,
        password_hash,
        now,
    )
    .await;
    let store = EmailAddressStore::new(database.pool().clone());

    let proof = auth.proof(now).await;
    let added = store
        .add_pending(&proof, "  Person+tag@example.test  ", now)
        .await
        .unwrap();
    assert_eq!(added.address, "Person+tag@example.test");
    assert!(added.confirmed_at.is_none());
    assert!(!added.is_primary);

    let duplicate_at = now + TimeDelta::minutes(1);
    let duplicate_proof = other_auth.proof(duplicate_at).await;
    assert!(matches!(
        store
            .add_pending(&duplicate_proof, "person+tag@EXAMPLE.TEST", duplicate_at,)
            .await,
        Err(EmailAddressManagementError::AddressUnavailable)
    ));
    let invalid_at = now + TimeDelta::minutes(2);
    let invalid_proof = auth.proof(invalid_at).await;
    assert!(matches!(
        store
            .add_pending(&invalid_proof, "invalid address", invalid_at)
            .await,
        Err(EmailAddressManagementError::InvalidAddress(_))
    ));

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE event_kind = 'auth.email.address_added'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 1);
    sqlx::query(
        "CREATE TRIGGER fail_email_audit BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.email.address_added' \
         BEGIN SELECT RAISE(ABORT, 'fixture audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let audit_failure_at = now + TimeDelta::minutes(3);
    assert!(
        store
            .add_pending(&proof, "audit-failure@example.test", audit_failure_at,)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_email_audit")
        .execute(database.pool())
        .await
        .unwrap();
    let partial_address: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_emails WHERE address_key = 'audit-failure@example.test'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(partial_address, 0);
}

#[tokio::test]
async fn only_a_confirmed_owned_email_can_become_primary() {
    let _guard = EMAIL_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let now = at("2026-10-10T12:00:00Z");
    let (password_service, password_hash) = password_fixture().await;
    let user_id = create_user(&database, &password_hash).await;
    let other_user_id = create_user(&database, &password_hash).await;
    let auth = create_auth(
        &database,
        &user_id,
        password_service.clone(),
        password_hash.clone(),
        now,
    )
    .await;
    let other_auth = create_auth(
        &database,
        &other_user_id,
        password_service,
        password_hash,
        now,
    )
    .await;
    let store = EmailAddressStore::new(database.pool().clone());
    let pending_proof = auth.proof(now).await;
    let pending = store
        .add_pending(&pending_proof, "pending@example.test", now)
        .await
        .unwrap();
    let confirmed_at = now + TimeDelta::minutes(1);
    let confirmed = confirmed_address(
        &database,
        &auth,
        "confirmed@example.test",
        false,
        confirmed_at,
    )
    .await;

    let pending_change_at = now + TimeDelta::minutes(2);
    let pending_change_proof = auth.proof(pending_change_at).await;
    assert!(
        !store
            .set_primary(&pending_change_proof, &pending.id, pending_change_at)
            .await
            .unwrap()
    );
    let foreign_change_at = now + TimeDelta::minutes(3);
    let foreign_change_proof = other_auth.proof(foreign_change_at).await;
    assert!(
        !store
            .set_primary(&foreign_change_proof, &confirmed, foreign_change_at)
            .await
            .unwrap()
    );
    let primary_change_at = now + TimeDelta::minutes(4);
    let primary_change_proof = auth.proof(primary_change_at).await;
    assert!(
        store
            .set_primary(&primary_change_proof, &confirmed, primary_change_at)
            .await
            .unwrap()
    );

    let listed = store.list_for_user(&user_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, confirmed);
    assert!(listed[0].is_primary);
    assert!(listed[0].confirmed_at.is_some());
    assert_eq!(listed[1].id, pending.id);
    assert!(!listed[1].is_primary);
    assert!(listed[1].confirmed_at.is_none());
}

#[tokio::test]
async fn removing_primary_promotes_another_confirmed_address_and_cascades_email_tokens() {
    let _guard = EMAIL_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let now = at("2026-10-10T12:00:00Z");
    let (password_service, password_hash) = password_fixture().await;
    let user_id = create_user(&database, &password_hash).await;
    let auth = create_auth(&database, &user_id, password_service, password_hash, now).await;
    let store = EmailAddressStore::new(database.pool().clone());
    let old_primary = confirmed_address(&database, &auth, "primary@example.test", true, now).await;
    let replacement = confirmed_address(
        &database,
        &auth,
        "secondary@example.test",
        false,
        now + TimeDelta::minutes(1),
    )
    .await;
    let pending_at = now + TimeDelta::minutes(2);
    let pending_proof = auth.proof(pending_at).await;
    let pending = store
        .add_pending(&pending_proof, "pending@example.test", pending_at)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO email_verification_tokens \
         (id, token_hash, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, '2026-10-10T00:00:00.000000000Z', \
                 '2026-10-11T12:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind([3_u8; 32].as_slice())
    .bind(&pending.id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO password_reset_tokens \
         (id, token_hash, user_id, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, '2026-10-10T00:00:00.000000000Z', \
                 '2026-10-11T00:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind([4_u8; 32].as_slice())
    .bind(&user_id)
    .bind(&pending.id)
    .execute(database.pool())
    .await
    .unwrap();

    let remove_primary_at = now + TimeDelta::minutes(3);
    let remove_primary_proof = auth.proof(remove_primary_at).await;
    assert!(
        store
            .remove(&remove_primary_proof, &old_primary, remove_primary_at)
            .await
            .unwrap()
    );
    let listed = store.list_for_user(&user_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, replacement);
    assert!(listed[0].is_primary);
    let verification_tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM email_verification_tokens WHERE email_id = ?")
            .bind(&pending.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let reset_tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE email_id = ?")
            .bind(&pending.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(verification_tokens, 1);
    assert_eq!(reset_tokens, 1);

    let remove_pending_at = now + TimeDelta::minutes(4);
    let remove_pending_proof = auth.proof(remove_pending_at).await;
    assert!(
        store
            .remove(&remove_pending_proof, &pending.id, remove_pending_at)
            .await
            .unwrap()
    );
    let verification_tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM email_verification_tokens WHERE email_id = ?")
            .bind(&pending.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let reset_tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE email_id = ?")
            .bind(&pending.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(verification_tokens, 0);
    assert_eq!(reset_tokens, 0);
}

#[tokio::test]
async fn last_address_and_last_confirmed_address_cannot_be_removed() {
    let _guard = EMAIL_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let now = at("2026-10-10T12:00:00Z");
    let (password_service, password_hash) = password_fixture().await;
    let only_pending_user = create_user(&database, &password_hash).await;
    let only_confirmed_user = create_user(&database, &password_hash).await;
    let only_pending_auth = create_auth(
        &database,
        &only_pending_user,
        password_service.clone(),
        password_hash.clone(),
        now,
    )
    .await;
    let only_confirmed_auth = create_auth(
        &database,
        &only_confirmed_user,
        password_service,
        password_hash,
        now,
    )
    .await;
    let store = EmailAddressStore::new(database.pool().clone());
    let only_pending_proof = only_pending_auth.proof(now).await;
    let only_pending = store
        .add_pending(&only_pending_proof, "only-pending@example.test", now)
        .await
        .unwrap();
    let only_confirmed = confirmed_address(
        &database,
        &only_confirmed_auth,
        "only-confirmed@example.test",
        true,
        now,
    )
    .await;
    let extra_pending_at = now + TimeDelta::minutes(1);
    let extra_pending_proof = only_confirmed_auth.proof(extra_pending_at).await;
    let extra_pending = store
        .add_pending(
            &extra_pending_proof,
            "extra-pending@example.test",
            extra_pending_at,
        )
        .await
        .unwrap();

    let remove_only_pending_proof = only_pending_auth.proof(now + TimeDelta::minutes(2)).await;
    assert!(matches!(
        store
            .remove(
                &remove_only_pending_proof,
                &only_pending.id,
                now + TimeDelta::minutes(2),
            )
            .await,
        Err(EmailAddressManagementError::LastAddress)
    ));
    let remove_only_confirmed_proof = only_confirmed_auth.proof(now + TimeDelta::minutes(2)).await;
    assert!(matches!(
        store
            .remove(
                &remove_only_confirmed_proof,
                &only_confirmed,
                now + TimeDelta::minutes(2),
            )
            .await,
        Err(EmailAddressManagementError::LastConfirmedAddress)
    ));
    let remove_extra_proof = only_confirmed_auth.proof(now + TimeDelta::minutes(3)).await;
    assert!(
        store
            .remove(
                &remove_extra_proof,
                &extra_pending.id,
                now + TimeDelta::minutes(3),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn removal_failure_rolls_back_primary_promotion() {
    let _guard = EMAIL_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let now = at("2026-10-10T12:00:00Z");
    let (password_service, password_hash) = password_fixture().await;
    let user_id = create_user(&database, &password_hash).await;
    let auth = create_auth(&database, &user_id, password_service, password_hash, now).await;
    let store = EmailAddressStore::new(database.pool().clone());
    let original = confirmed_address(&database, &auth, "first@example.test", true, now).await;
    let replacement = confirmed_address(
        &database,
        &auth,
        "second@example.test",
        false,
        now + TimeDelta::minutes(1),
    )
    .await;
    sqlx::query(
        "CREATE TRIGGER fail_email_delete BEFORE DELETE ON user_emails \
         BEGIN SELECT RAISE(ABORT, 'fixture delete failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let remove_at = now + TimeDelta::minutes(3);
    let remove_proof = auth.proof(remove_at).await;
    assert!(
        store
            .remove(&remove_proof, &original, remove_at)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_email_delete")
        .execute(database.pool())
        .await
        .unwrap();
    let listed = store.list_for_user(&user_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().filter(|email| email.is_primary).count(), 1);
    assert_eq!(
        listed.iter().find(|email| email.is_primary).unwrap().id,
        original
    );
    assert!(listed.iter().any(|email| email.id == replacement));
}
