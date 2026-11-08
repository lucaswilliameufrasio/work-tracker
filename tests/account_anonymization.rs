use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    accounts::{AccountAdministrationError, AccountAdministrationStore, AccountStatusChange},
    auth::session::{SessionPolicy, SessionStore},
    db::Database,
    invitations::{InvitationPolicy, OrganizationInvitationStore},
    issues::{IssueStore, NewIssue},
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
         VALUES (1, 'Account anonymization test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, display_name: &str, role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', '2026-10-15T08:00:00.000000000Z', \
                 '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(display_name)
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

async fn add_email(database: &Database, user_id: &str, address: &str, is_primary: bool) -> String {
    let email_id = Uuid::now_v7().to_string();
    let address_key = address.to_ascii_lowercase();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, ?, '2026-10-15T08:00:00.000000000Z', \
                 '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(&email_id)
    .bind(user_id)
    .bind(address)
    .bind(address_key)
    .bind(i64::from(is_primary))
    .execute(database.pool())
    .await
    .unwrap();
    email_id
}

async fn insert_invitation(
    database: &Database,
    creator_user_id: &str,
    address: &str,
    token_byte: u8,
    accepted_at: Option<&str>,
    canceled_at: Option<&str>,
) -> String {
    let invite_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO organization_invites \
         (id, email, email_key, role, token_hash, created_by_user_id, \
          created_at, expires_at, accepted_at, canceled_at) \
         VALUES (?, ?, ?, 'member', ?, ?, '2026-10-15T08:00:00.000000000Z', \
                 '2026-10-22T08:00:00.000000000Z', ?, ?)",
    )
    .bind(&invite_id)
    .bind(address)
    .bind(address.to_ascii_lowercase())
    .bind(vec![token_byte; 32])
    .bind(creator_user_id)
    .bind(accepted_at)
    .bind(canceled_at)
    .execute(database.pool())
    .await
    .unwrap();
    invite_id
}

async fn create_project(database: &Database, owner_user_id: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_user_id,
            "ANON",
            "Account anonymization project",
            None,
            at("2026-10-15T08:00:00Z"),
        )
        .await
        .unwrap()
        .id
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

