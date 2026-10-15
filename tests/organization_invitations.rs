use chrono::{DateTime, TimeDelta, Utc};
use puxiri::{
    access::ProjectRole,
    auth::{
        PasswordService,
        credentials::CredentialStore,
        reauthentication::{ReauthenticationService, RecentReauthentication},
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    invitations::{
        InvitationIssueResult, InvitationPolicy, InvitationProjectAssignment,
        OrganizationInvitationError, OrganizationInvitationStatus, OrganizationInvitationStore,
        OrganizationInviteRole,
    },
    projects::{ProjectStore, membership::ProjectMembershipStore},
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

const PASSWORD: &str = "a valid invitation-test password";
static INVITATION_TEST_KDF_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Organization invitations test', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(
    database: &Database,
    name: &str,
    organization_role: Option<&str>,
    email: Option<(&str, bool)>,
) -> String {
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

    if let Some(role) = organization_role {
        sqlx::query(
            "INSERT INTO organization_members \
             (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')",
        )
        .bind(&user_id)
        .bind(role)
        .execute(database.pool())
        .await
        .unwrap();
    }

    if let Some((address, confirmed)) = email {
        let confirmed_at = if confirmed {
            Some("2026-10-15T08:00:00.000000000Z")
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES (?, ?, ?, lower(?), ?, ?, '2026-10-15T08:00:00.000000000Z')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&user_id)
        .bind(address)
        .bind(address)
        .bind(if confirmed { 1_i64 } else { 0_i64 })
        .bind(confirmed_at)
        .execute(database.pool())
        .await
        .unwrap();
    }

    user_id
}

async fn create_project(database: &Database, owner_id: &str, key: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            key,
            "Organization invitation test project",
            None,
            at("2026-10-15T08:00:00Z"),
        )
        .await
        .unwrap()
        .id
}

async fn make_reauthentication(
    database: &Database,
    user_id: &str,
    now: DateTime<Utc>,
) -> RecentReauthentication {
    let password_service = PasswordService::default();
    let password_hash = password_service.hash(PASSWORD.to_owned()).await.unwrap();
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, ?, ?)",
    )
    .bind(user_id)
    .bind(&password_hash)
    .bind("2026-10-15T08:00:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let sessions = SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let session = sessions.issue(user_id, now).await.unwrap();
    ReauthenticationService::new(
        CredentialStore::new(database.pool().clone(), password_service),
        sessions,
        password_hash,
    )
    .reauthenticate_password(&session.token, PASSWORD.to_owned(), now)
    .await
    .unwrap()
    .unwrap()
}

fn invitation_store(database: &Database) -> OrganizationInvitationStore {
    OrganizationInvitationStore::new(database.pool().clone(), InvitationPolicy::initial())
}

fn invitation_sessions(database: &Database) -> SessionStore {
    SessionStore::new(
        database.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    )
}

#[tokio::test]
async fn organization_admin_can_issue_reissue_list_and_cancel_hash_only_invites() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let project_id = create_project(&database, &owner, "INVT").await;
    let invitations = invitation_store(&database);
    let now = at("2026-10-15T09:00:00Z");

    let first = invitations
        .issue(
            &owner,
            " Future.User@Example.Test ",
            OrganizationInviteRole::Member,
            &[InvitationProjectAssignment {
                project_id: project_id.clone(),
                role: ProjectRole::Manager,
            }],
            now,
        )
        .await
        .unwrap();
    assert_eq!(first.result, InvitationIssueResult::Created);
    assert_eq!(first.expires_at, now + TimeDelta::days(7));

    let stored = sqlx::query(
        "SELECT email, email_key, token_hash, accepted_at, canceled_at \
         FROM organization_invites WHERE id = ?",
    )
    .bind(&first.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        stored.try_get::<String, _>("email").unwrap(),
        "Future.User@Example.Test"
    );
    assert_eq!(
        stored.try_get::<String, _>("email_key").unwrap(),
        "future.user@example.test"
    );
    assert_eq!(
        stored.try_get::<Vec<u8>, _>("token_hash").unwrap().len(),
        32
    );
    assert!(
        stored
            .try_get::<Option<String>, _>("accepted_at")
            .unwrap()
            .is_none()
    );
    assert!(
        stored
            .try_get::<Option<String>, _>("canceled_at")
            .unwrap()
            .is_none()
    );

    let second = invitations
        .issue(
            &owner,
            "future.user@example.test",
            OrganizationInviteRole::Guest,
            &[InvitationProjectAssignment {
                project_id: project_id.clone(),
                role: ProjectRole::Member,
            }],
            now + TimeDelta::minutes(5),
        )
        .await
        .unwrap();
    assert_eq!(second.result, InvitationIssueResult::Reissued);
    assert_ne!(first.token.expose(), second.token.expose());

    let listings = invitations
        .list(&owner, now + TimeDelta::minutes(6))
        .await
        .unwrap();
    assert_eq!(listings.len(), 2);
    let old = listings
        .iter()
        .find(|invite| invite.id == first.id)
        .unwrap();
    let current = listings
        .iter()
        .find(|invite| invite.id == second.id)
        .unwrap();
    assert_eq!(old.status, OrganizationInvitationStatus::Canceled);
    assert_eq!(current.status, OrganizationInvitationStatus::Pending);
    assert_eq!(current.projects.len(), 1);
    assert_eq!(current.projects[0].role, ProjectRole::Member);

    invitations
        .cancel(&owner, &second.id, now + TimeDelta::minutes(7))
        .await
        .unwrap();
    let listings = invitations
        .list(&owner, now + TimeDelta::minutes(8))
        .await
        .unwrap();
    assert!(
        listings
            .iter()
            .all(|invite| { invite.status == OrganizationInvitationStatus::Canceled })
    );

    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE event_kind IN ( \
             'organization.invite.created', 'organization.invite.reissued', \
             'organization.invite.canceled' \
         )",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(event_count, 4);
}

