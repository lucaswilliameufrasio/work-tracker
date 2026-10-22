use std::{process::Command, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use puxiri::{auth::normalize_email_address, db::Database, http};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

async fn application() -> (TempDir, Arc<Database>, Router) {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let router = http::router(database.clone());
    (directory, database, router)
}

async fn request(router: &Router, method: Method, path: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

#[tokio::test]
async fn all_four_connections_have_durable_sqlite_settings_and_real_migrations() {
    let (directory, database, _router) = application().await;
    assert!(directory.path().join("puxiri.sqlite3").is_file());
    let mut connections = Vec::new();
    for _ in 0..4 {
        connections.push(database.pool().acquire().await.unwrap());
    }
    for connection in &mut connections {
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut **connection)
            .await
            .unwrap();
        assert_eq!(journal, "wal");
        for (query, expected) in [
            ("PRAGMA foreign_keys", 1_i64),
            ("PRAGMA synchronous", 2),
            ("PRAGMA busy_timeout", 5000),
        ] {
            let value: i64 = sqlx::query_scalar(query)
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            assert_eq!(value, expected, "{query}");
        }
    }
    drop(connections);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(rows, 30);
    let pending: Option<String> =
        sqlx::query_scalar("SELECT setup_completed_at FROM installation_state WHERE singleton = 1")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert!(pending.is_none());
    assert!(
        sqlx::query("INSERT INTO installation_state (singleton) VALUES (2)")
            .execute(database.pool())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn identity_foundation_schema_enforces_membership_and_admin_separation() {
    let (_directory, database, _router) = application().await;

    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Puxiri', 'pt-BR', 'America/Sao_Paulo', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
             VALUES (2, 'Outra', 'en', 'UTC', 'now', 'now')",
        )
        .execute(database.pool())
        .await
        .is_err()
    );

    for (id, role) in [
        ("01890f3e-7b1c-7cc4-98a7-123456789abc", "member"),
        ("01890f3e-7b1c-7cc4-98a7-123456789abd", "guest"),
    ] {
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, ?, 'active', 'now', 'now')",
        )
        .bind(id)
        .bind(role)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO organization_members (user_id, role, created_at) VALUES (?, ?, 'now')",
        )
        .bind(id)
        .bind(role)
        .execute(database.pool())
        .await
        .unwrap();
    }

    let member_id = "01890f3e-7b1c-7cc4-98a7-123456789abc";
    let guest_id = "01890f3e-7b1c-7cc4-98a7-123456789abd";
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(member_id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
            .bind(guest_id)
            .execute(database.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE organization_members SET role = 'guest' WHERE user_id = ?")
            .bind(member_id)
            .execute(database.pool())
            .await
            .is_err()
    );

    sqlx::query(
        "INSERT INTO auth_identities (id, user_id, provider, issuer, subject, created_at) \
         VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abe', ?, 'google', \
                 'https://accounts.google.com', 'provider-subject-1', 'now')",
    )
    .bind(member_id)
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO auth_identities (id, user_id, provider, issuer, subject, created_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abf', ?, 'google', \
                     'https://accounts.google.com', 'provider-subject-1', 'now')",
        )
        .bind(guest_id)
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn credential_schema_limits_password_hash_format_and_session_digest_shape() {
    let (_directory, database, _router) = application().await;
    let user_id = "01890f3e-7b1c-7cc4-98a7-123456789abc";
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Test user', 'active', 'now', 'now')",
    )
    .bind(user_id)
    .execute(database.pool())
    .await
    .unwrap();

    assert!(
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, 'not-a-password-hash', 'now')",
        )
        .bind(user_id)
        .execute(database.pool())
        .await
        .is_err()
    );
    sqlx::query(
        "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
         VALUES (?, '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA', 'now')",
    )
    .bind(user_id)
    .execute(database.pool())
    .await
    .unwrap();

    let session_id = "01890f3e-7b1c-7cc4-98a7-123456789abd";
    let token_hash = vec![7_u8; 32];
    sqlx::query(
        "INSERT INTO sessions \
         (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
         VALUES (?, ?, ?, 'now', 'now', 'later')",
    )
    .bind(session_id)
    .bind(&token_hash)
    .bind(user_id)
    .execute(database.pool())
    .await
    .unwrap();

    assert!(
        sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abe', ?, ?, 'now', 'now', 'later')",
        )
        .bind(&token_hash)
        .bind(user_id)
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abf', ?, ?, 'now', 'now', 'later')",
        )
        .bind(vec![8_u8; 31])
        .bind(user_id)
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn user_emails_are_unique_by_canonical_key_and_primary_requires_confirmation() {
    let (_directory, database, _router) = application().await;
    let user_ids = [
        "01890f3e-7b1c-7cc4-98a7-123456789abc",
        "01890f3e-7b1c-7cc4-98a7-123456789abd",
    ];
    for user_id in user_ids {
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'Test user', 'active', 'now', 'now')",
        )
        .bind(user_id)
        .execute(database.pool())
        .await
        .unwrap();
    }

    let email = normalize_email_address(" Alice.Example+work@EXAMPLE.COM ").unwrap();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, created_at) \
         VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abe', ?, ?, ?, 'now')",
    )
    .bind(user_ids[0])
    .bind(&email.address)
    .bind(&email.key)
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, created_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abf', ?, \
                     'different@example.com', 'unrelated@example.com', 'now')",
        )
        .bind(user_ids[1])
        .execute(database.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, created_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abf', ?, \
                     'alice.example+work@example.com', ?, 'now')",
        )
        .bind(user_ids[1])
        .bind(&email.key)
        .execute(database.pool())
        .await
        .is_err()
    );

    assert!(
        sqlx::query(
            "UPDATE user_emails SET is_primary = 1 \
             WHERE id = '01890f3e-7b1c-7cc4-98a7-123456789abe'",
        )
        .execute(database.pool())
        .await
        .is_err()
    );
    sqlx::query(
        "UPDATE user_emails SET confirmed_at = 'now', is_primary = 1 \
         WHERE id = '01890f3e-7b1c-7cc4-98a7-123456789abe'",
    )
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES ('01890f3e-7b1c-7cc4-98a7-123456789abf', ?, \
                     'alternate@example.com', 'alternate@example.com', 1, 'now', 'now')",
        )
        .bind(user_ids[0])
        .execute(database.pool())
        .await
        .is_err()
    );
}