async fn insert_login_credentials(
    database: &Database,
    user_id: &str,
    email_id: &str,
    email_key: &str,
) -> String {
    let now = "2026-10-15T08:00:00.000000000Z";
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, '$argon2id$v=19$m=19456,t=2,p=1$synthetic$salt', ?)",
    )
    .bind(user_id)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auth_identities \
         (id, user_id, provider, issuer, subject, created_at) \
         VALUES (?, ?, 'google', 'https://issuer.example.test', ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(format!("identity-{email_key}"))
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_verification_tokens \
         (id, token_hash, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, '2026-10-16T08:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(vec![11_u8; 32])
    .bind(email_id)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO password_reset_tokens \
         (id, token_hash, user_id, email_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, '2026-10-16T08:00:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(vec![12_u8; 32])
    .bind(user_id)
    .bind(email_id)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recovery_codes (id, user_id, code_hash, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(vec![13_u8; 32])
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();

    let credential_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO mfa_credentials \
         (id, user_id, secret_envelope, created_at, expires_at, activated_at, \
          last_accepted_step) \
         VALUES (?, ?, ?, ?, NULL, ?, 1)",
    )
    .bind(&credential_id)
    .bind(user_id)
    .bind(vec![14_u8; 65])
    .bind(now)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mfa_login_challenges \
         (token_hash, user_id, email_key, credential_fingerprint, \
          totp_credential_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, '2026-10-15T08:05:00.000000000Z')",
    )
    .bind(vec![15_u8; 32])
    .bind(user_id)
    .bind(email_key)
    .bind(vec![16_u8; 32])
    .bind(&credential_id)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();

    let recovery_session_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO mfa_recovery_sessions \
         (id, token_hash, user_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, '2026-10-15T08:10:00.000000000Z')",
    )
    .bind(&recovery_session_id)
    .bind(vec![17_u8; 32])
    .bind(user_id)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mfa_recovery_enrollments \
         (credential_id, recovery_session_id, secret_envelope, created_at, expires_at) \
         VALUES (?, ?, ?, ?, '2026-10-15T08:09:00.000000000Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(recovery_session_id)
    .bind(vec![18_u8; 65])
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    credential_id
}

#[tokio::test]
async fn anonymization_erases_login_identity_but_preserves_authorship_and_scrubs_invites() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Organization owner", "owner").await;
    let target = create_user(&database, "Private display name", "owner").await;
    let replacement_manager = create_user(&database, "Replacement manager", "member").await;
    let replacement_admin =
        create_user(&database, "Replacement installation admin", "member").await;
    grant_installation_admin(&database, &target).await;
    grant_installation_admin(&database, &replacement_admin).await;

    let project_id = create_project(&database, &owner).await;
    add_project_manager(&database, &project_id, &target).await;
    add_project_manager(&database, &project_id, &replacement_manager).await;
    let issue = IssueStore::new(database.pool().clone())
        .create(
            &target,
            &project_id,
            NewIssue {
                title: "Authored before anonymization",
                description: Some("Historical issue description remains unchanged."),
                assignee_user_id: None,
            },
            at("2026-10-15T08:30:00Z"),
        )
        .await
        .unwrap();

    let primary_email = "private.person@example.test";
    let secondary_email = "alternate.person@example.test";
    let primary_email_id = add_email(&database, &target, primary_email, true).await;
    add_email(&database, &target, secondary_email, false).await;
    let pending_invite = insert_invitation(&database, &owner, primary_email, 21, None, None).await;
    let canceled_invite = insert_invitation(
        &database,
        &owner,
        primary_email,
        22,
        None,
        Some("2026-10-15T08:20:00.000000000Z"),
    )
    .await;
    let accepted_invite = insert_invitation(
        &database,
        &owner,
        secondary_email,
        23,
        Some("2026-10-15T08:25:00.000000000Z"),
        None,
    )
    .await;
    insert_login_credentials(&database, &target, &primary_email_id, primary_email).await;
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let session = sessions
        .issue(&target, at("2026-10-15T08:30:00Z"))
        .await
        .unwrap();

    let now = at("2026-10-15T09:00:00Z");
    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert_eq!(
        administration
            .anonymize(&owner, &target, now)
            .await
            .unwrap(),
        AccountStatusChange::Changed
    );

    let user = sqlx::query("SELECT id, display_name, state, anonymized_at FROM users WHERE id = ?")
        .bind(&target)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(user.try_get::<String, _>("id").unwrap(), target);
    assert_eq!(
        user.try_get::<String, _>("display_name").unwrap(),
        "Usuário removido"
    );
    assert_eq!(user.try_get::<String, _>("state").unwrap(), "disabled");
    assert!(
        user.try_get::<Option<String>, _>("anonymized_at")
            .unwrap()
            .is_some()
    );

    for (table, query, bind_user_id) in [
        (
            "user_emails",
            "SELECT COUNT(*) FROM user_emails WHERE user_id = ?",
            true,
        ),
        (
            "auth_identities",
            "SELECT COUNT(*) FROM auth_identities WHERE user_id = ?",
            true,
        ),
        (
            "password_credentials",
            "SELECT COUNT(*) FROM password_credentials WHERE user_id = ?",
            true,
        ),
        (
            "sessions",
            "SELECT COUNT(*) FROM sessions WHERE user_id = ?",
            true,
        ),
        (
            "password_reset_tokens",
            "SELECT COUNT(*) FROM password_reset_tokens WHERE user_id = ?",
            true,
        ),
        (
            "email_verification_tokens",
            "SELECT COUNT(*) FROM email_verification_tokens",
            false,
        ),
        (
            "recovery_codes",
            "SELECT COUNT(*) FROM recovery_codes WHERE user_id = ?",
            true,
        ),
        (
            "mfa_credentials",
            "SELECT COUNT(*) FROM mfa_credentials WHERE user_id = ?",
            true,
        ),
        (
            "mfa_login_challenges",
            "SELECT COUNT(*) FROM mfa_login_challenges WHERE user_id = ?",
            true,
        ),
        (
            "mfa_recovery_sessions",
            "SELECT COUNT(*) FROM mfa_recovery_sessions WHERE user_id = ?",
            true,
        ),
        (
            "mfa_recovery_enrollments",
            "SELECT COUNT(*) FROM mfa_recovery_enrollments",
            false,
        ),
        (
            "installation_admins",
            "SELECT COUNT(*) FROM installation_admins WHERE user_id = ?",
            true,
        ),
    ] {
        let remaining: i64 = if bind_user_id {
            sqlx::query_scalar(query)
                .bind(&target)
                .fetch_one(database.pool())
                .await
                .unwrap()
        } else {
            sqlx::query_scalar(query)
                .fetch_one(database.pool())
                .await
                .unwrap()
        };
        assert_eq!(remaining, 0, "{table} should be erased");
    }
    let active_admin_grants: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM installation_admins WHERE user_id = ?")
            .bind(&replacement_admin)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(active_admin_grants, 1);
    assert!(
        sessions
            .validate(&session.token, at("2026-10-15T09:00:00Z"))
            .await
            .unwrap()
            .is_none()
    );

    let organization_role: String = sqlx::query_scalar(
        "SELECT role FROM organization_members WHERE organization_id = 1 AND user_id = ?",
    )
    .bind(&target)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&target)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let issue_author: String =
        sqlx::query_scalar("SELECT creator_user_id FROM issues WHERE id = ?")
            .bind(&issue.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "owner");
    assert_eq!(project_role, "manager");
    assert_eq!(issue_author, target);

    for (invite_id, expected_status) in [
        (&pending_invite, None),
        (&canceled_invite, None),
        (&accepted_invite, Some("2026-10-15T08:25:00.000000000Z")),
    ] {
        let invite = sqlx::query(
            "SELECT email, email_key, recipient_redacted_at, accepted_at, canceled_at \
             FROM organization_invites WHERE id = ?",
        )
        .bind(invite_id)
        .fetch_one(database.pool())
        .await
        .unwrap();
        let marker = format!("redacted+{invite_id}@invalid.invalid");
        assert_eq!(invite.try_get::<String, _>("email").unwrap(), marker);
        assert_eq!(
            invite.try_get::<String, _>("email_key").unwrap(),
            format!("redacted+{invite_id}@invalid.invalid")
        );
        assert!(
            invite
                .try_get::<Option<String>, _>("recipient_redacted_at")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            invite.try_get::<Option<String>, _>("accepted_at").unwrap(),
            expected_status.map(str::to_owned)
        );
        if invite_id == &pending_invite {
            assert_eq!(
                invite.try_get::<Option<String>, _>("canceled_at").unwrap(),
                Some("2026-10-15T09:00:00.000000000Z".to_owned())
            );
        }
    }

    let invitation_store = OrganizationInvitationStore::new(
        database.pool().clone(),
        InvitationPolicy::new(true, TimeDelta::days(7)).unwrap(),
    );
    let invitation_listing = invitation_store.list(&owner, now).await.unwrap();
    assert!(
        invitation_listing
            .iter()
            .filter(|invite| {
                invite.id == pending_invite
                    || invite.id == canceled_invite
                    || invite.id == accepted_invite
            })
            .all(|invite| invite.email.is_none())
    );

    let audit = sqlx::query(
        "SELECT actor_user_id, target_id FROM audit_events \
         WHERE event_kind = 'auth.user.anonymized' AND target_id = ?",
    )
    .bind(&target)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit.try_get::<String, _>("actor_user_id").unwrap(), owner);
    assert_eq!(audit.try_get::<String, _>("target_id").unwrap(), target);

    assert_eq!(
        administration
            .anonymize(&owner, &target, at("2026-10-15T09:01:00Z"))
            .await
            .unwrap(),
        AccountStatusChange::Unchanged
    );
    assert!(matches!(
        administration
            .reactivate(&replacement_admin, &target, now)
            .await,
        Err(AccountAdministrationError::AnonymizedAccount)
    ));
    assert!(matches!(
        administration
            .ban(&replacement_admin, &target, "not allowed", now)
            .await,
        Err(AccountAdministrationError::AnonymizedAccount)
    ));
    assert!(matches!(
        administration.unban(&replacement_admin, &target, now).await,
        Err(AccountAdministrationError::AnonymizedAccount)
    ));
    let anonymization_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE event_kind = 'auth.user.anonymized' AND target_id = ?",
    )
    .bind(&target)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(anonymization_events, 1);
    for query in [
        "UPDATE users SET anonymized_at = NULL WHERE id = ?",
        "UPDATE users SET state = 'active' WHERE id = ?",
        "UPDATE users SET display_name = 'Restored identity' WHERE id = ?",
    ] {
        assert!(
            sqlx::query(query)
                .bind(&target)
                .execute(database.pool())
                .await
                .is_err(),
            "anonymized identity mutation must be rejected: {query}"
        );
    }
    let remaining_emails: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_emails WHERE user_id = ? \
         AND address IN (?, ?)",
    )
    .bind(&target)
    .bind(primary_email)
    .bind(secondary_email)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(remaining_emails, 0);
}

