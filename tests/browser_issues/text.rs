use super::*;

impl Fixture {
    async fn put_text(&self, number: &str, key: &str, body: &str) -> Request<Body> {
        let session = json(
            self.get("/api/v1/auth/session", Some(&self.token.token))
                .await,
            StatusCode::OK,
        )
        .await;
        let mut req = request(
            &format!("{}/{number}/text", self.path("")),
            Some(&self.token.token),
        );
        *req.method_mut() = Method::PUT;
        req.headers_mut().insert(
            header::ORIGIN,
            "https://tracker.example.test".parse().unwrap(),
        );
        req.headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        req.headers_mut().insert(
            "x-csrf-token",
            session["csrf_token"].as_str().unwrap().parse().unwrap(),
        );
        req.headers_mut()
            .insert("idempotency-key", key.parse().unwrap());
        *req.body_mut() = Body::from(body.to_owned());
        req
    }
    async fn text_counts(&self) -> (i64, i64, i64, i64) {
        sqlx::query_as("SELECT version, (SELECT COUNT(*) FROM issue_events), (SELECT COUNT(*) FROM outbox_events), (SELECT COUNT(*) FROM issue_text_receipts) FROM issues WHERE project_id = ? AND issue_number = 1").bind(&self.project).fetch_one(self.db.pool()).await.unwrap()
    }
}

