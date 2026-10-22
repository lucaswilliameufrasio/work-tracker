use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::Arc,
};

use chrono::{TimeDelta, Utc};
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    x509::{X509, X509NameBuilder, extension::SubjectAlternativeName},
};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

struct BrowserChild(std::process::Child);

impl Drop for BrowserChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn certificate(directory: &std::path::Path) {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "Puxiri loopback test only")
        .unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    std::fs::write(
        directory.join("certificate.pem"),
        cert.build().to_pem().unwrap(),
    )
    .unwrap();
    let key_path = directory.join("key.pem");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(key_path)
        .unwrap()
        .write_all(&key.private_key_to_pem_pkcs8().unwrap())
        .unwrap();
}

async fn user(database: &Database) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic browser user', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn session_screen_uses_real_https_browser_cookies_confirmations_and_transactional_api() {
    let directory = TempDir::new().unwrap();
    certificate(directory.path());
    let database = Arc::new(
        Database::open(&directory.path().join("data"))
            .await
            .unwrap(),
    );
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_dir = directory.path().to_path_buf();
    let artifacts = std::env::var_os("PUXIRI_UI_SCREENSHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| directory.path().join("screenshots"));
    std::fs::create_dir_all(&artifacts).unwrap();
    let (mut child, origin, diagnostic_reader) = tokio::task::spawn_blocking(move || {
        let mut child = BrowserChild(
            Command::new("node")
                .arg(root.join("web/scripts/session-screen.integration.mjs"))
                .env("PUXIRI_TEST_TLS_DIR", fixture_dir)
                .env("PUXIRI_TEST_BACKEND", format!("http://{backend}"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = child.0.stderr.take().unwrap();
        let diagnostic_reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut diagnostic = String::new();
            stderr
                .take(64 * 1024)
                .read_to_string(&mut diagnostic)
                .unwrap();
            diagnostic
        });
        let stdout = child.0.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ready: Value = serde_json::from_str(&line)
            .expect("TLS fixture must announce only its loopback origin");
        let origin = ready["origin"].as_str().unwrap().to_owned();
        (child, origin, diagnostic_reader)
    })
    .await
    .unwrap();
    assert!(origin.starts_with("https://127.0.0.1:"));
    let security = BrowserSessionSecurity::new(
        &origin,
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let app = http::router_with_browser_sessions(database.clone(), security, policy);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut fixtures = Vec::new();
    let mut owners = Vec::new();
    for kind in [
        "uncertain",
        "global",
        "current",
        "native-retry",
        "lifecycle",
    ] {
        let actor = user(&database).await;
        let own = sessions.issue(&actor, Utc::now()).await.unwrap();
        let other = sessions.issue(&actor, Utc::now()).await.unwrap();
        for _ in 0..20 {
            sessions
                .issue(&actor, Utc::now() - TimeDelta::days(8))
                .await
                .unwrap();
        }
        fixtures.push(json!({"kind": kind, "token": own.token.expose(), "current_id": own.id, "other_id": other.id}));
        owners.push((
            actor,
            if matches!(kind, "current" | "lifecycle") {
                2_i64
            } else {
                22_i64
            },
        ));
    }
    let stranger = user(&database).await;
    let foreign = sessions.issue(&stranger, Utc::now()).await.unwrap();
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Read UI fixture', 'en', 'UTC', 'now', 'now')").execute(database.pool()).await.unwrap();
    let reader = user(&database).await;
    for (actor, role) in [(&reader, "owner"), (&stranger, "admin")] {
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')").bind(actor).bind(role).execute(database.pool()).await.unwrap();
    }
    let project = puxiri::projects::ProjectStore::new(database.pool().clone())
        .create(&reader, "READ", "Synthetic work project", None, Utc::now())
        .await
        .unwrap();
    let archived = puxiri::projects::ProjectStore::new(database.pool().clone())
        .create(
            &reader,
            "OLD",
            "Synthetic archived project",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    puxiri::projects::ProjectStore::new(database.pool().clone())
        .set_archived(&reader, &archived.id, true, Utc::now())
        .await
        .unwrap();
    let private = puxiri::projects::ProjectStore::new(database.pool().clone())
        .create(
            &stranger,
            "HIDE",
            "Private outsider project",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    for number in 1..=23 {
        puxiri::issues::IssueStore::new(database.pool().clone()).create(&reader, &project.id, puxiri::issues::NewIssue {
            title: &format!("Synthetic work issue {number}"),
            description: Some("# Private Markdown\n<script>throw new Error('not executable')</script>\n![remote](https://forbidden.example.test/private.png)"),
            assignee_user_id: None,
        }, Utc::now()).await.unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE project_id = ? AND issue_number = 2")
        .bind(Utc::now().to_rfc3339())
        .bind(&project.id)
        .execute(database.pool())
        .await
        .unwrap();
    let mut reference_status = String::new();
    let mut reference_type = String::new();
    for index in 0..101 {
        reference_status = Uuid::now_v7().to_string();
        reference_type = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO statuses (id, project_id, name, category, position, board_visible, archived_at, created_at, updated_at) VALUES (?, ?, ?, 'backlog', ?, 0, ?, 'now', 'now')")
            .bind(&reference_status).bind(&project.id).bind(if index == 100 { "Referenced status beyond page100".to_owned() } else { format!("Custom status {index}") })
            .bind(i64::from(index) + 10).bind(Utc::now().to_rfc3339()).execute(database.pool()).await.unwrap();
        sqlx::query("INSERT INTO issue_types (id, project_id, name, archived_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'now', 'now')")
            .bind(&reference_type).bind(&project.id).bind(if index == 100 { "Referenced type beyond page100".to_owned() } else { format!("Custom type {index}") })
            .bind(Utc::now().to_rfc3339()).execute(database.pool()).await.unwrap();
    }
    sqlx::query("UPDATE issues SET status_id = ?, issue_type_id = ? WHERE project_id = ? AND issue_number = 23")
        .bind(&reference_status).bind(&reference_type).bind(&project.id).execute(database.pool()).await.unwrap();
    for (query, reference) in [
        (
            "SELECT COUNT(*) FROM statuses WHERE project_id = ? AND id < ?",
            &reference_status,
        ),
        (
            "SELECT COUNT(*) FROM issue_types WHERE project_id = ? AND id < ?",
            &reference_type,
        ),
    ] {
        let preceding: i64 = sqlx::query_scalar(query)
            .bind(&project.id)
            .bind(reference)
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert!(preceding >= 100);
    }
    for kind in ["work-light", "work-dark"] {
        let issued = sessions.issue(&reader, Utc::now()).await.unwrap();
        fixtures.push(json!({"kind": kind, "token": issued.token.expose(), "project_id": project.id, "private_project_id": private.id}));
    }
    // Read/display fixture only; no production editing command is enabled.
    let journal_issue: String =
        sqlx::query_scalar("SELECT id FROM issues WHERE project_id = ? AND issue_number = 23")
            .bind(&project.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    for (index, kind) in [
        "issue.title_updated",
        "issue.description_updated",
        "issue.text_updated",
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(Uuid::now_v7().to_string()).bind(&project.id).bind(&journal_issue).bind(&reader).bind(kind)
            .bind((Utc::now() + TimeDelta::seconds(index as i64 + 1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)).execute(database.pool()).await.unwrap();
    }
    let mut creation_projects = Vec::new();
    for (kind, key) in [("create-light", "NEW"), ("create-dark", "DARK")] {
        let creator = user(&database).await;
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'admin', 'now')").bind(&creator).execute(database.pool()).await.unwrap();
        let created = puxiri::projects::ProjectStore::new(database.pool().clone())
            .create(&creator, key, "Creation UI fixture", None, Utc::now())
            .await
            .unwrap();
        let issued = sessions.issue(&creator, Utc::now()).await.unwrap();
        sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')").bind(&archived.id).bind(&creator).execute(database.pool()).await.unwrap();
        fixtures.push(json!({"kind": kind, "token": issued.token.expose(), "project_id": created.id, "private_project_id": private.id, "read_only_project_id": archived.id}));
        creation_projects.push(created.id);
    }
    let input = json!({"fixtures": fixtures, "artifacts": artifacts});
    let mut stdin = child.0.stdin.take().unwrap();
    serde_json::to_writer(&mut stdin, &input).unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    let result = tokio::task::spawn_blocking(move || {
        let status = child.0.wait().unwrap();
        (child, status, diagnostic_reader.join().unwrap())
    })
    .await
    .unwrap();
    let (_child, status, mut diagnostic) = result;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    if !status.success() {
        for fixture in input["fixtures"].as_array().unwrap() {
            diagnostic = diagnostic.replace(
                fixture["token"].as_str().unwrap(),
                "[redacted synthetic session]",
            );
        }
        panic!("isolated browser fixture failed: {diagnostic}");
    }
    for (actor, expected) in owners {
        let revoked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NOT NULL",
        )
        .bind(&actor)
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(revoked, expected);
        let audits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE actor_user_id = ?")
                .bind(actor)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(audits, 2);
    }
    let global: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.sessions.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(global, 3);
    let reader_revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NOT NULL",
    )
    .bind(&reader)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let reader_audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE actor_user_id = ? AND event_kind = 'auth.session.revoked'").bind(&reader).fetch_one(database.pool()).await.unwrap();
    assert_eq!((reader_revoked, reader_audits), (2, 2));
    let issue_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let issue_events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!((issue_count, issue_events), (29, 38));
    for project in creation_projects {
        let counts: (i64, i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM issues WHERE project_id = ?), (SELECT COUNT(*) FROM issue_events WHERE project_id = ?), (SELECT COUNT(*) FROM outbox_events WHERE project_id = ?), (SELECT COUNT(*) FROM issue_creation_receipts WHERE project_id = ?), next_issue_number FROM projects WHERE id = ?")
            .bind(&project).bind(&project).bind(&project).bind(&project).bind(&project).fetch_one(database.pool()).await.unwrap();
        assert_eq!(counts, (3, 6, 6, 3, 4));
        let edits: (i64, String, i64) = sqlx::query_as("SELECT version, title, (SELECT COUNT(*) FROM issue_text_receipts WHERE project_id = ?) FROM issues WHERE project_id = ? AND issue_number = 1").bind(&project).bind(&project).fetch_one(database.pool()).await.unwrap();
        assert_eq!(edits, (4, "Recovered edit".to_owned(), 3));
    }
    assert!(
        sessions
            .validate(&foreign.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
}