#[tokio::test]
async fn organization_invites_require_valid_project_assignments_and_reject_existing_members() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let member = create_user(
        &database,
        "Existing member",
        Some("member"),
        Some(("existing@example.test", true)),
    )
    .await;
    let project_id = create_project(&database, &owner, "RULE").await;
    let invitations = invitation_store(&database);
    let now = at("2026-10-15T09:00:00Z");

    assert!(matches!(
        invitations
            .issue(
                &owner,
                "guest@example.test",
                OrganizationInviteRole::Guest,
                &[],
                now,
            )
            .await,
        Err(OrganizationInvitationError::GuestRequiresProject)
    ));
    assert!(matches!(
        invitations
            .issue(
                &owner,
                "duplicate@example.test",
                OrganizationInviteRole::Member,
                &[
                    InvitationProjectAssignment {
                        project_id: project_id.clone(),
                        role: ProjectRole::Member,
                    },
                    InvitationProjectAssignment {
                        project_id: project_id.clone(),
                        role: ProjectRole::Viewer,
                    },
                ],
                now,
            )
            .await,
        Err(OrganizationInvitationError::DuplicateProject)
    ));
    assert!(matches!(
        invitations
            .issue(
                &owner,
                "EXISTING@example.test",
                OrganizationInviteRole::Member,
                &[],
                now,
            )
            .await,
        Err(OrganizationInvitationError::TargetAlreadyMember)
    ));

    let member_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id = ?")
            .bind(member)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(member_count, 1);
    let invite_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organization_invites")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(invite_count, 0);
}

