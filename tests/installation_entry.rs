use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        PasswordService, SessionToken,
        entry::{
            BuiltinSetup, EntryError, InstallationEntry, LoginSource, MuiraInstallation,
            MuiraLoginOutcome, VerifiedMuiraIdentity,
        },
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
        setup::{SetupToken, SetupTokenStore},
    },
    db::Database,
    invitations::{InvitationPolicy, OrganizationInvitationStore, OrganizationInviteRole},
};
use tempfile::TempDir;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

fn muira(issuer: &str) -> MuiraInstallation<'_> {
    MuiraInstallation {
        organization_name: "Puxiri",
        locale: "pt-BR",
        timezone: "America/Sao_Paulo",
        display_name: "Ada",
        email: "ada@example.test",
        issuer,
        client_id: "puxiri",
        client_secret: "muira-test-secret",
        redirect_uri: "https://tracker.example.test/api/v1/auth/muira/callback",
    }
}

fn builtin(password: &str) -> BuiltinSetup<'_> {
    BuiltinSetup {
        organization_name: "Puxiri",
        locale: "en",
        timezone: "UTC",
        display_name: "Ada",
        email: "ada@example.test",
        password: password.to_owned(),
    }
}

async fn harness() -> (TempDir, Database, InstallationEntry) {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let entry = InstallationEntry::new(
        database.pool().clone(),
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
        PasswordService::default(),
        SessionStore::new(database.pool().clone(), policy()),
    );
    (directory, database, entry)
}

fn state_from(url: &str) -> String {
    let query = url.split_once('?').expect("authorization url").1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("state=") {
            return value.to_owned();
        }
    }
    panic!("authorization url has no state");
}

fn identity(subject: &str, email: Option<&str>, verified: bool) -> VerifiedMuiraIdentity {
    VerifiedMuiraIdentity {
        issuer: "https://muira.example.test".to_owned(),
        subject: subject.to_owned(),
        email: email.map(str::to_owned),
        email_verified: verified,
    }
}

macro_rules! count {
    ($database:expr, $sql:literal) => {
        sqlx::query_scalar::<_, i64>($sql)
            .fetch_one($database.pool())
            .await
            .unwrap()
    };
}