#[tokio::test]
async fn put_text_preserves_large_i64_version_precision() {
    let f = Fixture::new().await;
    let issue = f.create("Original").await;
    sqlx::query("UPDATE issues SET version = 9007199254740993 WHERE id = ?")
        .bind(&issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    let response = f
        .app
        .clone()
        .oneshot(
            f.put_text(
                "1",
                &Uuid::now_v7().to_string(),
                r#"{"title":"Changed","expected_version":"9007199254740993"}"#,
            )
            .await,
        )
        .await
        .unwrap();
    let receipt = json(response, StatusCode::OK).await;
    assert_eq!(receipt["result_version"], "9007199254740994");
    assert_eq!(f.text_counts().await, (9007199254740994, 2, 2, 1));
}

#[tokio::test]
async fn put_text_returns_content_free_stable_receipts_noop_replay_readback_and_version_conflict() {
    let f = Fixture::new().await;
    let issue = f.create("Original private text").await;
    let key = Uuid::now_v7().to_string();
    let input = r#"{"title":" Changed ","expected_version":"1"}"#;
    let response = f
        .app
        .clone()
        .oneshot(f.put_text("1", &key, input).await)
        .await
        .unwrap();
    assert_eq!(response.headers()["idempotency-replayed"], "false");
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("{}/1", f.path(""))
    );
    let first = json(response, StatusCode::OK).await;
    assert_eq!(
        first,
        serde_json::json!({"id":issue,"project_id":f.project,"issue_number":"1","issue_key":"DEV-1","result_version":"2","changed":true})
    );
    let detail = json(
        f.get(&format!("{}/1", f.path("")), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["title"], "Changed");
    assert!(detail.get("description").is_none());
    let noop = json(
        f.app
            .clone()
            .oneshot(
                f.put_text(
                    "1",
                    &Uuid::now_v7().to_string(),
                    r#"{"title":"Changed","expected_version":"2"}"#,
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(noop["changed"], false);
    assert_eq!(noop["result_version"], "2");
    json(
        f.app
            .clone()
            .oneshot(
                f.put_text(
                    "1",
                    &Uuid::now_v7().to_string(),
                    r#"{"title":"Latest","description":"Current only","expected_version":"2"}"#,
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let replay = f
        .app
        .clone()
        .oneshot(f.put_text("1", &key, input).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, first);
    for (request_key, body, code) in [
        (Uuid::now_v7().to_string(), input, "issue_version_conflict"),
        (
            key.clone(),
            r#"{"title":"Different","expected_version":"1"}"#,
            "idempotency_conflict",
        ),
    ] {
        let error = json(
            f.app
                .clone()
                .oneshot(f.put_text("1", &request_key, body).await)
                .await
                .unwrap(),
            StatusCode::CONFLICT,
        )
        .await;
        assert_eq!(error["code"], code);
        assert_eq!(error["retryable"], false);
        assert!(!error.to_string().contains("private text"));
    }
    assert_eq!(f.text_counts().await, (3, 3, 3, 3));
}

#[tokio::test]
async fn put_text_competing_duplicates_commit_once_and_failed_receipt_rolls_back() {
    let f = Fixture::new().await;
    f.create("Original").await;
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"Changed","expected_version":"1"}"#;
    let left = f.put_text("1", &key, body).await;
    let right = f.put_text("1", &key, body).await;
    let (left, right) = tokio::join!(f.app.clone().oneshot(left), f.app.clone().oneshot(right));
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(
        left.headers()["idempotency-replayed"],
        right.headers()["idempotency-replayed"]
    );
    assert_eq!(
        json(left, StatusCode::OK).await,
        json(right, StatusCode::OK).await
    );
    assert_eq!(f.text_counts().await, (2, 2, 2, 1));
    sqlx::raw_sql("CREATE TRIGGER text_receipt_fail BEFORE INSERT ON issue_text_receipts BEGIN SELECT RAISE(FAIL, 'private fixture'); END").execute(f.db.pool()).await.unwrap();
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"Rollback","expected_version":"2"}"#;
    let error = json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "storage_unavailable");
    assert_eq!(f.text_counts().await, (2, 2, 2, 1));
    sqlx::query("DROP TRIGGER text_receipt_fail")
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(f.text_counts().await, (3, 3, 3, 2));
}

#[tokio::test]
async fn put_text_requires_strict_json_decimal_version_selectors_and_one_key_without_side_effects()
{
    let f = Fixture::new().await;
    f.create("Original").await;
    let key = Uuid::now_v7().to_string();
    for body in [
        "",
        "{}",
        r#"{"title":"a"}"#,
        r#"{"title":"a","expected_version":1}"#,
        r#"{"title":"a","expected_version":"1","description":null}"#,
        r#"{"title":"a","expected_version":"1","rank":"3"}"#,
        r#"{"title":"a","expected_version":"1","title":"duplicate"}"#,
    ] {
        json(
            f.app
                .clone()
                .oneshot(f.put_text("1", &key, body).await)
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for version in ["0", "01", "-1", "1.0", "9223372036854775808"] {
        json(
            f.app
                .clone()
                .oneshot(
                    f.put_text(
                        "1",
                        &key,
                        &serde_json::json!({"title":"a", "expected_version":version}).to_string(),
                    )
                    .await,
                )
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for number in ["0", "01", "-1", "bad"] {
        json(
            f.app
                .clone()
                .oneshot(
                    f.put_text(number, &key, r#"{"title":"a","expected_version":"1"}"#)
                        .await,
                )
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let mut req = f
        .put_text("1", &key, r#"{"title":"a","expected_version":"1"}"#)
        .await;
    req.headers_mut()
        .append("idempotency-key", key.parse().unwrap());
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let mut req = f
        .put_text("1", &key, r#"{"title":"a","expected_version":"1"}"#)
        .await;
    req.headers_mut().remove("idempotency-key");
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(f.text_counts().await, (1, 1, 1, 0));
}

#[tokio::test]
async fn put_text_security_media_body_budget_and_method_admission_do_not_widen_other_routes() {
    let f = Fixture::new().await;
    f.create("Original").await;
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"a","expected_version":"1"}"#;
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
            "content-encoding",
            "gzip",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("content-length", "524289", StatusCode::PAYLOAD_TOO_LARGE),
        ("content-length", "1", StatusCode::BAD_REQUEST),
    ] {
        let mut req = f.put_text("1", &key, body).await;
        req.headers_mut().insert(name, value.parse().unwrap());
        json(f.app.clone().oneshot(req).await.unwrap(), status).await;
    }
    let mut req = f.put_text("1", &key, body).await;
    *req.body_mut() = Body::from(vec![b' '; 524289]);
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    for path in [
        f.path(""),
        format!("{}/1", f.path("")),
        "/api/v1/auth/session".to_owned(),
    ] {
        let mut req = f.put_text("1", &key, body).await;
        *req.uri_mut() = path.parse().unwrap();
        json(
            f.app.clone().oneshot(req).await.unwrap(),
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
    }
    let mut req = request(&format!("{}/1/text", f.path("")), Some(&f.token.token));
    *req.method_mut() = Method::HEAD;
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::ALLOW], "PUT");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    let closed = http::router(f.db.clone())
        .oneshot(f.put_text("1", &key, body).await)
        .await
        .unwrap();
    assert_eq!(closed.status(), StatusCode::NOT_FOUND);
    assert_eq!(f.text_counts().await, (1, 1, 1, 0));
}

#[tokio::test]
async fn put_text_replay_revalidates_membership_archive_trash_and_revocation() {
    let mut f = Fixture::new().await;
    let issue = f.create("Original").await;
    let editor = user(&f.db, "member").await;
    member(&f.db, &f.project, &editor, "member").await;
    f.token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&editor, Utc::now())
        .await
        .unwrap();
    let key = Uuid::now_v7().to_string();
    let body = r#"{"title":"Changed","expected_version":"1"}"#;
    json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE user_id = ?")
        .bind(&editor)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    sqlx::query("UPDATE project_members SET role = 'member' WHERE user_id = ?")
        .bind(&editor)
        .execute(f.db.pool())
        .await
        .unwrap();
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, false, Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.app
            .clone()
            .oneshot(f.put_text("1", &key, body).await)
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    let req = f.put_text("1", &key, body).await;
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&editor, &f.token.id, Utc::now())
        .await
        .unwrap();
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(f.text_counts().await, (2, 2, 2, 1));
}
