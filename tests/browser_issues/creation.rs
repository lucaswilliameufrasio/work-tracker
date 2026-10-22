use super::*;
use axum::http::HeaderValue;

impl Fixture {
    async fn post(&self, key: &str, body: &str) -> Request<Body> {
        let session = json(
            self.get("/api/v1/auth/session", Some(&self.token.token))
                .await,
            StatusCode::OK,
        )
        .await;
        let mut req = request(&self.path(""), Some(&self.token.token));
        *req.method_mut() = Method::POST;
        req.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://tracker.example.test"),
        );
        req.headers_mut().insert(
            "x-csrf-token",
            session["csrf_token"].as_str().unwrap().parse().unwrap(),
        );
        req.headers_mut()
            .insert("idempotency-key", key.parse().unwrap());
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        *req.body_mut() = Body::from(body.to_owned());
        req
    }

    async fn write_counts(&self) -> (i64, i64, i64, i64, i64) {
        sqlx::query_as("SELECT (SELECT COUNT(*) FROM issues), (SELECT COUNT(*) FROM issue_events), (SELECT COUNT(*) FROM outbox_events), (SELECT COUNT(*) FROM issue_creation_receipts), next_issue_number FROM projects WHERE id = ?")
            .bind(&self.project).fetch_one(self.db.pool()).await.unwrap()
    }
}

#[tokio::test]
async fn post_commits_content_free_receipt_replays_and_conflicts_without_duplicate_events() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    sqlx::query("UPDATE projects SET next_issue_number = 9007199254740993 WHERE id = ?")
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    let body = r#"{"title":" Private <script>title</script> ","description":"Private text"}"#;
    let created = f
        .app
        .clone()
        .oneshot(f.post(&key, body).await)
        .await
        .unwrap();
    assert_eq!(created.headers()["idempotency-replayed"], "false");
    assert_eq!(
        created.headers()[header::LOCATION],
        format!("/api/v1/projects/{}/issues/9007199254740993", f.project)
    );
    let receipt = json(created, StatusCode::CREATED).await;
    assert_eq!(receipt.as_object().unwrap().len(), 5);
    assert_eq!(receipt["issue_number"], "9007199254740993");
    assert_eq!(receipt["created_version"], "1");
    let stored: (String, String) =
        sqlx::query_as("SELECT title, description FROM issues WHERE id = ?")
            .bind(receipt["id"].as_str().unwrap())
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(
        stored,
        (
            "Private <script>title</script>".to_owned(),
            "Private text".to_owned()
        )
    );
    let replay = f
        .app
        .clone()
        .oneshot(f.post(&key, body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::CREATED).await, receipt);
    let conflict = json(
        f.app
            .clone()
            .oneshot(f.post(&key, r#"{"title":"Different"}"#).await)
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(conflict["code"], "idempotency_conflict");
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 9007199254740994));
}