#[tokio::test]
async fn anonymization_accepts_banned_and_disabled_accounts() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let banned = create_user(&database, "Banned account", "member").await;
    let disabled = create_user(&database, "Disabled account", "member").await;
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&banned)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled)
        .execute(database.pool())
        .await
        .unwrap();
    let administration = AccountAdministrationStore::new(database.pool().clone());

    for target in [&banned, &disabled] {
        assert_eq!(
            administration
                .anonymize(&owner, target, at("2026-10-15T09:00:00Z"))
                .await
                .unwrap(),
            AccountStatusChange::Changed
        );
    }
    let anonymized_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE anonymized_at IS NOT NULL AND state = 'disabled'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(anonymized_count, 2);
}

#[tokio::test]
async fn anonymization_authority_is_limited_and_cannot_be_used_by_the_target() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let organization_admin = create_user(&database, "Organization admin", "admin").await;
    let target = create_user(&database, "Target account", "member").await;
    let administration = AccountAdministrationStore::new(database.pool().clone());
    let now = at("2026-10-15T09:00:00Z");

    assert!(matches!(
        administration
            .anonymize(&organization_admin, &target, now)
            .await,
        Err(AccountAdministrationError::Forbidden)
    ));
    assert!(matches!(
        administration.anonymize(&owner, &owner, now).await,
        Err(AccountAdministrationError::Forbidden)
    ));
    let target_name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = ?")
        .bind(&target)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(target_name, "Target account");
}

