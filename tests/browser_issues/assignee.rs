use super::*;

impl Fixture {
    async fn assignee_request(&self, number: &str, key: &str, body: &str) -> Request<Body> {
        let session = json(
            self.get("/api/v1/auth/session", Some(&self.token.token))
                .await,
            StatusCode::OK,
        )
        .await;
        let mut req = request(
            &format!("{}/{number}/assignee", self.path("")),
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

    async fn assignment_counts(&self, issue_id: &str) -> (i64, Option<String>, i64, i64, i64) {
        sqlx::query_as(
            "SELECT version, assignee_user_id, \
                    (SELECT COUNT(*) FROM issue_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM outbox_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM issue_assignee_receipts WHERE issue_id = ?) \
             FROM issues WHERE id = ?",
        )
        .bind(issue_id)
        .bind(issue_id)
        .bind(issue_id)
        .bind(issue_id)
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }
}

fn assignee_body(version: &str, assignee: Value) -> String {
    serde_json::json!({
        "expected_version": version,
        "assignee_user_id": assignee,
    })
    .to_string()
}

#[tokio::test]
async fn assignment_changes_are_versioned_idempotent_and_history_is_content_free() {
    let f = Fixture::new().await;
    let issue_id = f.create("Assignee receipt").await;
    let assigned_user = user(&f.db, "member").await;
    member(&f.db, &f.project, &assigned_user, "member").await;
    let first_key = Uuid::now_v7().to_string();
    let first_body = assignee_body("1", serde_json::json!(assigned_user));
    let first_response = f
        .app
        .clone()
        .oneshot(f.assignee_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    assert_eq!(first_response.headers()["idempotency-replayed"], "false");
    assert_eq!(
        first_response.headers()[header::LOCATION],
        format!("{}/1", f.path(""))
    );
    let first_receipt = json(first_response, StatusCode::OK).await;
    assert_eq!(first_receipt["id"], issue_id);
    assert_eq!(first_receipt["assignee_user_id"], assigned_user);
    assert_eq!(first_receipt["result_version"], "2");
    assert_eq!(first_receipt["changed"], true);
    assert_eq!(
        f.assignment_counts(&issue_id).await,
        (2, Some(assigned_user.clone()), 2, 2, 1)
    );

    let no_op = json(
        f.app
            .clone()
            .oneshot(
                f.assignee_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &assignee_body("2", serde_json::json!(assigned_user)),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(no_op["changed"], false);
    assert_eq!(no_op["result_version"], "2");
    assert_eq!(
        f.assignment_counts(&issue_id).await,
        (2, Some(assigned_user.clone()), 2, 2, 2)
    );

    let replay = f
        .app
        .clone()
        .oneshot(f.assignee_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, first_receipt);

    let clear_key = Uuid::now_v7().to_string();
    let clear_body = assignee_body("2", Value::Null);
    let clear = json(
        f.app
            .clone()
            .oneshot(f.assignee_request("1", &clear_key, &clear_body).await)
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(clear["assignee_user_id"].is_null());
    assert_eq!(clear["result_version"], "3");
    assert_eq!(clear["changed"], true);
    assert_eq!(f.assignment_counts(&issue_id).await, (3, None, 3, 3, 3));

    let history = json(
        f.get(&format!("{}/1/history", f.path("")), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    let changes = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event_kind"] == "issue.field_updated")
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0]["change"],
        serde_json::json!({
            "field": "assignee_user_id", "before": null, "after": assigned_user
        })
    );
    assert_eq!(
        changes[1]["change"],
        serde_json::json!({
            "field": "assignee_user_id", "before": assigned_user, "after": null
        })
    );
    assert!(!history.to_string().contains("Assignee receipt"));
}

#[tokio::test]
async fn assignment_revalidates_target_and_rolls_back_update_history_outbox_and_receipt() {
    let f = Fixture::new().await;
    let issue_id = f.create("Assignee eligibility").await;
    let member_id = user(&f.db, "member").await;
    member(&f.db, &f.project, &member_id, "member").await;
    let guest_id = user(&f.db, "guest").await;
    member(&f.db, &f.project, &guest_id, "member").await;
    let viewer_id = user(&f.db, "member").await;
    member(&f.db, &f.project, &viewer_id, "viewer").await;
    let no_membership_id = user(&f.db, "member").await;
    let inactive_id = user(&f.db, "member").await;
    member(&f.db, &f.project, &inactive_id, "member").await;
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&inactive_id)
        .execute(f.db.pool())
        .await
        .unwrap();

    for target in [&viewer_id, &no_membership_id, &inactive_id] {
        let response = f
            .app
            .clone()
            .oneshot(
                f.assignee_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &assignee_body("1", serde_json::json!(target)),
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(f.assignment_counts(&issue_id).await, (1, None, 1, 1, 0));

    let guest_receipt = json(
        f.app
            .clone()
            .oneshot(
                f.assignee_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &assignee_body("1", serde_json::json!(guest_id)),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(guest_receipt["assignee_user_id"], guest_id);
    assert_eq!(
        f.assignment_counts(&issue_id).await,
        (2, Some(guest_id), 2, 2, 1)
    );

    let rollback_issue = f.create("Assignee rollback").await;
    let rollback_key = Uuid::now_v7().to_string();
    let trigger = format!(
        "CREATE TRIGGER fail_assignee_outbox BEFORE INSERT ON outbox_events \
         WHEN NEW.issue_id = '{rollback_issue}' AND NEW.issue_version = 2 \
         BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(trigger))
        .execute(f.db.pool())
        .await
        .unwrap();
    let response = f
        .app
        .clone()
        .oneshot(
            f.assignee_request(
                "2",
                &rollback_key,
                &assignee_body("1", serde_json::json!(member_id)),
            )
            .await,
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        f.assignment_counts(&rollback_issue).await,
        (1, None, 1, 1, 0)
    );
    sqlx::query("DROP TRIGGER fail_assignee_outbox")
        .execute(f.db.pool())
        .await
        .unwrap();
    let retry = json(
        f.app
            .clone()
            .oneshot(
                f.assignee_request(
                    "2",
                    &rollback_key,
                    &assignee_body("1", serde_json::json!(member_id)),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(retry["assignee_user_id"], member_id);
    assert_eq!(
        f.assignment_counts(&rollback_issue).await,
        (2, Some(member_id), 2, 2, 1)
    );
}

#[tokio::test]
async fn assignment_rejects_stale_malformed_and_unexpected_values_without_mutation() {
    let f = Fixture::new().await;
    let issue_id = f.create("Assignee validation").await;
    let target_id = user(&f.db, "member").await;
    member(&f.db, &f.project, &target_id, "member").await;
    let stale = json(
        f.app
            .clone()
            .oneshot(
                f.assignee_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &assignee_body("2", serde_json::json!(target_id)),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(stale["code"], "issue_version_conflict");

    let cases = [
        ("{}", StatusCode::BAD_REQUEST),
        (
            "{\"expected_version\":\"1\",\"assignee_user_id\":\"not-a-uuid\"}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "{\"expected_version\":\"1\",\"assignee_user_id\":7}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "{\"expected_version\":\"1\",\"assignee_user_id\":null,\"priority\":\"high\"}",
            StatusCode::BAD_REQUEST,
        ),
        ("{\"expected_version\":\"1\"}", StatusCode::BAD_REQUEST),
    ];
    for (body, expected) in cases {
        let response = f
            .app
            .clone()
            .oneshot(
                f.assignee_request("1", &Uuid::now_v7().to_string(), body)
                    .await,
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "unexpected response for {body}"
        );
    }
    assert_eq!(f.assignment_counts(&issue_id).await, (1, None, 1, 1, 0));
}
