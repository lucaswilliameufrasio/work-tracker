use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    access::{AccessCapability, AccessStore, authorize},
    accounts::{AccountAdministrationError, AccountAdministrationStore, AccountStatusChange},
    auth::session::{SessionPolicy, SessionStore},
    db::Database,
    projects::ProjectStore,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Account deactivation test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, name: &str, role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', '2026-10-15T08:00:00.000000000Z', \
                 '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(name)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members \
         (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn grant_installation_admin(database: &Database, user_id: &str) {
    sqlx::query(
        "INSERT INTO installation_admins (user_id, created_at) \
         VALUES (?, '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(user_id)
    .execute(database.pool())
    .await
    .unwrap();
}

async fn add_project_manager(database: &Database, project_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO project_members \
         (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, 'manager', '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(project_id)
    .bind(user_id)
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_project(database: &Database, owner_id: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            "DIS",
            "Account deactivation project",
            None,
            at("2026-10-15T08:00:00Z"),
        )
        .await
        .unwrap()
        .id
}

fn session_store(database: &Database) -> SessionStore {
    SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    )
}

#[tokio::test]
async fn self_deactivation_revokes_credentials_and_reactivation_never_restores_them() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Organization owner", "owner").await;
    let user = create_user(&database, "Self-deactivating manager", "member").await;
    let replacement = create_user(&database, "Replacement manager", "member").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    let project_id = create_project(&database, &owner).await;
    add_project_manager(&database, &project_id, &user).await;
    add_project_manager(&database, &project_id, &replacement).await;

    let now = at("2026-10-15T09:00:00Z");
    let sessions = session_store(&database);
    let issued_session = sessions.issue(&user, now).await.unwrap();
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, 'deactivation@example.test', 'deactivation@example.test', 1, ?, ?)",
    )
    .bind(&email_id)
    .bind(&user)
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_verification_tokens \
         (id, token_hash, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(vec![1_u8; 32])
    .bind(&email_id)
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-16T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO password_reset_tokens \
         (id, token_hash, user_id, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(vec![2_u8; 32])
    .bind(&user)
    .bind(&email_id)
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-16T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recovery_codes (id, user_id, code_hash, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&user)
    .bind(vec![3_u8; 32])
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
         VALUES (?, ?, ?, ?, NULL, ?, 1)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&user)
    .bind(vec![4_u8; 65])
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert_eq!(
        administration.disable_self(&user, now).await.unwrap(),
        AccountStatusChange::Changed
    );

    let disabled_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&user)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let session = sessions.validate(&issued_session.token, now).await.unwrap();
    let verification_consumed: Option<String> =
        sqlx::query_scalar("SELECT consumed_at FROM email_verification_tokens WHERE email_id = ?")
            .bind(&email_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let reset_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM password_reset_tokens WHERE user_id = ?")
            .bind(&user)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let recovery_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM recovery_codes WHERE user_id = ?")
            .bind(&user)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&user)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let active_totp_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mfa_credentials \
         WHERE user_id = ? AND activated_at IS NOT NULL",
    )
    .bind(&user)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(disabled_state, "disabled");
    assert!(session.is_none());
    assert!(verification_consumed.is_some());
    assert!(reset_invalidated.is_some());
    assert!(recovery_invalidated.is_some());
    assert_eq!(project_role, "manager");
    assert_eq!(active_totp_count, 1);

    let access = AccessStore::new(database.pool().clone());
    let disabled_context = access
        .load_project_context(&user, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(disabled_context.project_has_active_manager());
    assert!(authorize(&disabled_context, AccessCapability::MutateIssues).is_err());

    let disable_audit = sqlx::query(
        "SELECT actor_user_id, target_id FROM audit_events \
         WHERE event_kind = 'auth.user.disabled' AND target_id = ?",
    )
    .bind(&user)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        disable_audit.try_get::<String, _>("actor_user_id").unwrap(),
        user
    );
    assert_eq!(
        disable_audit.try_get::<String, _>("target_id").unwrap(),
        user
    );

    assert_eq!(
        administration
            .reactivate(&installation_admin, &user, at("2026-10-15T09:05:00Z"))
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );
    assert!(
        sessions
            .validate(&issued_session.token, at("2026-10-15T09:05:00Z"))
            .await
            .unwrap()
            .is_none()
    );
    let reactivated_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&user)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let reactivation_audit = sqlx::query(
        "SELECT actor_user_id, reason FROM audit_events \
         WHERE event_kind = 'auth.user.reactivated' AND target_id = ?",
    )
    .bind(&user)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(reactivated_state, "active");
    assert_eq!(
        reactivation_audit
            .try_get::<String, _>("actor_user_id")
            .unwrap(),
        installation_admin
    );
    assert!(
        reactivation_audit
            .try_get::<Option<String>, _>("reason")
            .unwrap()
            .is_none()
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT invalidated_at FROM password_reset_tokens WHERE user_id = ?",
        )
        .bind(&user)
        .fetch_one(database.pool())
        .await
        .unwrap()
        .is_some()
    );
}