#[tokio::test]
async fn anonymization_requires_active_replacements_for_last_responsible_accounts() {
    let owner_directory = TempDir::new().unwrap();
    let owner_database = Database::open(owner_directory.path()).await.unwrap();
    create_organization(&owner_database).await;
    let owner_actor = create_user(&owner_database, "Installation admin", "member").await;
    let last_owner = create_user(&owner_database, "Last owner", "owner").await;
    grant_installation_admin(&owner_database, &owner_actor).await;
    let owner_administration = AccountAdministrationStore::new(owner_database.pool().clone());
    assert!(matches!(
        owner_administration
            .anonymize(&owner_actor, &last_owner, at("2026-10-15T09:00:00Z"))
            .await,
        Err(AccountAdministrationError::LastOrganizationOwner)
    ));

    let admin_directory = TempDir::new().unwrap();
    let admin_database = Database::open(admin_directory.path()).await.unwrap();
    create_organization(&admin_database).await;
    let owner_actor = create_user(&admin_database, "Owner", "owner").await;
    let last_admin = create_user(&admin_database, "Last installation admin", "member").await;
    grant_installation_admin(&admin_database, &last_admin).await;
    let admin_administration = AccountAdministrationStore::new(admin_database.pool().clone());
    assert!(matches!(
        admin_administration
            .anonymize(&owner_actor, &last_admin, at("2026-10-15T09:00:00Z"))
            .await,
        Err(AccountAdministrationError::LastInstallationAdmin)
    ));

    let manager_directory = TempDir::new().unwrap();
    let manager_database = Database::open(manager_directory.path()).await.unwrap();
    create_organization(&manager_database).await;
    let owner_actor = create_user(&manager_database, "Owner", "owner").await;
    let last_manager = create_user(&manager_database, "Last project manager", "member").await;
    let project_id = create_project(&manager_database, &owner_actor).await;
    add_project_manager(&manager_database, &project_id, &last_manager).await;
    sqlx::query(
        "UPDATE project_members SET role = 'member' \
         WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project_id)
    .bind(&owner_actor)
    .execute(manager_database.pool())
    .await
    .unwrap();
    let manager_administration = AccountAdministrationStore::new(manager_database.pool().clone());
    assert!(matches!(
        manager_administration
            .anonymize(&owner_actor, &last_manager, at("2026-10-15T09:00:00Z"))
            .await,
        Err(AccountAdministrationError::LastProjectManager)
    ));
}

#[tokio::test]
async fn anonymization_audit_failure_rolls_back_identity_invites_credentials_and_admin_grant() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", "owner").await;
    let target = create_user(&database, "Target", "member").await;
    let replacement_admin = create_user(&database, "Replacement admin", "member").await;
    grant_installation_admin(&database, &target).await;
    grant_installation_admin(&database, &replacement_admin).await;
    let address = "rollback@example.test";
    let email_id = add_email(&database, &target, address, true).await;
    let invite_id = insert_invitation(&database, &owner, address, 31, None, None).await;
    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let session = sessions
        .issue(&target, at("2026-10-15T08:30:00Z"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, '$argon2id$v=19$m=19456,t=2,p=1$synthetic$salt', \
                 '2026-10-15T08:00:00.000000000Z')",
    )
    .bind(&target)
    .execute(database.pool())
    .await
    .unwrap();

    sqlx::query(
        "CREATE TRIGGER reject_user_anonymized_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'auth.user.anonymized' \
         BEGIN SELECT RAISE(ABORT, 'forced account anonymization audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let administration = AccountAdministrationStore::new(database.pool().clone());
    assert!(matches!(
        administration
            .anonymize(&owner, &target, at("2026-10-15T09:00:00Z"))
            .await,
        Err(AccountAdministrationError::Audit(_))
    ));

    let user = sqlx::query("SELECT display_name, state, anonymized_at FROM users WHERE id = ?")
        .bind(&target)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(user.try_get::<String, _>("display_name").unwrap(), "Target");
    assert_eq!(user.try_get::<String, _>("state").unwrap(), "active");
    assert!(
        user.try_get::<Option<String>, _>("anonymized_at")
            .unwrap()
            .is_none()
    );
    let email_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_emails WHERE id = ?")
        .bind(&email_id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(email_count, 1);
    let invite = sqlx::query(
        "SELECT email, recipient_redacted_at, canceled_at \
         FROM organization_invites WHERE id = ?",
    )
    .bind(&invite_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(invite.try_get::<String, _>("email").unwrap(), address);
    assert!(
        invite
            .try_get::<Option<String>, _>("recipient_redacted_at")
            .unwrap()
            .is_none()
    );
    assert!(
        invite
            .try_get::<Option<String>, _>("canceled_at")
            .unwrap()
            .is_none()
    );
    let admin_grant_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM installation_admins WHERE user_id = ?")
            .bind(&target)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(admin_grant_count, 1);
    assert!(
        sessions
            .validate(&session.token, at("2026-10-15T09:00:00Z"))
            .await
            .unwrap()
            .is_some()
    );
}
