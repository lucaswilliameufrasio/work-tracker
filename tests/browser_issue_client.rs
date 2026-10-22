use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    issues::{IssueStore, NewIssue},
    projects::ProjectStore,
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn typescript_issue_client_reads_real_router_and_sqlite_without_writes_or_private_access() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Client fixture', 'en', 'UTC', 'now', 'now')").execute(database.pool()).await.unwrap();
    let owner = Uuid::now_v7().to_string();
    let stranger = Uuid::now_v7().to_string();
    for (id, role) in [(&owner, "owner"), (&stranger, "admin")] {
        sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic issue client', 'active', 'now', 'now')").bind(id).execute(database.pool()).await.unwrap();
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')").bind(id).bind(role).execute(database.pool()).await.unwrap();
    }
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "API", "Client fixture", None, Utc::now())
        .await
        .unwrap();
    let issues = IssueStore::new(database.pool().clone());
    let escaped = "\0".repeat(65536);
    for title in ["First", "Trashed", "Third"] {
        issues
            .create(
                &owner,
                &project.id,
                NewIssue {
                    title,
                    description: Some(&escaped),
                    assignee_user_id: None,
                },
                Utc::now(),
            )
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE project_id = ? AND issue_number = 2")
        .bind(Utc::now().to_rfc3339())
        .bind(&project.id)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET next_issue_number = 9007199254740993 WHERE id = ?")
        .bind(&project.id)
        .execute(database.pool())
        .await
        .unwrap();
    let large = issues
        .create(
            &owner,
            &project.id,
            NewIssue {
                title: "Large number",
                description: None,
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET version = 9223372036854775807 WHERE id = ?")
        .bind(&large.id)
        .execute(database.pool())
        .await
        .unwrap();
    ProjectStore::new(database.pool().clone())
        .set_archived(&owner, &project.id, true, Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET archived_at = ?, position = 9223372036854775807 WHERE project_id = ? AND system_key = 'backlog'")
        .bind(Utc::now().to_rfc3339()).bind(&project.id).execute(database.pool()).await.unwrap();
    sqlx::query("UPDATE issue_types SET archived_at = ?, icon_key = 'opaque_icon', color_key = 'opaque-color' WHERE project_id = ?")
        .bind(Utc::now().to_rfc3339()).bind(&project.id).execute(database.pool()).await.unwrap();
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy);
    let own = sessions.issue(&owner, Utc::now()).await.unwrap();
    let foreign = sessions.issue(&stranger, Utc::now()).await.unwrap();
    let expired = sessions
        .issue(&owner, Utc::now() - TimeDelta::days(8))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let security = BrowserSessionSecurity::new(
        &format!("https://{address}"),
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let app = http::router_with_browser_sessions(database.clone(), security, policy);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("node")
            .arg(root.join("web/scripts/issue-client.integration.mjs"))
            .env("PUXIRI_TEST_BASE_URL", format!("http://{address}"))
            .env("PUXIRI_TEST_PROJECT", project.id)
            .env("PUXIRI_TEST_SESSION", own.token.expose())
            .env("PUXIRI_TEST_FOREIGN_SESSION", foreign.token.expose())
            .env("PUXIRI_TEST_EXPIRED_SESSION", expired.token.expose())
            .output()
    })
    .await
    .unwrap()
    .unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert!(
        output.status.success(),
        "Node issue client fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("SELECT version FROM issues WHERE id = ?")
        .bind(large.id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!((count, events, version), (4, 4, i64::MAX));
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(revoked, 0);
}
