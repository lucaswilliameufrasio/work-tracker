use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    access::{AccessCapability, AccessStore, ProjectRole, authorize},
    accounts::{AccountAdministrationError, AccountAdministrationStore, AccountStatusChange},
    auth::session::{SessionPolicy, SessionStore},
    db::Database,
    issues::{IssueCreationError, IssueStore, NewIssue},
    projects::{ProjectStore, membership::ProjectMembershipStore},
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
         VALUES (1, 'Account administration test', 'en', 'UTC', 'now', 'now')",
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

async fn create_project(database: &Database, owner_id: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            "BAN",
            "Ban lifecycle project",
            None,
            at("2026-10-15T08:00:00Z"),
        )
        .await
        .unwrap()
        .id
}

async fn add_project_member(database: &Database, project_id: &str, user_id: &str, role: &str) {
    sqlx::query(
        "INSERT INTO project_members \
         (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, ?, '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
}

fn session_store(database: &Database) -> SessionStore {
    SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    )
}

#[tokio::test]
async fn ban_revokes_existing_credentials_and_managerless_projects_are_read_only_until_repaired() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner manager", "owner").await;
    let org_admin = create_user(&database, "Organization admin", "admin").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    let member = create_user(&database, "Project member", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    let project_id = create_project(&database, &owner).await;
    add_project_member(&database, &project_id, &member, "member").await;
    let now = at("2026-10-15T09:00:00Z");
    let sessions = session_store(&database);
    let issued_session = sessions.issue(&owner, now).await.unwrap();
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, 'owner@example.test', 'owner@example.test', 1, ?, ?)",
    )
    .bind(&email_id)
    .bind(&owner)
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
    .bind(&owner)
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
    .bind(&owner)
    .bind(vec![3_u8; 32])
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let mfa_credential_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
         VALUES (?, ?, ?, ?, NULL, ?, 1)",
    )
    .bind(&mfa_credential_id)
    .bind(&owner)
    .bind(vec![4_u8; 65])
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mfa_login_challenges \
         (token_hash, user_id, email_key, credential_fingerprint, totp_credential_id, \
          created_at, expires_at) \
         VALUES (?, ?, 'owner@example.test', ?, ?, ?, ?)",
    )
    .bind(vec![5_u8; 32])
    .bind(&owner)
    .bind(vec![6_u8; 32])
    .bind(&mfa_credential_id)
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:05:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let recovery_session_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO mfa_recovery_sessions \
         (id, token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&recovery_session_id)
    .bind(vec![7_u8; 32])
    .bind(&owner)
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:10:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mfa_recovery_enrollments \
         (credential_id, recovery_session_id, secret_envelope, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&recovery_session_id)
    .bind(vec![8_u8; 65])
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:10:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let pending_mfa_user = create_user(&database, "Pending MFA user", "member").await;
    sqlx::query(
        "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&pending_mfa_user)
    .bind(vec![9_u8; 65])
    .bind("2026-10-15T08:00:00.000000000Z")
    .bind("2026-10-15T08:10:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let issue_store = IssueStore::new(database.pool().clone());
    issue_store
        .create(
            &member,
            &project_id,
            NewIssue {
                title: "Before manager ban",
                description: None,
                assignee_user_id: None,
            },
            now,
        )
        .await
        .unwrap();

    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert_eq!(
        administration
            .ban(
                &installation_admin,
                &owner,
                "  confirmed security incident  ",
                now,
            )
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );
    assert_eq!(
        administration
            .ban(
                &installation_admin,
                &owner,
                "repeated operation is idempotent",
                now,
            )
            .await
            .unwrap(),
        AccountStatusChange::Unchanged
    );
    assert_eq!(
        administration
            .ban(
                &installation_admin,
                &pending_mfa_user,
                "pending enrollment is no longer trusted",
                now,
            )
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );

    let account_state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&owner)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(account_state, "banned");
    let access = AccessStore::new(database.pool().clone());
    let member_context = access
        .load_project_context(&member, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(!member_context.project_has_active_manager());
    assert!(authorize(&member_context, AccessCapability::ReadProjectContent).is_ok());
    assert!(authorize(&member_context, AccessCapability::MutateIssues).is_err());
    assert!(matches!(
        issue_store
            .create(
                &member,
                &project_id,
                NewIssue {
                    title: "While managerless",
                    description: None,
                    assignee_user_id: None,
                },
                now,
            )
            .await,
        Err(IssueCreationError::Forbidden)
    ));

    let memberships = ProjectMembershipStore::new(database.pool().clone());
    assert!(matches!(
        memberships
            .set_role(&org_admin, &project_id, &member, ProjectRole::Viewer, now,)
            .await,
        Err(puxiri::projects::membership::ProjectMembershipError::ActiveManagerRequired)
    ));
    assert_eq!(
        memberships
            .set_role(&org_admin, &project_id, &member, ProjectRole::Manager, now,)
            .await
            .unwrap(),
        puxiri::projects::membership::ProjectMembershipChange::RoleChanged
    );
    let restored_context = access
        .load_project_context(&member, &project_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(restored_context.project_has_active_manager());
    issue_store
        .create(
            &member,
            &project_id,
            NewIssue {
                title: "After manager replacement",
                description: None,
                assignee_user_id: None,
            },
            now,
        )
        .await
        .unwrap();

    let session = sessions.validate(&issued_session.token, now).await.unwrap();
    assert!(session.is_none());
    let revoked_at: Option<String> =
        sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE id = ?")
            .bind(&issued_session.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(revoked_at.is_some());

    let verification_consumed: Option<String> = sqlx::query_scalar(
        "SELECT consumed_at FROM email_verification_tokens \
         WHERE email_id = ?",
    )
    .bind(&email_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let reset_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM password_reset_tokens WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let recovery_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM recovery_codes WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let recovery_session_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM mfa_recovery_sessions WHERE id = ?")
            .bind(&recovery_session_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let challenge_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mfa_login_challenges WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let recovery_enrollment_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mfa_recovery_enrollments WHERE recovery_session_id = ?",
    )
    .bind(&recovery_session_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let active_mfa_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mfa_credentials WHERE user_id = ? AND activated_at IS NOT NULL",
    )
    .bind(&owner)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let pending_mfa_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mfa_credentials WHERE user_id = ? AND activated_at IS NULL",
    )
    .bind(&pending_mfa_user)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert!(verification_consumed.is_some());
    assert!(reset_invalidated.is_some());
    assert!(recovery_invalidated.is_some());
    assert!(recovery_session_invalidated.is_some());
    assert_eq!(challenge_count, 0);
    assert_eq!(recovery_enrollment_count, 0);
    assert_eq!(active_mfa_count, 1);
    assert_eq!(pending_mfa_count, 0);

    let audit = sqlx::query(
        "SELECT event_kind, reason FROM audit_events \
         WHERE target_kind = 'user' AND target_id = ? \
           AND event_kind = 'auth.user.banned'",
    )
    .bind(&owner)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        audit.try_get::<String, _>("event_kind").unwrap(),
        "auth.user.banned"
    );
    assert_eq!(
        audit.try_get::<String, _>("reason").unwrap(),
        "confirmed security incident"
    );

    assert_eq!(
        administration
            .unban(&installation_admin, &owner, at("2026-10-15T09:05:00Z"))
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
    let unban_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE target_id = ? \
         AND event_kind = 'auth.user.unbanned'",
    )
    .bind(&owner)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(unban_events, 1);
}

#[tokio::test]
async fn ban_requires_installation_admin_and_a_valid_reason_without_changing_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let member = create_user(&database, "Ordinary member", "member").await;
    let disabled = create_user(&database, "Disabled member", "member").await;
    let installation_admin = create_user(&database, "Installation admin", "member").await;
    grant_installation_admin(&database, &installation_admin).await;
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled)
        .execute(database.pool())
        .await
        .unwrap();
    let administration = AccountAdministrationStore::new(database.pool().clone());
    let now = at("2026-10-15T09:00:00Z");

    assert!(matches!(
        administration
            .ban(&member, &member, "ordinary member", now)
            .await,
        Err(AccountAdministrationError::Forbidden)
    ));
    assert!(matches!(
        administration
            .ban(&installation_admin, &member, "   ", now)
            .await,
        Err(AccountAdministrationError::InvalidReason)
    ));
    assert!(matches!(
        administration
            .ban(&installation_admin, &member, &"x".repeat(501), now,)
            .await,
        Err(AccountAdministrationError::InvalidReason)
    ));
    assert!(matches!(
        administration
            .ban(&installation_admin, &member, "multi\nline", now)
            .await,
        Err(AccountAdministrationError::InvalidReason)
    ));
    assert!(matches!(
        administration
            .ban(&installation_admin, &disabled, "disabled already", now)
            .await,
        Err(AccountAdministrationError::TargetNotActive)
    ));
    assert!(matches!(
        administration
            .unban(&installation_admin, &disabled, now)
            .await,
        Err(AccountAdministrationError::TargetNotBanned)
    ));

    let active_states: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE state = 'active'")
            .fetch_one(database.pool())
            .await
            .unwrap();
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(active_states, 2);
    assert_eq!(event_count, 0);
}