#[tokio::test]
async fn invite_schema_protects_guest_scope_immutable_assignments_and_audit_targets() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let first_project = create_project(&database, &owner, "GUEST").await;
    let second_project = create_project(&database, &owner, "SCOPE").await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "guest@example.test",
            OrganizationInviteRole::Guest,
            &[InvitationProjectAssignment {
                project_id: first_project,
                role: ProjectRole::Member,
            }],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();

    assert!(
        sqlx::query(
            "INSERT INTO organization_invite_projects \
         (invite_id, project_id, organization_id, role) VALUES (?, ?, 1, 'manager')",
        )
        .bind(&issued.id)
        .bind(second_project)
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE organization_invites SET email = 'changed@example.test' WHERE id = ?")
            .bind(&issued.id)
            .execute(database.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM organization_invites WHERE id = ?")
            .bind(&issued.id)
            .execute(database.pool())
            .await
            .is_err()
    );

    assert!(
        sqlx::query(
            "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, 'organization.invite.created', 'success', ?, 'user', ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&owner)
        .bind(Uuid::now_v7().to_string())
        .bind("2026-10-15T09:01:00.000000000Z")
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn project_manager_can_invite_only_guests_to_projects_they_manage() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let manager = create_user(&database, "Manager", Some("member"), None).await;
    let ordinary_member = create_user(&database, "Ordinary member", Some("member"), None).await;
    let managed_project = create_project(&database, &owner, "MGRS").await;
    let other_project = create_project(&database, &owner, "OTHR").await;
    ProjectMembershipStore::new(database.pool().clone())
        .set_role(
            &owner,
            &managed_project,
            &manager,
            ProjectRole::Manager,
            at("2026-10-15T08:30:00Z"),
        )
        .await
        .unwrap();
    let invitations = invitation_store(&database);

    let issued = invitations
        .issue(
            &manager,
            "guest@example.test",
            OrganizationInviteRole::Guest,
            &[InvitationProjectAssignment {
                project_id: managed_project.clone(),
                role: ProjectRole::Member,
            }],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    assert_eq!(issued.result, InvitationIssueResult::Created);
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let registered = invitations
        .register(
            &token,
            "Invited guest",
            "a valid guest registration password".to_owned(),
            &PasswordService::default(),
            &invitation_sessions(&database),
            at("2026-10-15T09:00:30Z"),
        )
        .await
        .unwrap();
    let invited_guest = registered.user_id;
    let guest_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&invited_guest)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(guest_role, "guest");
    let guest_project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&managed_project)
            .bind(&invited_guest)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(guest_project_role, "member");
    assert!(matches!(
        invitations
            .list(&ordinary_member, at("2026-10-15T09:00:30Z"))
            .await,
        Err(OrganizationInvitationError::Forbidden)
    ));
    assert!(matches!(
        invitations
            .cancel(&ordinary_member, &issued.id, at("2026-10-15T09:00:45Z"))
            .await,
        Err(OrganizationInvitationError::Forbidden)
    ));
    assert!(matches!(
        invitations
            .issue(
                &ordinary_member,
                "unauthorized@example.test",
                OrganizationInviteRole::Guest,
                &[InvitationProjectAssignment {
                    project_id: managed_project.clone(),
                    role: ProjectRole::Member,
                }],
                at("2026-10-15T09:00:50Z"),
            )
            .await,
        Err(OrganizationInvitationError::Forbidden)
    ));

    assert!(matches!(
        invitations
            .issue(
                &manager,
                "member@example.test",
                OrganizationInviteRole::Member,
                &[InvitationProjectAssignment {
                    project_id: managed_project.clone(),
                    role: ProjectRole::Member,
                }],
                at("2026-10-15T09:01:00Z"),
            )
            .await,
        Err(OrganizationInvitationError::Forbidden)
    ));
    assert!(matches!(
        invitations
            .issue(
                &manager,
                "other-project@example.test",
                OrganizationInviteRole::Guest,
                &[InvitationProjectAssignment {
                    project_id: other_project,
                    role: ProjectRole::Member,
                }],
                at("2026-10-15T09:02:00Z"),
            )
            .await,
        Err(OrganizationInvitationError::Forbidden)
    ));
    assert!(matches!(
        invitations
            .issue(
                &manager,
                "manager-guest@example.test",
                OrganizationInviteRole::Guest,
                &[InvitationProjectAssignment {
                    project_id: managed_project,
                    role: ProjectRole::Manager,
                }],
                at("2026-10-15T09:03:00Z"),
            )
            .await,
        Err(OrganizationInvitationError::GuestCannotBeProjectManager)
    ));
}

#[tokio::test]
async fn accepting_invite_requires_a_confirmed_matching_email_and_applies_roles_atomically() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitee = create_user(
        &database,
        "Invitee",
        None,
        Some(("other@example.test", true)),
    )
    .await;
    let project_id = create_project(&database, &owner, "ACPT").await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "invitee@example.test",
            OrganizationInviteRole::Member,
            &[InvitationProjectAssignment {
                project_id: project_id.clone(),
                role: ProjectRole::Viewer,
            }],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let now = at("2026-10-15T09:01:00Z");
    let proof = make_reauthentication(&database, &invitee, now).await;

    assert!(matches!(
        invitations
            .accept(
                &proof,
                &token,
                now + TimeDelta::minutes(10) + TimeDelta::seconds(1),
            )
            .await,
        Err(OrganizationInvitationError::ReauthenticationRequired)
    ));
    assert!(matches!(
        invitations.accept(&proof, &token, now).await,
        Err(OrganizationInvitationError::EmailNotConfirmed)
    ));
    let accepted_at: Option<String> =
        sqlx::query_scalar("SELECT accepted_at FROM organization_invites WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(accepted_at.is_none());

    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, 'invitee@example.test', 'invitee@example.test', 0, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&invitee)
    .bind("2026-10-15T09:02:00.000000000Z")
    .bind("2026-10-15T09:02:00.000000000Z")
    .execute(database.pool())
    .await
    .unwrap();

    let acceptance = invitations
        .accept(&proof, &token, at("2026-10-15T09:03:00Z"))
        .await
        .unwrap();
    assert_eq!(acceptance.invitation_id, issued.id);
    assert_eq!(acceptance.role, OrganizationInviteRole::Member);
    assert_eq!(acceptance.projects.len(), 1);

    let organization_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&invitee)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&invitee)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "member");
    assert_eq!(project_role, "viewer");

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE event_kind IN ( \
             'organization.invite.accepted', 'organization.member.added', \
             'project.member.added' \
         ) AND actor_user_id = ?",
    )
    .bind(&invitee)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(audit_count, 3);
    assert!(matches!(
        invitations
            .accept(&proof, &token, at("2026-10-15T09:04:00Z"))
            .await,
        Err(OrganizationInvitationError::InvitationAlreadyAccepted)
    ));
}

