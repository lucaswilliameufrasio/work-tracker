use super::*;
use puxiri::projects::session::SessionProjectReader;

fn path(project: &str) -> String {
    format!("/api/v1/projects/{project}/capabilities")
}

#[tokio::test]
async fn capabilities_use_current_membership_archive_and_manager_context_without_granting_private_access()
 {
    let f = Fixture::new().await;
    let initial = json(
        f.get(&path(&f.project), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        initial,
        serde_json::json!({ "project_id": f.project, "can_create_issues": true, "can_replace_issue_text": true })
    );
    let member_id = user(&f.db, "member").await;
    member(&f.db, &f.project, &member_id, "member").await;
    let session = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&member_id, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        json(
            f.get(&path(&f.project), Some(&session.token)).await,
            StatusCode::OK
        )
        .await["can_create_issues"],
        true
    );
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE user_id = ?")
        .bind(&member_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    let viewer = json(
        f.get(&path(&f.project), Some(&session.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(viewer["can_replace_issue_text"], false);
    let csrf = json(
        f.get("/api/v1/auth/session", Some(&session.token)).await,
        StatusCode::OK,
    )
    .await;
    let mut stale = request(&f.path(""), Some(&session.token));
    *stale.method_mut() = Method::POST;
    stale.headers_mut().insert(
        header::ORIGIN,
        "https://tracker.example.test".parse().unwrap(),
    );
    stale
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    stale.headers_mut().insert(
        "x-csrf-token",
        csrf["csrf_token"].as_str().unwrap().parse().unwrap(),
    );
    stale.headers_mut().insert(
        "idempotency-key",
        Uuid::now_v7().to_string().parse().unwrap(),
    );
    *stale.body_mut() = Body::from(r#"{"title":"Stale capability is not authority"}"#);
    json(
        f.app.clone().oneshot(stale).await.unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM issues")
            .fetch_one(f.db.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        json(
            f.get(&path(&f.project), Some(&session.token)).await,
            StatusCode::OK
        )
        .await["can_create_issues"],
        false
    );
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    let archived = json(
        f.get(&path(&f.project), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(archived["can_replace_issue_text"], false);
    assert_eq!(
        json(
            f.get(&path(&f.project), Some(&f.token.token)).await,
            StatusCode::OK
        )
        .await["can_create_issues"],
        false
    );
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, false, Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE project_members SET role = 'member' WHERE user_id = ?")
        .bind(&member_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert_eq!(
        json(
            f.get(&path(&f.project), Some(&session.token)).await,
            StatusCode::OK
        )
        .await["can_create_issues"],
        true
    );
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&f.owner)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert_eq!(
        json(
            f.get(&path(&f.project), Some(&session.token)).await,
            StatusCode::OK
        )
        .await["can_create_issues"],
        false
    );
    let outsider = user(&f.db, "admin").await;
    let outsider = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    for project in [&f.project, &Uuid::now_v7().to_string()] {
        let error = json(
            f.get(&path(project), Some(&outsider.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(error["code"], "not_found");
        assert_eq!(error["message"], "Resource not found");
    }
}

#[tokio::test]
async fn capability_reader_is_token_bound_read_only_no_activity_touch_and_revalidates_revocation() {
    let f = Fixture::new().await;
    let reader = SessionProjectReader::new(f.db.pool().clone(), policy());
    let activity: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(
        reader
            .can_create_issues(
                &f.token.token,
                &f.project,
                Utc::now() + TimeDelta::minutes(6)
            )
            .await
            .unwrap(),
        Some(true)
    );
    let after: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(activity, after);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_creation_receipts")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .can_create_issues(&f.token.token, &f.project, Utc::now())
            .await,
        Err(puxiri::projects::session::SessionProjectReadError::Unauthenticated)
    ));
    json(
        f.get(&path(&f.project), Some(&f.token.token)).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

#[tokio::test]
async fn capabilities_keep_closed_opt_in_bodyless_methods_strict_selectors_and_safe_storage_errors()
{
    let f = Fixture::new().await;
    for suffix in ["?actor=forged", "?", "?page_size=1"] {
        json(
            f.get(
                &format!("{}{suffix}", path(&f.project)),
                Some(&f.token.token),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    json(
        f.get(&path("invalid"), Some(&f.token.token)).await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let closed = http::router(f.db.clone())
        .oneshot(request(&path(&f.project), Some(&f.token.token)))
        .await
        .unwrap();
    assert_eq!(closed.status(), StatusCode::NOT_FOUND);
    let mut req = request(&path(&f.project), Some(&f.token.token));
    *req.method_mut() = Method::POST;
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    let mut req = request(&path(&f.project), Some(&f.token.token));
    *req.body_mut() = Body::from("not accepted");
    json(
        f.app.clone().oneshot(req).await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    let mut req = request(&path(&f.project), Some(&f.token.token));
    *req.method_mut() = Method::HEAD;
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    f.db.close().await;
    let error = json(
        f.get(&path(&f.project), Some(&f.token.token)).await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "storage_unavailable");
}