#[tokio::test]
async fn self_deactivation_cannot_remove_the_last_owner_admin_or_project_manager() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Last owner", "owner").await;
    let installation_admin = create_user(&database, "Last installation admin", "member").await;
    let project_manager = create_user(&database, "Last project manager", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    let project_id = create_project(&database, &owner).await;
    add_project_manager(&database, &project_id, &project_manager).await;
    sqlx::query(
        "UPDATE project_members SET role = 'member' \
         WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project_id)
    .bind(&owner)
    .execute(database.pool())
    .await
    .unwrap();

    let administration = AccountAdministrationStore::new(database.pool().clone());
    let now = at("2026-10-15T09:00:00Z");
    assert!(matches!(
        administration.disable_self(&owner, now).await,
        Err(AccountAdministrationError::LastOrganizationOwner)
    ));
    assert!(matches!(
        administration.disable_self(&installation_admin, now).await,
        Err(AccountAdministrationError::LastInstallationAdmin)
    ));
    assert!(matches!(
        administration.disable_self(&project_manager, now).await,
        Err(AccountAdministrationError::LastProjectManager)
    ));

    let active_accounts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE state = 'active'")
            .fetch_one(database.pool())
            .await
            .unwrap();
    let account_status_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE event_kind IN ('auth.user.disabled', 'auth.user.reactivated')",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(active_accounts, 3);
    assert_eq!(account_status_events, 0);
}

#[tokio::test]
async fn self_deactivation_of_an_owner_is_allowed_when_another_active_owner_remains() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Current owner", "owner").await;
    let replacement_owner = create_user(&database, "Replacement owner", "admin").await;
    sqlx::query(
        "UPDATE organization_members SET role = 'owner' \
         WHERE organization_id = 1 AND user_id = ?",
    )
    .bind(&replacement_owner)
    .execute(database.pool())
    .await
    .unwrap();

    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert_eq!(
        administration
            .disable_self(&owner, at("2026-10-15T09:00:00Z"))
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );
    let owner_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&owner)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let active_owners: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM organization_members \
         JOIN users ON users.id = organization_members.user_id \
         WHERE organization_members.organization_id = 1 \
           AND organization_members.role = 'owner' AND users.state = 'active'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(owner_state, "disabled");
    assert_eq!(active_owners, 1);
}

#[tokio::test]
async fn reactivation_requires_an_active_installation_admin_and_only_accepts_disabled_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let member = create_user(&database, "Ordinary member", "member").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    let disabled_admin = create_user(&database, "Disabled installation admin", "member").await;
    let disabled = create_user(&database, "Disabled account", "member").await;
    let banned = create_user(&database, "Banned account", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    grant_installation_admin(&database, &disabled_admin).await;
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled_admin)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&banned)
        .execute(database.pool())
        .await
        .unwrap();

    let administration = AccountAdministrationStore::new(database.pool().clone());
    let now = at("2026-10-15T09:00:00Z");
    assert!(matches!(
        administration.reactivate(&member, &disabled, now).await,
        Err(AccountAdministrationError::Forbidden)
    ));
    assert!(matches!(
        administration
            .reactivate(&disabled_admin, &disabled, now)
            .await,
        Err(AccountAdministrationError::Forbidden)
    ));
    assert!(matches!(
        administration
            .reactivate(&installation_admin, &banned, now)
            .await,
        Err(AccountAdministrationError::TargetNotDisabled)
    ));
    assert_eq!(
        administration
            .reactivate(&installation_admin, &disabled, now)
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );
    assert_eq!(
        administration
            .reactivate(&installation_admin, &disabled, now)
            .await
            .unwrap(),
        AccountStatusChange::Unchanged
    );
}

#[tokio::test]
async fn audit_failures_roll_back_deactivation_credentials_and_reactivation_state() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let _owner = create_user(&database, "Organization owner", "owner").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    let user = create_user(&database, "Self-deactivating user", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    let sessions = session_store(&database);
    let now = at("2026-10-15T09:00:00Z");
    let issued_session = sessions.issue(&user, now).await.unwrap();

    sqlx::query(
        "CREATE TRIGGER reject_user_disabled_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.user.disabled' \
         BEGIN SELECT RAISE(ABORT, 'forced account disable audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert!(matches!(
        administration.disable_self(&user, now).await,
        Err(AccountAdministrationError::Audit(_))
    ));
    let active_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&user)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(active_state, "active");
    assert!(
        sessions
            .validate(&issued_session.token, now)
            .await
            .unwrap()
            .is_some()
    );

    sqlx::query("DROP TRIGGER reject_user_disabled_audit")
        .execute(database.pool())
        .await
        .unwrap();
    administration.disable_self(&user, now).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_user_reactivated_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.user.reactivated' \
         BEGIN SELECT RAISE(ABORT, 'forced account reactivate audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(matches!(
        administration
            .reactivate(&installation_admin, &user, at("2026-10-15T09:01:00Z"))
            .await,
        Err(AccountAdministrationError::Audit(_))
    ));
    let disabled_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&user)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(disabled_state, "disabled");
    assert!(
        sessions
            .validate(&issued_session.token, at("2026-10-15T09:01:00Z"))
            .await
            .unwrap()
            .is_none()
    );
}