#[tokio::test]
async fn invite_only_registration_creates_confirmed_account_memberships_and_session_atomically() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let project_id = create_project(&database, &owner, "SIGN").await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "New.User+work@Example.Test",
            OrganizationInviteRole::Member,
            &[InvitationProjectAssignment {
                project_id: project_id.clone(),
                role: ProjectRole::Viewer,
            }],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let password_service = PasswordService::default();
    let sessions = invitation_sessions(&database);
    let now = at("2026-10-15T09:01:00Z");

    let registered = invitations
        .register(
            &token,
            "  New User  ",
            "a valid registration password".to_owned(),
            &password_service,
            &sessions,
            now,
        )
        .await
        .unwrap();
    assert_eq!(registered.invitation_id, issued.id);
    assert_eq!(registered.role, OrganizationInviteRole::Member);
    assert_eq!(registered.projects.len(), 1);

    let account = sqlx::query(
        "SELECT users.display_name, users.state, user_emails.address, \
                user_emails.address_key, user_emails.is_primary, user_emails.confirmed_at, \
                password_credentials.password_hash \
         FROM users \
         JOIN user_emails ON user_emails.user_id = users.id \
         JOIN password_credentials ON password_credentials.user_id = users.id \
         WHERE users.id = ?",
    )
    .bind(&registered.user_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        account.try_get::<String, _>("display_name").unwrap(),
        "New User"
    );
    assert_eq!(account.try_get::<String, _>("state").unwrap(), "active");
    assert_eq!(
        account.try_get::<String, _>("address").unwrap(),
        "New.User+work@Example.Test"
    );
    assert_eq!(
        account.try_get::<String, _>("address_key").unwrap(),
        "new.user+work@example.test"
    );
    assert_eq!(account.try_get::<i64, _>("is_primary").unwrap(), 1);
    assert_eq!(
        account
            .try_get::<Option<String>, _>("confirmed_at")
            .unwrap()
            .as_deref(),
        Some("2026-10-15T09:01:00.000000000Z")
    );
    let password_hash = account.try_get::<String, _>("password_hash").unwrap();
    assert!(password_hash.starts_with("$argon2id$"));
    assert!(
        password_service
            .verify("a valid registration password".to_owned(), password_hash,)
            .await
            .unwrap()
    );

    let organization_role: String =
        sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = ?")
            .bind(&registered.user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&registered.user_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(organization_role, "member");
    assert_eq!(project_role, "viewer");
    let authenticated = sessions
        .validate(&registered.session.token, now)
        .await
        .unwrap();
    assert_eq!(
        authenticated.map(|session| session.user_id),
        Some(registered.user_id.clone())
    );

    let account_audit_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE actor_user_id = ? \
         AND event_kind IN ( \
             'auth.email.address_added', 'auth.email.address_confirmed', \
             'auth.password.changed', 'organization.invite.accepted', \
             'organization.member.added', 'project.member.added' \
         )",
    )
    .bind(&registered.user_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(account_audit_events, 6);

    assert!(matches!(
        invitations
            .register(
                &token,
                "Another Name",
                "a valid registration password".to_owned(),
                &password_service,
                &sessions,
                now,
            )
            .await,
        Err(OrganizationInvitationError::InvitationAlreadyAccepted)
    ));
}

