use super::*;

impl Fixture {
    async fn priority_request_for(
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
        let mut req = request(&format!("{}/{number}/priority", self.path("")), Some(token));
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

    async fn priority_request(&self, number: &str, key: &str, body: &str) -> Request<Body> {
        self.priority_request_for(&self.token.token, number, key, body)
            .await
    }

    async fn priority_counts(&self, issue_id: &str) -> (i64, String, i64, i64, i64) {
        sqlx::query_as(
            "SELECT version, priority, \
                    (SELECT COUNT(*) FROM issue_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM outbox_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM issue_priority_receipts WHERE issue_id = ?) \
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

fn priority_body(version: &str, priority: &str) -> String {
    serde_json::json!({
        "expected_version": version,
        "priority": priority,
    })
    .to_string()
}

#[tokio::test]
async fn priority_route_commits_structured_history_and_original_receipt_replay() {
    let f = Fixture::new().await;
    let issue_id = f.create("Priority route").await;
    let first_key = Uuid::now_v7().to_string();
    let first_body = priority_body("1", "high");
    let first = f
        .app
        .clone()
        .oneshot(f.priority_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()["idempotency-replayed"], "false");
    assert_eq!(
        first.headers()[header::LOCATION],
        format!("{}/1", f.path(""))
    );
    let receipt = json(first, StatusCode::OK).await;
    assert_eq!(receipt["id"], issue_id);
    assert_eq!(receipt["project_id"], f.project);
    assert_eq!(receipt["issue_number"], "1");
    assert_eq!(receipt["issue_key"], "DEV-1");
    assert_eq!(receipt["priority"], "high");
    assert_eq!(receipt["result_version"], "2");
    assert_eq!(receipt["changed"], true);
    assert_eq!(
        f.priority_counts(&issue_id).await,
        (2, "high".to_owned(), 2, 2, 1)
    );

    let later = json(
        f.app
            .clone()
            .oneshot(
                f.priority_request("1", &Uuid::now_v7().to_string(), &priority_body("2", "low"))
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(later["result_version"], "3");

    let replay = f
        .app
        .clone()
        .oneshot(f.priority_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, receipt);
    assert_eq!(
        f.priority_counts(&issue_id).await,
        (3, "low".to_owned(), 3, 3, 2)
    );

    let conflict = json(
        f.app
            .clone()
            .oneshot(
                f.priority_request("1", &first_key, &priority_body("1", "urgent"))
                    .await,
            )
            .await
            .unwrap(),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(conflict["code"], "idempotency_conflict");

    let history = json(
        f.get(&format!("{}/1/history", f.path("")), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    let structured: Vec<&Value> = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event_kind"] == "issue.field_updated")
        .collect();
    assert_eq!(structured.len(), 2);
    assert_eq!(
        structured[0]["change"],
        serde_json::json!({
            "field": "priority", "before": "none", "after": "high"
        })
    );
    assert_eq!(
        structured[1]["change"],
        serde_json::json!({
            "field": "priority", "before": "high", "after": "low"
        })
    );
    assert_eq!(structured[0].as_object().unwrap().len(), 7);
    assert!(!history.to_string().contains("PRIVATE DESCRIPTION"));

    let envelopes: Vec<(String, i64)> = sqlx::query_as(
        "SELECT event_kind, issue_version FROM outbox_events \
         WHERE issue_id = ? ORDER BY issue_version",
    )
    .bind(&issue_id)
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(
        envelopes,
        [
            ("issue.created".to_owned(), 1),
            ("issue.field_updated".to_owned(), 2),
            ("issue.field_updated".to_owned(), 3),
        ]
    );
    let persisted_events: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT field_key, before_value_json, after_value_json \
             FROM issue_events WHERE issue_id = ? AND event_kind = 'issue.field_updated' \
             ORDER BY occurred_at, id",
    )
    .bind(&issue_id)
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(
        persisted_events,
        [
            (
                Some("priority".to_owned()),
                Some("\"none\"".to_owned()),
                Some("\"high\"".to_owned()),
            ),
            (
                Some("priority".to_owned()),
                Some("\"high\"".to_owned()),
                Some("\"low\"".to_owned()),
            ),
        ]
    );
}

#[tokio::test]
async fn priority_route_rejects_stale_and_unsupported_requests_without_mutating() {
    let f = Fixture::new().await;
    let issue_id = f.create("Priority validation").await;
    let stale = json(
        f.app
            .clone()
            .oneshot(
                f.priority_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &priority_body("2", "high"),
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
        (
            "",
            "bad-key".to_owned(),
            "{\"expected_version\":\"1\",\"priority\":\"high\"}".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "?unknown=true",
            Uuid::now_v7().to_string(),
            "{\"expected_version\":\"1\",\"priority\":\"high\"}".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "",
            Uuid::now_v7().to_string(),
            "{\"expected_version\":\"1\",\"priority\":\"high\",\"due_date\":\"2028-01-01\"}"
                .to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "",
            Uuid::now_v7().to_string(),
            "{\"expected_version\":\"1\",\"priority\":\"unknown\"}".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (suffix, key, body, expected) in cases {
        let mut request = f.priority_request("1", &key, &body).await;
        let uri = format!("{}{suffix}", request.uri());
        *request.uri_mut() = uri.parse().unwrap();
        let observed = f.app.clone().oneshot(request).await.unwrap().status();
        assert_eq!(
            observed, expected,
            "unexpected response for suffix {suffix:?} and body {body:?}"
        );
    }
    assert_eq!(
        f.priority_counts(&issue_id).await,
        (1, "none".to_owned(), 1, 1, 0)
    );
}
