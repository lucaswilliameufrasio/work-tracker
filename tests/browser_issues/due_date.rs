use super::*;

impl Fixture {
    async fn due_date_request(&self, number: &str, key: &str, body: &str) -> Request<Body> {
        let session = json(
            self.get("/api/v1/auth/session", Some(&self.token.token))
                .await,
            StatusCode::OK,
        )
        .await;
        let mut req = request(
            &format!("{}/{number}/due-date", self.path("")),
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

    async fn due_date_counts(&self, issue_id: &str) -> (i64, Option<String>, i64, i64, i64) {
        sqlx::query_as(
            "SELECT version, due_date, \
                    (SELECT COUNT(*) FROM issue_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM outbox_events WHERE issue_id = ?), \
                    (SELECT COUNT(*) FROM issue_due_date_receipts WHERE issue_id = ?) \
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

fn due_date_body(version: &str, date: Value) -> String {
    serde_json::json!({
        "expected_version": version,
        "due_date": date,
    })
    .to_string()
}

#[tokio::test]
async fn due_date_route_changes_civil_date_clears_and_replays_original_receipt() {
    let f = Fixture::new().await;
    let issue_id = f.create("Due date route").await;
    let first_key = Uuid::now_v7().to_string();
    let first_body = due_date_body("1", serde_json::json!("2028-02-29"));
    let first = f
        .app
        .clone()
        .oneshot(f.due_date_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()["idempotency-replayed"], "false");
    assert_eq!(
        first.headers()[header::LOCATION],
        format!("{}/1", f.path(""))
    );
    let first_receipt = json(first, StatusCode::OK).await;
    assert_eq!(first_receipt["id"], issue_id);
    assert_eq!(first_receipt["due_date"], "2028-02-29");
    assert_eq!(first_receipt["result_version"], "2");
    assert_eq!(first_receipt["changed"], true);
    assert_eq!(
        f.due_date_counts(&issue_id).await,
        (2, Some("2028-02-29".to_owned()), 2, 2, 1)
    );

    let no_op = json(
        f.app
            .clone()
            .oneshot(
                f.due_date_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &due_date_body("2", serde_json::json!("2028-02-29")),
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

    let cleared_key = Uuid::now_v7().to_string();
    let cleared_body = due_date_body("2", Value::Null);
    let cleared = json(
        f.app
            .clone()
            .oneshot(f.due_date_request("1", &cleared_key, &cleared_body).await)
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(cleared["due_date"].is_null());
    assert_eq!(cleared["result_version"], "3");
    assert_eq!(cleared["changed"], true);
    assert_eq!(f.due_date_counts(&issue_id).await, (3, None, 3, 3, 3));

    let replay = f
        .app
        .clone()
        .oneshot(f.due_date_request("1", &first_key, &first_body).await)
        .await
        .unwrap();
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(json(replay, StatusCode::OK).await, first_receipt);

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
            "field": "due_date", "before": null, "after": "2028-02-29"
        })
    );
    assert_eq!(
        changes[1]["change"],
        serde_json::json!({
            "field": "due_date", "before": "2028-02-29", "after": null
        })
    );
    let detail = json(
        f.get(&format!("{}/1", f.path("")), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(detail.get("due_date").is_none());
    assert_eq!(detail["is_overdue"], false);
}

#[tokio::test]
async fn due_date_route_rejects_stale_missing_invalid_and_extra_values_without_mutation() {
    let f = Fixture::new().await;
    let issue_id = f.create("Due date validation").await;
    let stale = json(
        f.app
            .clone()
            .oneshot(
                f.due_date_request(
                    "1",
                    &Uuid::now_v7().to_string(),
                    &due_date_body("2", serde_json::json!("2028-01-01")),
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
            "{\"expected_version\":\"1\",\"due_date\":\"2026-02-29\"}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "{\"expected_version\":\"1\",\"due_date\":\"2028-2-09\"}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "{\"expected_version\":\"1\",\"due_date\":7}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "{\"expected_version\":\"1\",\"due_date\":null,\"priority\":\"high\"}",
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (body, expected) in cases {
        let response = f
            .app
            .clone()
            .oneshot(
                f.due_date_request("1", &Uuid::now_v7().to_string(), body)
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
    assert_eq!(f.due_date_counts(&issue_id).await, (1, None, 1, 1, 0));
}