#[tokio::test]
async fn invite_registration_rejects_existing_accounts_and_invalid_fields_without_consuming_invite()
{
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let existing_user = create_user(
        &database,
        "Existing account",
        None,
        Some(("existing@example.test", true)),
    )
    .await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "existing@example.test",
            OrganizationInviteRole::Member,
            &[],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let unused_issued = invitations
        .issue(
            &owner,
            "unused@example.test",
            OrganizationInviteRole::Member,
            &[],
            at("2026-10-15T09:00:30Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let unused_token =
        puxiri::invitations::OrganizationInvitationToken::parse(unused_issued.token.expose())
            .unwrap();
    let password_service = PasswordService::default();
    let sessions = invitation_sessions(&database);
    let now = at("2026-10-15T09:01:00Z");

    assert!(matches!(
        invitations
            .register(
                &token,
                "New Duplicate",
                "a valid registration password".to_owned(),
                &password_service,
                &sessions,
                now,
            )
            .await,
        Err(OrganizationInvitationError::AccountAlreadyExists)
    ));
    assert!(matches!(
        invitations
            .register(
                &unused_token,
                "   ",
                "a valid registration password".to_owned(),
                &password_service,
                &sessions,
                now,
            )
            .await,
        Err(OrganizationInvitationError::InvalidDisplayName)
    ));
    assert!(matches!(
        invitations
            .register(
                &unused_token,
                "Valid Name",
                "short".to_owned(),
                &password_service,
                &sessions,
                now,
            )
            .await,
        Err(OrganizationInvitationError::Password(_))
    ));

    let accepted_at: Option<String> =
        sqlx::query_scalar("SELECT accepted_at FROM organization_invites WHERE id = ?")
            .bind(&unused_issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert!(accepted_at.is_none());
    assert_eq!(users, 2);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id = ?")
            .bind(&existing_user)
            .fetch_one(database.pool())
            .await
            .unwrap(),
        existing_user
    );
}

#[tokio::test]
async fn registration_audit_failure_rolls_back_account_invite_memberships_and_session() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "rollback-registration@example.test",
            OrganizationInviteRole::Member,
            &[],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_invite_registration_audit \
         BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'organization.invite.accepted' \
         BEGIN SELECT RAISE(ABORT, 'forced invite registration audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let sessions = invitation_sessions(&database);

    assert!(matches!(
        invitations
            .register(
                &token,
                "Rollback User",
                "a valid registration password".to_owned(),
                &PasswordService::default(),
                &sessions,
                at("2026-10-15T09:01:00Z"),
            )
            .await,
        Err(OrganizationInvitationError::Audit(_))
    ));

    let accepted_at: Option<String> =
        sqlx::query_scalar("SELECT accepted_at FROM organization_invites WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let registered_users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id != ?")
        .bind(&owner)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let registrations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_emails \
         WHERE address_key = 'rollback-registration@example.test'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    let sessions_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert!(accepted_at.is_none());
    assert_eq!(registered_users, 0);
    assert_eq!(registrations, 0);
    assert_eq!(sessions_count, 0);
}

#[tokio::test]
async fn acceptance_rejects_expired_tokens_and_disabled_policy_blocks_issue_and_acceptance() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitee = create_user(
        &database,
        "Invitee",
        None,
        Some(("expired@example.test", true)),
    )
    .await;
    let short_policy = InvitationPolicy::new(true, TimeDelta::hours(1)).unwrap();
    let invitations = OrganizationInvitationStore::new(database.pool().clone(), short_policy);
    let issued_at = at("2026-10-15T09:00:00Z");
    let issued = invitations
        .issue(
            &owner,
            "expired@example.test",
            OrganizationInviteRole::Member,
            &[],
            issued_at,
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let expired_at = issued_at + TimeDelta::hours(1);
    let proof = make_reauthentication(&database, &invitee, expired_at).await;
    assert!(matches!(
        invitations.accept(&proof, &token, expired_at).await,
        Err(OrganizationInvitationError::InvitationExpired)
    ));
    let listing = invitations.list(&owner, expired_at).await.unwrap();
    assert_eq!(listing[0].status, OrganizationInvitationStatus::Expired);

    let disabled = OrganizationInvitationStore::new(
        database.pool().clone(),
        InvitationPolicy::new(false, TimeDelta::days(7)).unwrap(),
    );
    assert!(matches!(
        disabled
            .issue(
                &owner,
                "another@example.test",
                OrganizationInviteRole::Member,
                &[],
                expired_at,
            )
            .await,
        Err(OrganizationInvitationError::Disabled)
    ));
    assert!(matches!(
        disabled.accept(&proof, &token, expired_at).await,
        Err(OrganizationInvitationError::Disabled)
    ));
    assert!(matches!(
        disabled
            .register(
                &token,
                "Invitee",
                "a valid registration password".to_owned(),
                &PasswordService::default(),
                &invitation_sessions(&database),
                expired_at,
            )
            .await,
        Err(OrganizationInvitationError::Disabled)
    ));
}

#[tokio::test]
async fn invitation_audit_failure_rolls_back_consumption_and_all_memberships() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitee = create_user(
        &database,
        "Invitee",
        None,
        Some(("rollback@example.test", true)),
    )
    .await;
    let project_id = create_project(&database, &owner, "RBCK").await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "rollback@example.test",
            OrganizationInviteRole::Member,
            &[InvitationProjectAssignment {
                project_id: project_id.clone(),
                role: ProjectRole::Member,
            }],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let now = at("2026-10-15T09:01:00Z");
    let proof = make_reauthentication(&database, &invitee, now).await;
    sqlx::query(
        "CREATE TRIGGER reject_organization_member_added BEFORE INSERT ON audit_events \
         WHEN NEW.event_kind = 'organization.member.added' \
         BEGIN SELECT RAISE(ABORT, 'forced invite acceptance audit failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    assert!(matches!(
        invitations.accept(&proof, &token, now).await,
        Err(OrganizationInvitationError::Audit(_))
    ));
    let accepted_at: Option<String> =
        sqlx::query_scalar("SELECT accepted_at FROM organization_invites WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let organization_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id = ?")
            .bind(&invitee)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let project_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_members WHERE user_id = ?")
            .bind(&invitee)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(accepted_at.is_none());
    assert_eq!(organization_count, 0);
    assert_eq!(project_count, 0);
}