#[tokio::test]
async fn data_persists_and_migrations_do_not_repeat_after_restart() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    sqlx::query("CREATE TABLE persistence_probe (value TEXT NOT NULL)")
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO persistence_probe (value) VALUES (?)")
        .bind("persisted value")
        .execute(database.pool())
        .await
        .unwrap();
    database.close().await;
    drop(database);
    let database = Database::open(directory.path()).await.unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM persistence_probe")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(value, "persisted value");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM installation_state")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
async fn tampered_migration_checksum_prevents_startup() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1")
        .execute(database.pool())
        .await
        .unwrap();
    database.close().await;
    drop(database);
    let error = Database::open(directory.path())
        .await
        .err()
        .expect("tampered migration must prevent startup");
    assert!(
        matches!(error, puxiri::db::OpenError::Migration(_)),
        "unexpected startup error: {error:?}"
    );
}

#[tokio::test]
async fn directory_lock_survives_router_ownership_and_rejects_another_process() {
    let (directory, database, router) = application().await;
    assert!(matches!(
        Database::open(directory.path()).await,
        Err(puxiri::db::OpenError::AlreadyRunning)
    ));
    drop(database);
    let output = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args(["serve", "--bind", "127.0.0.1:0", "--data-dir"])
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("AlreadyRunning"));
    assert_eq!(
        request(&router, Method::GET, "/health/ready")
            .await
            .status(),
        StatusCode::OK
    );
    drop(router);
    let database = Database::open(directory.path()).await.unwrap();
    database.close().await;
}

#[tokio::test]
async fn invalid_data_directory_fails_without_starting_a_server() {
    let directory = TempDir::new().unwrap();
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, "existing content").unwrap();
    assert!(matches!(
        Database::open(&file).await,
        Err(puxiri::db::OpenError::Filesystem(_))
    ));
    assert_eq!(std::fs::read_to_string(file).unwrap(), "existing content");
}

#[tokio::test]
async fn bundled_sqlite_supports_fts5() {
    let (_directory, database, _router) = application().await;
    let mut connection = database.pool().acquire().await.unwrap();
    sqlx::query("CREATE VIRTUAL TABLE temp.search_probe USING fts5(body)")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO temp.search_probe (body) VALUES (?)")
        .bind("trabalho em movimento")
        .execute(&mut *connection)
        .await
        .unwrap();
    let found: i64 =
        sqlx::query_scalar("SELECT count(*) FROM temp.search_probe WHERE body MATCH ?")
            .bind("movimento")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
    assert_eq!(found, 1);
}

