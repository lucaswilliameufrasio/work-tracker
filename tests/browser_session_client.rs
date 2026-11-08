use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
};
use tempfile::TempDir;
use uuid::Uuid;

async fn user(database: &Database) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic client fixture', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn typescript_session_client_runs_against_real_loopback_router_and_sqlite() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let actor = user(&database).await;
    let stranger = user(&database).await;
    let logout_user = user(&database).await;
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy);
    let own = sessions.issue(&actor, Utc::now()).await.unwrap();
    let target = sessions.issue(&actor, Utc::now()).await.unwrap();
    let foreign = sessions.issue(&stranger, Utc::now()).await.unwrap();
    let logout = sessions.issue(&logout_user, Utc::now()).await.unwrap();
    let expired = sessions
        .issue(&actor, Utc::now() - TimeDelta::days(8))
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
            .arg(root.join("web/scripts/session-client.integration.mjs"))
            .env("PUXIRI_TEST_BASE_URL", format!("http://{address}"))
            .env("PUXIRI_TEST_SESSION", own.token.expose())
            .env("PUXIRI_TEST_CURRENT_ID", &own.id)
            .env("PUXIRI_TEST_USER", actor)
            .env("PUXIRI_TEST_TARGET", &target.id)
            .env("PUXIRI_TEST_LOGOUT_SESSION", logout.token.expose())
            .output()
    })
    .await
    .unwrap()
    .unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert!(
        output.status.success(),
        "Node client fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(revoked, 4); // current, selected target, expired owned row, logout user
    let individual: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.session.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    let global: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'auth.sessions.revoked'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!((individual, global), (2, 1));
    assert!(
        sessions
            .validate(&foreign.token, Utc::now())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions
            .validate(&expired.token, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
}