#[tokio::test]
async fn audit_failure_rolls_back_account_session_and_one_time_token_changes() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let actor = create_user(&database, "Installation admin", "member").await;
    let target = create_user(&database, "Target account", "member").await;
    grant_installation_admin(&database, &actor).await;
    let sessions = session_store(&database);
    let now = at("2026-10-15T09:00:00Z");
    let issued_session = sessions.issue(&target, now).await.unwrap();
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, 'rollback@example.test', 'rollback@example.test', 1, ?, ?)",
    )
    .bind(&email_id)
    .bind(&target)
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
    .bind(vec![11_u8; 32])
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
    .bind(vec![12_u8; 32])
    .bind(&target)
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
    .bind(&target)
    .bind(vec![13_u8; 32])
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_user_ban_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.user.banned' \
         BEGIN SELECT RAISE(ABORT, 'forced account audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(matches!(
        AccountAdministrationStore::new(database.pool().clone())
            .ban(&actor, &target, "verified incident", now)
            .await,
        Err(AccountAdministrationError::Audit(_))
    ));
    let state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&target)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let revoked_at: Option<String> =
        sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE id = ?")
            .bind(&issued_session.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let verification_consumed: Option<String> =
        sqlx::query_scalar("SELECT consumed_at FROM email_verification_tokens WHERE email_id = ?")
            .bind(&email_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let reset_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM password_reset_tokens WHERE user_id = ?")
            .bind(&target)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let recovery_invalidated: Option<String> =
        sqlx::query_scalar("SELECT invalidated_at FROM recovery_codes WHERE user_id = ?")
            .bind(&target)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(state, "active");
    assert!(revoked_at.is_none());
    assert!(verification_consumed.is_none());
    assert!(reset_invalidated.is_none());
    assert!(recovery_invalidated.is_none());
    assert!(
        sessions
            .validate(&issued_session.token, now)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_last_installation_admin_can_be_banned_and_requires_an_external_recovery_path() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let only_admin = create_user(&database, "Last installation admin", "member").await;
    grant_installation_admin(&database, &only_admin).await;
    let administration = AccountAdministrationStore::new(database.pool().clone());

    assert_eq!(
        administration
            .ban(
                &only_admin,
                &only_admin,
                "local recovery boundary test",
                at("2026-10-15T09:00:00Z"),
            )
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );
    assert!(matches!(
        administration
            .unban(&only_admin, &only_admin, at("2026-10-15T09:01:00Z"),)
            .await,
        Err(AccountAdministrationError::Forbidden)
    ));
    let state: String = sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
        .bind(&only_admin)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(state, "banned");
}