#[tokio::test]
async fn post_concurrent_duplicates_and_injected_write_failure_are_atomic() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let left = f.post(&key, r#"{"title":"Concurrent"}"#).await;
    let right = f.post(&key, r#"{"title":"Concurrent"}"#).await;
    let (left, right) = tokio::join!(f.app.clone().oneshot(left), f.app.clone().oneshot(right));
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(
        left.headers()["idempotency-replayed"],
        right.headers()["idempotency-replayed"]
    );
    assert_eq!(
        json(left, StatusCode::CREATED).await,
        json(right, StatusCode::CREATED).await
    );
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 2));
    sqlx::raw_sql("CREATE TRIGGER fail_browser_receipt BEFORE INSERT ON issue_creation_receipts BEGIN SELECT RAISE(FAIL, 'private fixture failure'); END").execute(f.db.pool()).await.unwrap();
    let key = Uuid::now_v7().to_string();
    let error = json(
        f.app
            .clone()
            .oneshot(f.post(&key, r#"{"title":"Rollback"}"#).await)
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "storage_unavailable");
    assert!(!error.to_string().contains("fixture"));
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 2));
    sqlx::query("DROP TRIGGER fail_browser_receipt")
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.app
            .clone()
            .oneshot(f.post(&key, r#"{"title":"Rollback"}"#).await)
            .await
            .unwrap(),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(f.write_counts().await, (2, 2, 2, 2, 3));
}

#[tokio::test]
async fn post_strict_json_and_domain_validation_leave_keys_and_counters_unused() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    for body in [
        "",
        "[]",
        "{}",
        r#"{"title":"a","title":"b"}"#,
        r#"{"title":"a","actor_user_id":"forged"}"#,
        r#"{"title":"a","description":null}"#,
        r#"{"title":"a","assignee_user_id":null}"#,
        r#"{"title":"a"} {}"#,
    ] {
        let error = json(
            f.app
                .clone()
                .oneshot(f.post(&key, body).await)
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(error["code"], "malformed_request");
    }
    for body in [
        r#"{"title":" "}"#.to_owned(),
        serde_json::json!({"title":"a".repeat(241)}).to_string(),
        serde_json::json!({"title":"a", "description":"é".repeat(32769)}).to_string(),
        r#"{"title":"a","assignee_user_id":"bad"}"#.to_owned(),
    ] {
        json(
            f.app
                .clone()
                .oneshot(f.post(&key, &body).await)
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for key in ["bad", "00000000000000000000000000000000"] {
        json(
            f.app
                .clone()
                .oneshot(f.post(key, r#"{"title":"a"}"#).await)
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let mut req = f.post(&key, r#"{"title":"a"}"#).await;
    req.headers_mut()
        .append("idempotency-key", key.parse().unwrap());
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let mut req = f.post(&key, r#"{"title":"a"}"#).await;
    req.headers_mut().remove("idempotency-key");
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let mut req = f.post(&key, r#"{"title":"a"}"#).await;
    *req.uri_mut() = f.path("?actor=forged").parse().unwrap();
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(f.write_counts().await, (0, 0, 0, 0, 1));
    // Escaped maximum UTF-8 description remains within the transport budget.
    let body = format!(
        r#"{{"title":"accepted","description":"{}"}}"#,
        "\\u0001".repeat(65536)
    );
    let mut req = f.post(&key, &body).await;
    req.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=\"UTF-8\""),
    );
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 2));
}

#[tokio::test]
async fn post_transport_media_framing_and_body_deadline_reject_before_activity_or_mutation() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let base = f.post(&key, r#"{"title":"a"}"#).await;
    let activity: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    for (name, value, status) in [
        (
            "origin",
            "https://hostile.example.test",
            StatusCode::FORBIDDEN,
        ),
        ("x-csrf-token", "wrong", StatusCode::FORBIDDEN),
        (
            "content-type",
            "text/plain",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "content-type",
            "application/json; charset=latin1",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "content-encoding",
            "identity",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("content-length", "524289", StatusCode::PAYLOAD_TOO_LARGE),
        ("content-length", "1", StatusCode::BAD_REQUEST),
        (
            "transfer-encoding",
            "gzip, chunked",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut req = request(&f.path(""), Some(&f.token.token));
        *req.method_mut() = Method::POST;
        *req.headers_mut() = base.headers().clone();
        req.headers_mut().insert(name, value.parse().unwrap());
        *req.body_mut() = Body::from(r#"{"title":"a"}"#);
        json(f.app.clone().oneshot(req).await.unwrap(), status).await;
    }
    for oversized in [false, true] {
        let mut req = request(&f.path(""), Some(&f.token.token));
        *req.method_mut() = Method::POST;
        *req.headers_mut() = base.headers().clone();
        *req.body_mut() = if oversized {
            Body::from(vec![b' '; 524289])
        } else {
            Body::from(vec![0xff])
        };
        json(
            f.app.clone().oneshot(req).await.unwrap(),
            if oversized {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            },
        )
        .await;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = f.app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for (method, route, suffix, status) in [
        ("POST", "", "", "408"),
        ("POST", "", "invalid-chunk\r\n", "400"),
        ("PUT", "/1/text", "", "408"),
        ("PUT", "/1/text", "invalid-chunk\r\n", "400"),
    ] {
        let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
        let csrf = base.headers()["x-csrf-token"].to_str().unwrap();
        connection.write_all(format!("{method} {}{route} HTTP/1.1\r\nHost: tracker.example.test\r\nCookie: __Host-puxiri_session={}\r\nOrigin: https://tracker.example.test\r\nx-csrf-token: {csrf}\r\nIdempotency-Key: {key}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{suffix}", f.path(""), f.token.token.expose()).as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let current: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(activity, current);
    assert_eq!(f.write_counts().await, (0, 0, 0, 0, 1));
}

#[tokio::test]
async fn post_replay_revalidates_membership_session_and_archive_and_default_router_stays_closed() {
    let mut f = Fixture::new().await;
    let actor = user(&f.db, "member").await;
    member(&f.db, &f.project, &actor, "member").await;
    f.token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&actor, Utc::now())
        .await
        .unwrap();
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"Private"}"#;
    json(
        f.app
            .clone()
            .oneshot(f.post(&key, body).await)
            .await
            .unwrap(),
        StatusCode::CREATED,
    )
    .await;
    let req = f.post(&key, body).await;
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE user_id = ?")
        .bind(&actor)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    sqlx::query("UPDATE project_members SET role = 'member' WHERE user_id = ?")
        .bind(&actor)
        .execute(f.db.pool())
        .await
        .unwrap();
    let req = f.post(&key, body).await;
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, false, Utc::now())
        .await
        .unwrap();
    let req = f.post(&key, body).await;
    let closed = http::router(f.db.clone()).oneshot(req).await.unwrap();
    assert_eq!(closed.status(), StatusCode::NOT_FOUND);
    let req = f.post(&key, body).await;
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&actor, &f.token.id, Utc::now())
        .await
        .unwrap();
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 2));
}

#[tokio::test]
async fn post_body_permission_is_not_widened_to_other_methods_or_routes_and_framing_is_unambiguous()
{
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    for (method, path) in [
        (Method::GET, f.path("")),
        (Method::PATCH, f.path("")),
        (Method::POST, format!("{}/1", f.path(""))),
        (Method::POST, "/api/v1/projects".to_owned()),
        (Method::POST, "/api/v1/auth/session".to_owned()),
        (Method::POST, "/api/v1/auth/login".to_owned()),
    ] {
        let mut req = f.post(&key, r#"{"title":"Not accepted"}"#).await;
        *req.method_mut() = method;
        *req.uri_mut() = path.parse().unwrap();
        let response = f.app.clone().oneshot(req).await.unwrap();
        assert!(
            [StatusCode::PAYLOAD_TOO_LARGE, StatusCode::NOT_FOUND].contains(&response.status())
        );
    }
    for mode in 0..4 {
        let mut req = f.post(&key, r#"{"title":"a"}"#).await;
        match mode {
            0 => {
                req.headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("13"));
                req.headers_mut()
                    .append(header::CONTENT_LENGTH, HeaderValue::from_static("13"));
            }
            1 => {
                req.headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("13"));
                req.headers_mut().insert(
                    header::TRANSFER_ENCODING,
                    HeaderValue::from_static("chunked"),
                );
            }
            2 => {
                req.headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("+13"));
            }
            _ => {
                req.headers_mut().insert(
                    header::TRANSFER_ENCODING,
                    HeaderValue::from_static("chunked"),
                );
                req.headers_mut().append(
                    header::TRANSFER_ENCODING,
                    HeaderValue::from_static("chunked"),
                );
            }
        }
        json(
            f.app.clone().oneshot(req).await.unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    assert_eq!(f.write_counts().await, (0, 0, 0, 0, 1));
}

#[tokio::test]
async fn post_safe_errors_cover_missing_authority_defaults_and_unavailable_receipt_key() {
    let mut f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"Key protected"}"#;
    json(
        f.app
            .clone()
            .oneshot(f.post(&key, body).await)
            .await
            .unwrap(),
        StatusCode::CREATED,
    )
    .await;
    let other_master =
        MasterKeyStore::load_or_create(&f._directory.path().join("different.key")).unwrap();
    f.app = http::router_with_browser_sessions(
        f.db.clone(),
        BrowserSessionSecurity::new("https://tracker.example.test", other_master).unwrap(),
        policy(),
    );
    let error = json(
        f.app
            .clone()
            .oneshot(f.post(&key, body).await)
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "idempotency_unavailable");
    assert_eq!(error["retryable"], false);
    sqlx::query("UPDATE issue_types SET archived_at = ? WHERE project_id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    let error = json(
        f.app
            .clone()
            .oneshot(f.post(&Uuid::now_v7().to_string(), body).await)
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(error["code"], "issue_schema_unavailable");
    let admin = user(&f.db, "admin").await;
    f.token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&admin, Utc::now())
        .await
        .unwrap();
    let foreign = json(
        f.app
            .clone()
            .oneshot(f.post(&key, body).await)
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    let mut req = f.post(&key, body).await;
    *req.uri_mut() = format!("/api/v1/projects/{}/issues", Uuid::now_v7())
        .parse()
        .unwrap();
    let missing = json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(foreign["code"], missing["code"]);
    assert_eq!(foreign["message"], missing["message"]);
    assert_eq!(f.write_counts().await, (1, 1, 1, 1, 2));
}
