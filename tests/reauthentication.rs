use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService,
        credentials::CredentialStore,
        email_addresses::{EmailAddressManagementError, EmailAddressStore},
        reauthentication::ReauthenticationService,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
};
use tempfile::TempDir;
use uuid::Uuid;

const PASSWORD: &str = "a valid reauthentication password";
static REAUTH_TEST_KDF_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn fixture() -> (
    TempDir,
    Database,
    String,
    String,
    PasswordService,
    SessionStore,
    ReauthenticationService,
) {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let user_id = Uuid::now_v7().to_string();
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(PASSWORD.to_owned()).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Reauthentication test', 'active', \
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
    .bind(&password_hash)
    .execute(database.pool())
    .await
    .unwrap();
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = at("2026-10-10T12:00:00Z");
    let session = sessions.issue(&user_id, now).await.unwrap();
    let reauthentication = ReauthenticationService::new(
        CredentialStore::new(database.pool().clone(), password_service.clone()),
        sessions.clone(),
        password_hash.clone(),
    );
    (
        directory,
        database,
        user_id,
        session.token.expose().to_owned(),
        password_service,
        sessions,
        reauthentication,
    )
}

#[tokio::test]
async fn valid_password_reauthentication_is_recorded_only_for_the_current_session() {
    let _guard = REAUTH_TEST_KDF_LOCK.lock().await;
    let (_directory, database, user_id, token_value, _password_service, sessions, service) =
        fixture().await;
    let token = puxiri::auth::SessionToken::parse(&token_value).unwrap();
    let now = at("2026-10-10T12:01:00Z");

    let proof = service
        .reauthenticate_password(&token, PASSWORD.to_owned(), now)
        .await
        .unwrap()
        .unwrap();
    let timestamp: Option<String> =
        sqlx::query_scalar("SELECT reauthenticated_at FROM sessions WHERE token_hash = ?")
            .bind(token.digest().as_slice())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(timestamp.as_deref(), Some("2026-10-10T12:01:00.000000000Z"));

    sessions.revoke_all(&user_id, now).await.unwrap();
    let email_store = EmailAddressStore::new(database.pool().clone());
    assert!(matches!(
        email_store
            .add_pending(&proof, "after-revocation@example.test", now)
            .await,
        Err(EmailAddressManagementError::ReauthenticationRequired)
    ));
}

#[tokio::test]
async fn wrong_password_future_proof_and_expired_proof_fail_closed() {
    let _guard = REAUTH_TEST_KDF_LOCK.lock().await;
    let (_directory, database, _user_id, token_value, _password_service, _sessions, service) =
        fixture().await;
    let token = puxiri::auth::SessionToken::parse(&token_value).unwrap();
    let authenticated_at = at("2026-10-10T12:01:00Z");

    assert!(
        service
            .reauthenticate_password(&token, "incorrect password".to_owned(), authenticated_at)
            .await
            .unwrap()
            .is_none()
    );
    let proof = service
        .reauthenticate_password(&token, PASSWORD.to_owned(), authenticated_at)
        .await
        .unwrap()
        .unwrap();
    let email_store = EmailAddressStore::new(database.pool().clone());
    assert!(matches!(
        email_store
            .add_pending(
                &proof,
                "future-proof@example.test",
                authenticated_at - TimeDelta::seconds(1),
            )
            .await,
        Err(EmailAddressManagementError::ReauthenticationRequired)
    ));
    assert!(matches!(
        email_store
            .add_pending(
                &proof,
                "expired-proof@example.test",
                authenticated_at + TimeDelta::minutes(10) + TimeDelta::nanoseconds(1),
            )
            .await,
        Err(EmailAddressManagementError::ReauthenticationRequired)
    ));
}

#[tokio::test]
async fn installation_admin_email_mutations_require_mfa_not_password_reauthentication() {
    let _guard = REAUTH_TEST_KDF_LOCK.lock().await;
    let (_directory, database, user_id, token_value, _password_service, _sessions, service) =
        fixture().await;
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Reauthentication test', 'en', 'UTC', \
                 '2026-10-10T00:00:00.000000000Z', '2026-10-10T00:00:00.000000000Z')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, 'admin', '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-10T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .execute(database.pool())
    .await
    .unwrap();
    let token = puxiri::auth::SessionToken::parse(&token_value).unwrap();
    let now = at("2026-10-10T12:01:00Z");
    let proof = service
        .reauthenticate_password(&token, PASSWORD.to_owned(), now)
        .await
        .unwrap()
        .unwrap();

    let error = EmailAddressStore::new(database.pool().clone())
        .add_pending(&proof, "admin-email@example.test", now)
        .await
        .unwrap_err();
    assert!(matches!(error, EmailAddressManagementError::MfaRequired));
}

#[tokio::test]
async fn changed_password_is_rejected_during_reauthentication() {
    let _guard = REAUTH_TEST_KDF_LOCK.lock().await;
    let (_directory, database, user_id, token_value, password_service, sessions, service) =
        fixture().await;
    let token = puxiri::auth::SessionToken::parse(&token_value).unwrap();
    let now = at("2026-10-10T12:01:00Z");
    let changed_hash = password_service
        .hash("a changed password after verification".to_owned())
        .await
        .unwrap();
    sqlx::query("UPDATE password_credentials SET password_hash = ? WHERE user_id = ?")
        .bind(&changed_hash)
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();

    let proof = service
        .reauthenticate_password(&token, PASSWORD.to_owned(), now)
        .await
        .unwrap();
    assert!(proof.is_none());
    let timestamp: Option<String> =
        sqlx::query_scalar("SELECT reauthenticated_at FROM sessions WHERE token_hash = ?")
            .bind(token.digest().as_slice())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(timestamp.is_none());
    assert!(sessions.validate(&token, now).await.unwrap().is_some());
}