#[tokio::test]
async fn muira_setup_login_requires_membership_or_an_invitation() {
    let (_directory, database, entry) = harness().await;
    let now = Utc::now();
    let status = entry.status().await.unwrap();
    assert!(status.setup_required);
    assert!(status.login.is_none());
    let token = SetupTokenStore::new(database.pool().clone())
        .issue(now)
        .await
        .unwrap();
    let unknown = SetupToken::parse(SessionToken::generate().unwrap().expose()).unwrap();
    let Err(rejected) = entry
        .begin_muira_setup(&unknown, muira("https://muira.example.test"), now)
        .await
    else {
        panic!("unknown setup token must be rejected");
    };
    assert!(matches!(rejected, EntryError::SetupUnavailable));
    assert_eq!(count!(&database, "SELECT COUNT(*) FROM users"), 0);

    let started = entry
        .begin_muira_setup(&token, muira("https://muira.example.test/"), now)
        .await
        .unwrap();
    let Err(wrong) = entry
        .finish_muira_callback(
            &state_from(&started.url),
            &identity("owner-subject", Some("other@example.test"), true),
            now,
        )
        .await
    else {
        panic!("wrong email must be rejected");
    };
    assert!(matches!(wrong, EntryError::IdentityRejected));
    assert_eq!(count!(&database, "SELECT COUNT(*) FROM users"), 0);
    assert!(
        SetupTokenStore::new(database.pool().clone())
            .is_current(&token)
            .await
            .unwrap()
    );

    let started = entry
        .begin_muira_setup(&token, muira("https://muira.example.test"), now)
        .await
        .unwrap();
    let session = match entry
        .finish_muira_callback(
            &state_from(&started.url),
            &identity("owner-subject", Some("Ada@Example.test"), true),
            now,
        )
        .await
        .unwrap()
    {
        MuiraLoginOutcome::Session(session) => session,
        MuiraLoginOutcome::InvitationRequired => panic!("owner setup must issue a session"),
    };
    assert_eq!(session.token.expose().len(), 43);
    let status = entry.status().await.unwrap();
    assert!(!status.setup_required);
    assert!(matches!(status.login, Some(LoginSource::Muira)));
    assert!(
        !SetupTokenStore::new(database.pool().clone())
            .is_current(&token)
            .await
            .unwrap()
    );
    assert_eq!(count!(&database, "SELECT COUNT(*) FROM users"), 1);
    assert_eq!(
        count!(&database, "SELECT COUNT(*) FROM password_credentials"),
        0
    );
    assert_eq!(
        count!(
            &database,
            "SELECT COUNT(*) FROM auth_identities WHERE provider = 'muira'"
        ),
        1
    );
    let source: String = sqlx::query_scalar("SELECT source FROM installation_login")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(source, "muira");

    let stranger = entry
        .begin_muira_login(
            "https://tracker.example.test/api/v1/auth/muira/callback",
            now,
        )
        .await
        .unwrap();
    let outcome = entry
        .finish_muira_callback(
            &state_from(&stranger.url),
            &identity("stranger-subject", Some("stranger@example.test"), true),
            now,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, MuiraLoginOutcome::InvitationRequired));
    assert_eq!(count!(&database, "SELECT COUNT(*) FROM users"), 1);

    let owner: String = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(database.pool())
        .await
        .unwrap();
    OrganizationInvitationStore::new(database.pool().clone(), InvitationPolicy::initial())
        .issue(
            &owner,
            "guest@example.test",
            OrganizationInviteRole::Member,
            &[],
            now,
        )
        .await
        .unwrap();
    let invited = entry
        .begin_muira_login(
            "https://tracker.example.test/api/v1/auth/muira/callback",
            now,
        )
        .await
        .unwrap();
    let invited_session = match entry
        .finish_muira_callback(
            &state_from(&invited.url),
            &identity("guest-subject", Some("guest@example.test"), true),
            now,
        )
        .await
        .unwrap()
    {
        MuiraLoginOutcome::Session(session) => session,
        MuiraLoginOutcome::InvitationRequired => panic!("matching invite must issue a session"),
    };
    assert_eq!(invited_session.token.expose().len(), 43);
    assert_eq!(count!(&database, "SELECT COUNT(*) FROM users"), 2);
    assert_eq!(
        count!(&database, "SELECT COUNT(*) FROM organization_members"),
        2
    );

    let repeat = entry
        .begin_muira_login(
            "https://tracker.example.test/api/v1/auth/muira/callback",
            now,
        )
        .await
        .unwrap();
    assert!(matches!(
        entry
            .finish_muira_callback(
                &state_from(&repeat.url),
                &identity("guest-subject", Some("guest@example.test"), true),
                now,
            )
            .await
            .unwrap(),
        MuiraLoginOutcome::Session(_)
    ));
}

#[tokio::test]
async fn built_in_setup_and_password_login_share_one_credential() {
    let (_directory, database, entry) = harness().await;
    let now = Utc::now();
    let token = SetupTokenStore::new(database.pool().clone())
        .issue(now)
        .await
        .unwrap();
    let session = entry
        .complete_puxiri_setup(&token, builtin("puxiri-entry-passphrase"), now)
        .await
        .unwrap();
    assert_eq!(session.token.expose().len(), 43);
    let again = entry
        .login_puxiri(
            "Ada@Example.test",
            "puxiri-entry-passphrase".to_owned(),
            now,
        )
        .await
        .unwrap();
    assert_ne!(again.token.expose(), session.token.expose());
    let Err(rejected_password) = entry
        .login_puxiri("ada@example.test", "wrong-passphrase-value".to_owned(), now)
        .await
    else {
        panic!("wrong password must be rejected");
    };
    assert!(matches!(rejected_password, EntryError::LoginUnavailable));
    let Err(second_setup) = entry
        .complete_puxiri_setup(&token, builtin("puxiri-entry-passphrase"), now)
        .await
    else {
        panic!("setup must not run twice");
    };
    assert!(matches!(second_setup, EntryError::SetupUnavailable));
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM password_credentials")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
}
