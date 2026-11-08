use super::*;

impl Fixture {
    async fn move_request_for(
        &self,
        token: &SessionToken,
        number: &str,
        key: &str,
        body: &str,
    ) -> Request<Body> {
        let session = json(
            self.get("/api/v1/auth/session", Some(token)).await,
            StatusCode::OK,
        )
        .await;
        let mut req = request(&format!("{}/{number}/move", self.path("")), Some(token));
        *req.method_mut() = Method::POST;
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

    async fn move_request(&self, number: &str, key: &str, body: &str) -> Request<Body> {
        self.move_request_for(&self.token.token, number, key, body)
            .await
    }

    async fn status(&self, system_key: &str) -> String {
        sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = ?")
            .bind(&self.project)
            .bind(system_key)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn move_counts(&self, issue_id: &str) -> (i64, i64, i64, i64, String) {
        sqlx::query_as("SELECT version, (SELECT COUNT(*) FROM issue_events WHERE issue_id = ?), (SELECT COUNT(*) FROM outbox_events WHERE issue_id = ?), (SELECT COUNT(*) FROM issue_move_receipts WHERE issue_id = ?), status_id FROM issues WHERE id = ?")
            .bind(issue_id)
            .bind(issue_id)
            .bind(issue_id)
            .bind(issue_id)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

fn move_body(version: &str, status: &str, previous: Option<&str>, next: Option<&str>) -> String {
    let mut value = serde_json::json!({
        "expected_version": version,
        "target_status_id": status,
    });
    if let Some(previous) = previous {
        value["previous_issue_id"] = serde_json::json!(previous);
    }
    if let Some(next) = next {
        value["next_issue_id"] = serde_json::json!(next);
    }
    value.to_string()
}

#[tokio::test]
async fn move_commits_completion_history_outbox_and_original_receipt_for_later_replay() {
    let f = Fixture::new().await;
    let issue_id = f.create("Move lifecycle").await;
    let active = f.status("in_progress").await;
    let done = f.status("done").await;
    let first_key = Uuid::now_v7().to_string();
    let first_body = move_body("1", &active, None, None);

    let first_response = f
        .app
        .clone()
        .oneshot(f.move_request("1", &first_key, &first_body).await)
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
    assert_eq!(first_receipt["project_id"], f.project);
    assert_eq!(first_receipt["issue_number"], "1");
    assert_eq!(first_receipt["issue_key"], "DEV-1");
    assert_eq!(first_receipt["status_id"], active);
    assert_eq!(first_receipt["result_version"], "2");
    assert_eq!(first_receipt["changed"], true);
    assert_eq!(f.move_counts(&issue_id).await, (2, 2, 2, 1, active.clone()));

    let completed = json(
        f.app
            .clone()
            .oneshot(
                f.move_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &move_body("2", &done, None, None),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(completed["result_version"], "3");
    let completed_at: Option<String> =
        sqlx::query_scalar("SELECT completed_at FROM issues WHERE id = ?")
            .bind(&issue_id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert!(completed_at.is_some());

    let reopened = json(
        f.app
            .clone()
            .oneshot(
                f.move_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &move_body("3", &active, None, None),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(reopened["result_version"], "4");
    let reopened_at: Option<String> =
        sqlx::query_scalar("SELECT completed_at FROM issues WHERE id = ?")
            .bind(&issue_id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(reopened_at, None);

    let replay = f
        .app
        .clone()
        .oneshot(f.move_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, first_receipt);
    assert_eq!(f.move_counts(&issue_id).await, (4, 4, 4, 3, active.clone()));

    let history = json(
        f.get(&format!("{}/1/history", f.path("")), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    let kinds: Vec<&str> = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event_kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["issue.created", "issue.moved", "issue.moved", "issue.moved"]
    );
    let outbox = sqlx::query_as::<_, (String, i64)>(
        "SELECT event_kind, issue_version FROM outbox_events WHERE issue_id = ? ORDER BY issue_version",
    )
    .bind(&issue_id)
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(
        outbox,
        [
            ("issue.created".to_owned(), 1),
            ("issue.moved".to_owned(), 2),
            ("issue.moved".to_owned(), 3),
            ("issue.moved".to_owned(), 4),
        ]
    );

    let conflict = json(
        f.app
            .clone()
            .oneshot(
                f.move_request("1", &first_key, &move_body("1", &done, None, None))
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(conflict["code"], "idempotency_conflict");
    assert_eq!(conflict["retryable"], false);
}

#[tokio::test]
async fn move_effective_noop_is_receipted_once_and_replay_does_not_reapply_or_bypass_current_access()
 {
    let f = Fixture::new().await;
    let issue_id = f.create("No-op move").await;
    let todo = f.status("todo").await;
    let active = f.status("in_progress").await;
    let no_op_key = Uuid::now_v7().to_string();
    let no_op_body = move_body("1", &todo, None, None);

    let no_op = f
        .app
        .clone()
        .oneshot(f.move_request("1", &no_op_key, &no_op_body).await)
        .await
        .unwrap();
    assert_eq!(no_op.status(), StatusCode::OK);
    assert_eq!(no_op.headers()["idempotency-replayed"], "false");
    let no_op_receipt = json(no_op, StatusCode::OK).await;
    assert_eq!(no_op_receipt["status_id"], todo);
    assert_eq!(no_op_receipt["result_version"], "1");
    assert_eq!(no_op_receipt["changed"], false);
    assert_eq!(f.move_counts(&issue_id).await, (1, 1, 1, 1, todo.clone()));

    let changed = json(
        f.app
            .clone()
            .oneshot(
                f.move_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &move_body("1", &active, None, None),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed["result_version"], "2");
    assert_eq!(changed["changed"], true);

    let replay = f
        .app
        .clone()
        .oneshot(f.move_request("1", &no_op_key, &no_op_body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, no_op_receipt);
    assert_eq!(f.move_counts(&issue_id).await, (2, 2, 2, 2, active.clone()));

    let replacement_manager = user(&f.db, "member").await;
    member(&f.db, &f.project, &replacement_manager, "manager").await;
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&f.owner)
        .execute(f.db.pool())
        .await
        .unwrap();
    let denied_replay = json(
        f.app
            .clone()
            .oneshot(f.move_request("1", &no_op_key, &no_op_body).await)
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(denied_replay["code"], "not_found");
    assert_eq!(f.move_counts(&issue_id).await, (2, 2, 2, 2, active));
}

#[tokio::test]
async fn concurrent_identical_moves_commit_once_and_return_one_replay_receipt() {
    let f = Fixture::new().await;
    let issue_id = f.create("Concurrent move").await;
    let active = f.status("in_progress").await;
    let key = Uuid::now_v7().to_string();
    let body = move_body("1", &active, None, None);
    let first_request = f.move_request("1", &key, &body).await;
    let second_request = f.move_request("1", &key, &body).await;
    let (first, second) = tokio::join!(
        f.app.clone().oneshot(first_request),
        f.app.clone().oneshot(second_request),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let replay_flags = [
        first.headers()["idempotency-replayed"].to_str().unwrap(),
        second.headers()["idempotency-replayed"].to_str().unwrap(),
    ];
    assert!(replay_flags.contains(&"true"));
    assert!(replay_flags.contains(&"false"));
    let first_receipt = json(first, StatusCode::OK).await;
    let second_receipt = json(second, StatusCode::OK).await;
    assert_eq!(first_receipt, second_receipt);
    assert_eq!(first_receipt["result_version"], "2");
    assert_eq!(f.move_counts(&issue_id).await, (2, 2, 2, 1, active));
}

#[tokio::test]
async fn move_uses_neighbor_ids_and_rebalances_tight_rank_gaps_transactionally() {
    let f = Fixture::new().await;
    let moving = f.create("Moving").await;
    let previous = f.create("Previous").await;
    let next = f.create("Next").await;
    let active = f.status("in_progress").await;
    sqlx::query("UPDATE issues SET status_id = ?, rank = 0 WHERE id = ?")
        .bind(&active)
        .bind(&previous)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET status_id = ?, rank = 1 WHERE id = ?")
        .bind(&active)
        .bind(&next)
        .execute(f.db.pool())
        .await
        .unwrap();

    let body = move_body("1", &active, Some(&previous), Some(&next));
    let receipt = json(
        f.app
            .clone()
            .oneshot(
                f.move_request("1", &Uuid::now_v7().to_string(), &body)
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(receipt["changed"], true);
    let ordered: Vec<(String, i64)> = sqlx::query_as(
        "SELECT id, rank FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL ORDER BY rank, id",
    )
    .bind(&f.project)
    .bind(&active)
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(
        ordered,
        [(previous, 1_024), (moving, 2_048), (next.clone(), 3_072)]
    );
    let foreign_move = json(
        f.app
            .clone()
            .oneshot(
                f.move_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &move_body("2", &active, None, Some(&next)),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(foreign_move["code"], "issue_order_conflict");
}

#[tokio::test]
async fn move_rejects_stale_inputs_hidden_statuses_and_read_only_roles_without_writes() {
    let f = Fixture::new().await;
    let issue = f.create("Protected move").await;
    let active = f.status("in_progress").await;
    let wrong_method = f
        .get(&format!("{}/1/move", f.path("")), Some(&f.token.token))
        .await;
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong_method.headers()[header::ALLOW], "POST");
    assert_eq!(
        json(wrong_method, StatusCode::METHOD_NOT_ALLOWED).await["code"],
        "method_not_allowed"
    );
    for (body, status, code) in [
        (
            move_body("2", &active, None, None),
            StatusCode::CONFLICT,
            "issue_version_conflict",
        ),
        (
            move_body("1", &f.status("backlog").await, None, None),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ] {
        let error = json(
            f.app
                .clone()
                .oneshot(
                    f.move_request("1", &Uuid::now_v7().to_string(), &body)
                        .await,
                )
                .await
                .unwrap(),
            status,
        )
        .await;
        assert_eq!(error["code"], code);
    }

    let reader = user(&f.db, "guest").await;
    member(&f.db, &f.project, &reader, "viewer").await;
    let read_only = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&reader, Utc::now())
        .await
        .unwrap();
    let denied = json(
        f.app
            .clone()
            .oneshot(
                f.move_request_for(
                    &read_only.token,
                    "1",
                    &Uuid::now_v7().to_string(),
                    &move_body("1", &active, None, None),
                )
                .await,
            )
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(denied["code"], "not_found");
    assert_eq!(
        f.move_counts(&issue).await,
        (1, 1, 1, 0, f.status("todo").await)
    );
}

#[tokio::test]
async fn move_requires_strict_json_and_rolls_back_all_effects_when_receipt_insert_fails() {
    let f = Fixture::new().await;
    let issue = f.create("Atomic move").await;
    let active = f.status("in_progress").await;
    let key = Uuid::now_v7().to_string();
    for body in [
        "",
        "{}",
        r#"{"expected_version":1,"target_status_id":"bad"}"#,
        r#"{"expected_version":"1","target_status_id":"bad","rank":"2"}"#,
        r#"{"expected_version":"1","target_status_id":"00000000-0000-0000-0000-000000000000","next_issue_id":null}"#,
        r#"{"expected_version":"1","target_status_id":"00000000-0000-0000-0000-000000000000","expected_version":"1"}"#,
    ] {
        json(
            f.app
                .clone()
                .oneshot(f.move_request("1", &key, body).await)
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for version in ["0", "01", "-1", "9223372036854775808"] {
        let body = move_body(version, &active, None, None);
        json(
            f.app
                .clone()
                .oneshot(f.move_request("1", &key, &body).await)
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }

    sqlx::raw_sql("CREATE TRIGGER move_receipt_fail BEFORE INSERT ON issue_move_receipts BEGIN SELECT RAISE(FAIL, 'private fixture'); END")
        .execute(f.db.pool())
        .await
        .unwrap();
    let failed = json(
        f.app
            .clone()
            .oneshot(
                f.move_request("1", &key, &move_body("1", &active, None, None))
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(failed["code"], "storage_unavailable");
    assert_eq!(
        f.move_counts(&issue).await,
        (1, 1, 1, 0, f.status("todo").await)
    );
    sqlx::query("DROP TRIGGER move_receipt_fail")
        .execute(f.db.pool())
        .await
        .unwrap();

    let success = json(
        f.app
            .clone()
            .oneshot(
                f.move_request("1", &key, &move_body("1", &active, None, None))
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(success["result_version"], "2");
    assert_eq!(f.move_counts(&issue).await, (2, 2, 2, 1, active));
}