#[tokio::test]
async fn health_is_minimal_and_readiness_reflects_real_database_failure() {
    let (_directory, database, router) = application().await;
    let response = request(&router, Method::GET, "/health/ready").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(json_body(response).await, json!({"status": "ready"}));
    assert_eq!(
        json_body(request(&router, Method::GET, "/health/live").await).await,
        json!({"status": "live"})
    );
    database.close().await;
    let response = request(&router, Method::GET, "/health/ready").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(response).await, json!({"status": "not_ready"}));
    assert_eq!(
        request(&router, Method::GET, "/health/live").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn spa_and_built_assets_are_embedded_with_correct_cache_and_security_headers() {
    let (_directory, _database, router) = application().await;
    for path in ["/", "/index.html", "/projects/example"] {
        let response = request(&router, Method::GET, path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            if path.starts_with("/projects/") {
                "no-store"
            } else {
                "no-cache"
            },
            "{path}"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(
            response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }
    for method in [Method::GET, Method::HEAD] {
        for path in [
            "/settings/sessions",
            "/projects",
            "/projects/019a0000-0000-7000-8000-000000000001/issues",
            "/projects/019a0000-0000-7000-8000-000000000001/issues/1",
        ] {
            let response = request(&router, method.clone(), path).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            if method == Method::HEAD {
                assert!(
                    to_bytes(response.into_body(), 1024 * 1024)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    let response = request(&router, Method::GET, "/").await;
    let html = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("<title>Puxiri</title>"));
    let asset = html
        .split("src=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(asset.starts_with("/assets/"));
    let response = request(&router, Method::GET, asset).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(
        !to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .is_empty()
    );
    let response = request(&router, Method::HEAD, asset).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn nonexistent_endpoints_and_assets_do_not_fall_back_to_successful_html() {
    let (_directory, _database, router) = application().await;
    for path in [
        "/api/v1/projects",
        "/mcp",
        "/setup",
        "/admin/settings",
        "/auth/login",
        "/health/unknown",
    ] {
        for method in [Method::GET, Method::POST] {
            let response = request(&router, method, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body = json_body(response).await;
            assert_eq!(body["code"], "not_found");
            assert_eq!(body["retryable"], false);
            assert_eq!(
                uuid::Uuid::parse_str(body["request_id"].as_str().unwrap())
                    .unwrap()
                    .get_version_num(),
                7
            );
        }
    }
    for path in [
        "/assets/missing.js",
        "/assets/missing",
        "/assets/../Cargo.toml",
        "/assets/%2e%2e/Cargo.toml",
    ] {
        assert_eq!(
            request(&router, Method::GET, path).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = request(&router, Method::POST, "/projects").await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
}

#[test]
fn cli_exposes_version_and_rejects_invalid_bind_before_creating_data() {
    let directory = TempDir::new().unwrap();
    let version = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("puxiri 0.1.0"));
    let data = directory.path().join("unused");
    let output = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args(["serve", "--bind", "invalid", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!data.exists());
}

#[test]
fn setup_token_cli_refuses_non_terminal_output_before_creating_data() {
    let directory = TempDir::new().unwrap();
    let data = directory.path().join("setup-data");
    let output = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args(["setup-token", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("One-time setup token"));
    assert!(!data.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn serve_creates_a_private_default_master_key_before_binding() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let data = directory.path().join("serve-data");
    let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let occupied_address = occupied_listener.local_addr().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args([
            "serve",
            "--bind",
            &occupied_address.to_string(),
            "--data-dir",
        ])
        .arg(&data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let key_path = data.join("master.key");
    let metadata = std::fs::metadata(&key_path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(key_path).unwrap().len(), 32);

    let custom_data = directory.path().join("custom-data");
    let custom_key = directory.path().join("custom-master.key");
    let output = Command::new(env!("CARGO_BIN_EXE_puxiri"))
        .args([
            "serve",
            "--bind",
            &occupied_address.to_string(),
            "--data-dir",
        ])
        .arg(&custom_data)
        .arg("--master-key")
        .arg(&custom_key)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(custom_key.is_file());
    assert!(!custom_data.join("master.key").exists());
}