#[tokio::test]
async fn acceptance_rechecks_account_state_and_session_revocation() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitee = create_user(
        &database,
        "Invitee",
        None,
        Some(("current-session@example.test", true)),
    )
    .await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "current-session@example.test",
            OrganizationInviteRole::Member,
            &[],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let now = at("2026-10-15T09:01:00Z");
    let proof = make_reauthentication(&database, &invitee, now).await;

    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&invitee)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        invitations.accept(&proof, &token, now).await,
        Err(OrganizationInvitationError::UserNotActive)
    ));
    sqlx::query("UPDATE users SET state = 'active' WHERE id = ?")
        .bind(&invitee)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE user_id = ?")
        .bind("2026-10-15T09:02:00.000000000Z")
        .bind(&invitee)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        invitations
            .accept(&proof, &token, at("2026-10-15T09:02:00Z"))
            .await,
        Err(OrganizationInvitationError::ReauthenticationRequired)
    ));

    let accepted_at: Option<String> =
        sqlx::query_scalar("SELECT accepted_at FROM organization_invites WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(accepted_at.is_none());
}

#[tokio::test]
async fn concurrent_acceptance_consumes_an_invitation_once() {
    let _guard = INVITATION_TEST_KDF_LOCK.lock().await;
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "Owner", Some("owner"), None).await;
    let invitee = create_user(
        &database,
        "Invitee",
        None,
        Some(("race@example.test", true)),
    )
    .await;
    let invitations = invitation_store(&database);
    let issued = invitations
        .issue(
            &owner,
            "race@example.test",
            OrganizationInviteRole::Member,
            &[],
            at("2026-10-15T09:00:00Z"),
        )
        .await
        .unwrap();
    let token =
        puxiri::invitations::OrganizationInvitationToken::parse(issued.token.expose()).unwrap();
    let now = at("2026-10-15T09:01:00Z");
    let proof = make_reauthentication(&database, &invitee, now).await;

    let first = invitations.accept(&proof, &token, now);
    let second = invitations.accept(&proof, &token, now);
    let (first, second) = tokio::join!(first, second);
    match (first, second) {
        (Ok(_), Err(OrganizationInvitationError::InvitationAlreadyAccepted))
        | (Err(OrganizationInvitationError::InvitationAlreadyAccepted), Ok(_)) => {}
        results => panic!("unexpected concurrent invite acceptances: {results:?}"),
    }

    let member_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM organization_members WHERE user_id = ?")
            .bind(&invitee)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let acceptance_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'organization.invite.accepted'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(member_count, 1);
    assert_eq!(acceptance_events, 1);
}
