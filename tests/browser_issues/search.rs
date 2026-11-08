use super::*;

#[tokio::test]
async fn project_search_returns_literal_text_or_exact_key_pages_without_trash_or_other_projects() {
    let f = Fixture::new().await;
    let first = f.create("Shared alpha first").await;
    let second = f.create("Shared alpha second").await;
    let trashed = f.create("Shared alpha hidden").await;
    let operator = f.create("Operator OR private").await;
    let description_only = f.create("Description target").await;
    sqlx::query("UPDATE issues SET description = 'distinctive searchable phrase' WHERE id = ?")
        .bind(&description_only)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();

    let other_project = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHR", "Other project", None, Utc::now())
        .await
        .unwrap();
    IssueStore::new(f.db.pool().clone())
        .create(
            &f.owner,
            &other_project.id,
            NewIssue {
                title: "Shared alpha cross-project",
                description: None,
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();

    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let first_page = json(
        f.get(
            &format!("{path}?q=Shared%20alpha&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["data"][0]["id"], first);
    assert_eq!(first_page["data"][0]["issue_key"], "DEV-1");
    assert_eq!(first_page["has_more"], true);
    let cursor = first_page["next_cursor"].as_str().unwrap();
    for field in ["description", "history", "rank", "snippet"] {
        assert!(first_page["data"][0].get(field).is_none());
    }
    assert!(!first_page.to_string().contains("PRIVATE DESCRIPTION"));

    let second_page = json(
        f.get(
            &format!("{path}?q=Shared%20alpha&page_size=1&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(second_page["data"][0]["id"], second);
    assert_eq!(second_page["has_more"], false);
    assert!(second_page.get("next_cursor").is_none());

    let exact_key = json(
        f.get(&format!("{path}?q=dev-2"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(exact_key["data"].as_array().unwrap().len(), 1);
    assert_eq!(exact_key["data"][0]["issue_key"], "DEV-2");
    assert_eq!(exact_key["data"][0]["id"], second);

    let literal_operator = json(
        f.get(&format!("{path}?q=OR%20private"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(literal_operator["data"].as_array().unwrap().len(), 1);
    assert_eq!(literal_operator["data"][0]["id"], operator);
    let description = json(
        f.get(
            &format!("{path}?q=searchable%20phrase"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(description["data"][0]["id"], description_only);
    let trashed_search = json(
        f.get(&format!("{path}?q=hidden"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(trashed_search["data"].as_array().unwrap().is_empty());

    let response = f
        .get(&format!("{path}?q=Shared%20alpha"), Some(&f.token.token))
        .await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn project_search_filters_priority_alone_and_in_combination_with_text_or_key() {
    let f = Fixture::new().await;
    let urgent_shared = f.create("Shared urgent work").await;
    let high_shared = f.create("Shared high work").await;
    let urgent_other = f.create("Other urgent work").await;
    let trashed_urgent = f.create("Trashed urgent work").await;
    let low_issue = f.create("Low priority work").await;
    let medium_issue = f.create("Medium priority work").await;
    let none_issue = f.create("No priority work").await;
    for (id, priority) in [
        (&urgent_shared, "urgent"),
        (&high_shared, "high"),
        (&urgent_other, "urgent"),
        (&trashed_urgent, "urgent"),
        (&low_issue, "low"),
        (&medium_issue, "medium"),
    ] {
        sqlx::query("UPDATE issues SET priority = ? WHERE id = ?")
            .bind(priority)
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed_urgent)
        .execute(f.db.pool())
        .await
        .unwrap();

    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let urgent_page = json(
        f.get(
            &format!("{path}?priority=urgent&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(urgent_page["data"][0]["id"], urgent_shared);
    assert_eq!(urgent_page["has_more"], true);
    let cursor = urgent_page["next_cursor"].as_str().unwrap();
    let next_urgent_page = json(
        f.get(
            &format!("{path}?priority=urgent&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(next_urgent_page["data"][0]["id"], urgent_other);
    assert_eq!(next_urgent_page["has_more"], false);

    let text_and_priority = json(
        f.get(
            &format!("{path}?q=Shared&priority=urgent"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(text_and_priority["data"].as_array().unwrap().len(), 1);
    assert_eq!(text_and_priority["data"][0]["id"], urgent_shared);
    let key_and_priority = json(
        f.get(
            &format!("{path}?q=DEV-2&priority=high"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(key_and_priority["data"][0]["id"], high_shared);
    let mismatched_key_priority = json(
        f.get(
            &format!("{path}?q=DEV-2&priority=urgent"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert!(
        mismatched_key_priority["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for (priority, expected_id) in [
        ("none", &none_issue),
        ("low", &low_issue),
        ("medium", &medium_issue),
        ("high", &high_shared),
        ("urgent", &urgent_shared),
    ] {
        let filtered = json(
            f.get(&format!("{path}?priority={priority}"), Some(&f.token.token))
                .await,
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            filtered["data"][0]["id"],
            expected_id.as_str(),
            "{priority}"
        );
    }
}

#[tokio::test]
async fn project_search_filters_an_exact_due_date_and_preserves_keyset_pages() {
    let f = Fixture::new().await;
    let first = f.create("Shared urgent deadline").await;
    let second = f.create("Other urgent deadline").await;
    let later = f.create("Shared later deadline").await;
    let no_date = f.create("Shared without deadline").await;
    let trashed = f.create("Trashed deadline").await;
    for (id, due_date, priority) in [
        (&first, "2026-10-30", "urgent"),
        (&second, "2026-10-30", "urgent"),
        (&later, "2026-10-31", "urgent"),
        (&trashed, "2026-10-30", "urgent"),
    ] {
        sqlx::query("UPDATE issues SET due_date = ?, priority = ? WHERE id = ?")
            .bind(due_date)
            .bind(priority)
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();

    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let first_page = json(
        f.get(
            &format!("{path}?due_date=2026-10-30&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first_page["data"][0]["id"], first);
    assert_eq!(first_page["data"][0]["due_date"], "2026-10-30");
    assert_eq!(first_page["has_more"], true);
    let cursor = first_page["next_cursor"].as_str().unwrap();
    let second_page = json(
        f.get(
            &format!("{path}?due_date=2026-10-30&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(second_page["data"][0]["id"], second);
    assert_eq!(second_page["has_more"], false);
    assert_ne!(first_page["data"][0]["id"], no_date.as_str());
    assert_ne!(second_page["data"][0]["id"], no_date.as_str());

    let combined = json(
        f.get(
            &format!("{path}?q=Shared&priority=urgent&due_date=2026-10-30"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(combined["data"].as_array().unwrap().len(), 1);
    assert_eq!(combined["data"][0]["id"], first);
    let exact_key = json(
        f.get(
            &format!("{path}?q=DEV-3&priority=urgent&due_date=2026-10-31"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(exact_key["data"][0]["id"], later);
    let mismatch = json(
        f.get(
            &format!("{path}?q=DEV-3&priority=urgent&due_date=2026-10-30"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert!(mismatch["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn project_search_filters_an_exact_issue_type_including_archived_types() {
    let f = Fixture::new().await;
    let first = f.create("Shared bug one").await;
    let second = f.create("Shared bug two").await;
    let task = f.create("Shared task").await;
    let trashed = f.create("Shared bug hidden").await;
    let bug_type = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO issue_types (id, project_id, name, created_at, updated_at) VALUES (?, ?, 'Bug', 'now', 'now')",
    )
    .bind(&bug_type)
    .bind(&f.project)
    .execute(f.db.pool())
    .await
    .unwrap();
    for id in [&first, &second, &trashed] {
        sqlx::query("UPDATE issues SET issue_type_id = ? WHERE id = ?")
            .bind(&bug_type)
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issue_types SET archived_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&bug_type)
        .execute(f.db.pool())
        .await
        .unwrap();
    let other = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let foreign_type: String = sqlx::query_scalar(
        "SELECT id FROM issue_types WHERE project_id = ? AND system_key = 'task'",
    )
    .bind(&other.id)
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let first_page = json(
        f.get(
            &format!("{path}?issue_type_id={bug_type}&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first_page["data"][0]["id"], first);
    assert_eq!(first_page["data"][0]["issue_type_id"], bug_type);
    let cursor = first_page["next_cursor"].as_str().unwrap();
    let second_page = json(
        f.get(
            &format!("{path}?issue_type_id={bug_type}&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(second_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["data"][0]["id"], second);
    assert!(second_page.get("next_cursor").is_none());
    let combined = json(
        f.get(
            &format!("{path}?q=Shared&priority=none&issue_type_id={bug_type}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let combined_ids = combined["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(combined_ids, vec![first.clone(), second.clone()]);
    assert!(!combined_ids.contains(&task));
    let unknown = json(
        f.get(
            &format!("{path}?issue_type_id={}", Uuid::now_v7()),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(unknown["field_errors"].get("issue_type_id").is_some());
    let foreign = json(
        f.get(
            &format!("{path}?issue_type_id={foreign_type}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(foreign["field_errors"].get("issue_type_id").is_some());
    let outsider = user(&f.db, "member").await;
    let outsider_session = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    json(
        f.get(
            &format!("{path}?issue_type_id={bug_type}"),
            Some(&outsider_session.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn project_search_filters_an_exact_status_including_archived_statuses() {
    let f = Fixture::new().await;
    let first = f.create("Shared active one").await;
    let second = f.create("Shared active two").await;
    let todo = f.create("Shared todo").await;
    let trashed = f.create("Shared active hidden").await;
    let active_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND system_key = 'in_progress'",
    )
    .bind(&f.project)
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    for id in [&first, &second, &trashed] {
        sqlx::query("UPDATE issues SET status_id = ? WHERE id = ?")
            .bind(&active_status)
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET archived_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&active_status)
        .execute(f.db.pool())
        .await
        .unwrap();
    let other = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let foreign_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND system_key = 'in_progress'",
    )
    .bind(&other.id)
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let first_page = json(
        f.get(
            &format!("{path}?status_id={active_status}&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first_page["data"][0]["id"], first);
    assert_eq!(first_page["data"][0]["status_id"], active_status);
    let cursor = first_page["next_cursor"].as_str().unwrap();
    let second_page = json(
        f.get(
            &format!("{path}?status_id={active_status}&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(second_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["data"][0]["id"], second);
    assert!(second_page.get("next_cursor").is_none());
    let combined = json(
        f.get(
            &format!("{path}?q=Shared&priority=none&status_id={active_status}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let combined_ids = combined["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(combined_ids, vec![first.clone(), second.clone()]);
    assert!(!combined_ids.contains(&todo));
    let unknown = json(
        f.get(
            &format!("{path}?status_id={}", Uuid::now_v7()),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(unknown["field_errors"].get("status_id").is_some());
    let foreign = json(
        f.get(
            &format!("{path}?status_id={foreign_status}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(foreign["field_errors"].get("status_id").is_some());
    let outsider = user(&f.db, "member").await;
    let outsider_session = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    json(
        f.get(
            &format!("{path}?status_id={active_status}"),
            Some(&outsider_session.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn project_search_filters_an_exact_assignee_including_ineligible_members() {
    let f = Fixture::new().await;
    let first = f.create("Shared assigned one").await;
    let second = f.create("Shared assigned two").await;
    let owner_issue = f.create("Shared owner").await;
    let trashed = f.create("Shared assigned hidden").await;
    let assignee = user(&f.db, "member").await;
    member(&f.db, &f.project, &assignee, "member").await;
    let untouched = user(&f.db, "member").await;
    member(&f.db, &f.project, &untouched, "viewer").await;
    for id in [&first, &second, &trashed] {
        sqlx::query("UPDATE issues SET assignee_user_id = ? WHERE id = ?")
            .bind(&assignee)
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET assignee_user_id = ? WHERE id = ?")
        .bind(&f.owner)
        .bind(&owner_issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&assignee)
        .execute(f.db.pool())
        .await
        .unwrap();
    let other = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let foreign = user(&f.db, "member").await;
    member(&f.db, &other.id, &foreign, "member").await;
    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    let first_page = json(
        f.get(
            &format!("{path}?assignee_user_id={assignee}&page_size=1"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first_page["data"][0]["id"], first);
    assert_eq!(first_page["data"][0]["assignee_user_id"], assignee);
    let cursor = first_page["next_cursor"].as_str().unwrap();
    let second_page = json(
        f.get(
            &format!("{path}?assignee_user_id={assignee}&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(second_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["data"][0]["id"], second);
    assert!(second_page.get("next_cursor").is_none());
    let combined = json(
        f.get(
            &format!("{path}?q=Shared&priority=none&assignee_user_id={assignee}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let combined_ids = combined["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(combined_ids, vec![first.clone(), second.clone()]);
    assert!(!combined_ids.contains(&owner_issue));
    let unknown = json(
        f.get(
            &format!("{path}?assignee_user_id={}", Uuid::now_v7()),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(unknown["field_errors"].get("assignee_user_id").is_some());
    let foreign_response = json(
        f.get(
            &format!("{path}?assignee_user_id={foreign}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(
        foreign_response["field_errors"]
            .get("assignee_user_id")
            .is_some()
    );
    let empty_viewer = json(
        f.get(
            &format!("{path}?assignee_user_id={untouched}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty_viewer["data"].as_array().unwrap().len(), 0);
    let outsider = user(&f.db, "member").await;
    let outsider_session = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    json(
        f.get(
            &format!("{path}?assignee_user_id={assignee}"),
            Some(&outsider_session.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn project_search_revalidates_session_and_membership_and_hides_private_existence() {
    let f = Fixture::new().await;
    f.create("Private searchable issue").await;
    let admin = user(&f.db, "admin").await;
    let installation_admin = user(&f.db, "member").await;
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&installation_admin)
        .execute(f.db.pool())
        .await
        .unwrap();
    let viewer = user(&f.db, "guest").await;
    member(&f.db, &f.project, &viewer, "viewer").await;
    let sessions = SessionStore::new(f.db.pool().clone(), policy());
    let path = format!("/api/v1/projects/{}/issues/search?priority=none", f.project);

    let missing_path = format!(
        "/api/v1/projects/{}/issues/search?priority=none",
        Uuid::now_v7()
    );
    let missing = json(
        f.get(&missing_path, Some(&f.token.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    for actor in [&admin, &installation_admin] {
        let issued = sessions.issue(actor, Utc::now()).await.unwrap();
        let inaccessible = json(
            f.get(&path, Some(&issued.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        for field in ["code", "message", "field_errors", "retryable"] {
            assert_eq!(inaccessible[field], missing[field]);
        }
        assert!(
            !inaccessible
                .to_string()
                .contains("Private searchable issue")
        );
    }

    let viewer_session = sessions.issue(&viewer, Utc::now()).await.unwrap();
    let visible = json(
        f.get(&path, Some(&viewer_session.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(visible["data"].as_array().unwrap().len(), 1);
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&viewer)
        .execute(f.db.pool())
        .await
        .unwrap();
    let denied_after_removal = json(
        f.get(&path, Some(&viewer_session.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(denied_after_removal["code"], missing["code"]);
    let revoked = sessions
        .revoke_current(&f.token.token, Utc::now())
        .await
        .unwrap();
    assert!(revoked);
    json(
        f.get(&path, Some(&f.token.token)).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

#[tokio::test]
async fn project_search_rejects_invalid_queries_cursors_and_unsupported_methods() {
    let f = Fixture::new().await;
    f.create("Search input").await;
    let path = format!("/api/v1/projects/{}/issues/search", f.project);
    for (query, status) in [
        ("", StatusCode::UNPROCESSABLE_ENTITY),
        ("q=", StatusCode::UNPROCESSABLE_ENTITY),
        ("q=%20%20", StatusCode::UNPROCESSABLE_ENTITY),
        ("q=one%0Atwo", StatusCode::UNPROCESSABLE_ENTITY),
        ("priority=unknown", StatusCode::UNPROCESSABLE_ENTITY),
        ("due_date=", StatusCode::UNPROCESSABLE_ENTITY),
        ("due_date=2026-02-30", StatusCode::UNPROCESSABLE_ENTITY),
        ("due_date=2026-1-01", StatusCode::UNPROCESSABLE_ENTITY),
        ("q=&due_date=2026-10-30", StatusCode::UNPROCESSABLE_ENTITY),
        ("issue_type_id=", StatusCode::UNPROCESSABLE_ENTITY),
        ("issue_type_id=not-a-type", StatusCode::UNPROCESSABLE_ENTITY),
        (
            "issue_type_id=019A0000-0000-7000-8000-000000000003",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "issue_type_id=019a0000-0000-7000-8000-000000000003&issue_type_id=019a0000-0000-7000-8000-000000000004",
            StatusCode::BAD_REQUEST,
        ),
        ("status_id=", StatusCode::UNPROCESSABLE_ENTITY),
        ("status_id=not-a-status", StatusCode::UNPROCESSABLE_ENTITY),
        (
            "status_id=019A0000-0000-7000-8000-000000000002",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "status_id=019a0000-0000-7000-8000-000000000002&status_id=019a0000-0000-7000-8000-000000000003",
            StatusCode::BAD_REQUEST,
        ),
        ("assignee_user_id=", StatusCode::UNPROCESSABLE_ENTITY),
        (
            "assignee_user_id=not-an-assignee",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "assignee_user_id=019A0000-0000-7000-8000-000000000004",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "assignee_user_id=019a0000-0000-7000-8000-000000000004&assignee_user_id=019a0000-0000-7000-8000-000000000005",
            StatusCode::BAD_REQUEST,
        ),
        (
            "due_date=2026-10-30&due_date=2026-10-31",
            StatusCode::BAD_REQUEST,
        ),
        (
            "q=one&priority=high&priority=urgent",
            StatusCode::BAD_REQUEST,
        ),
        ("q=one&unknown=true", StatusCode::BAD_REQUEST),
        ("q=one&q=two", StatusCode::BAD_REQUEST),
        ("q=one&cursor=bad", StatusCode::UNPROCESSABLE_ENTITY),
        ("q=one&page_size=101", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let response = f
            .get(&format!("{path}?{query}"), Some(&f.token.token))
            .await;
        assert_eq!(response.status(), status, "{query}");
        let body = json_body_for_test(response).await;
        assert!(body.get("field_errors").is_some());
    }

    let head = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(format!("{path}?q=search"))
                .header(header::HOST, "tracker.example.test")
                .header(
                    header::COOKIE,
                    format!("__Host-puxiri_session={}", f.token.token.expose()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );

    let post = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("{path}?q=search"))
                .header(header::HOST, "tracker.example.test")
                .header(
                    header::COOKIE,
                    format!("__Host-puxiri_session={}", f.token.token.expose()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
}

async fn json_body_for_test(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
