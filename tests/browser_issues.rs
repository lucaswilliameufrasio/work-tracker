use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use chrono::{TimeDelta, Utc};
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        SessionToken,
        master_key::MasterKeyStore,
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    issues::{
        IssueStore, NewIssue,
        session::{IssueListOptions, SessionIssueReadError, SessionIssueReader},
    },
    projects::ProjectStore,
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

#[path = "browser_issues/creation.rs"]
mod creation;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

async fn user(db: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Issue API fixture', 'active', 'now', 'now')")
        .bind(&id).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')")
        .bind(&id).bind(role).execute(db.pool()).await.unwrap();
    id
}

async fn member(db: &Database, project: &str, user: &str, role: &str) {
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, ?, 'now')")
        .bind(project).bind(user).bind(role).execute(db.pool()).await.unwrap();
}

struct Fixture {
    _directory: TempDir,
    db: Arc<Database>,
    owner: String,
    project: String,
    token: IssuedSession,
    app: Router,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Issue API tests', 'en', 'UTC', 'now', 'now')").execute(db.pool()).await.unwrap();
        let owner = user(&db, "owner").await;
        let project = ProjectStore::new(db.pool().clone())
            .create(&owner, "DEV", "Private project", None, Utc::now())
            .await
            .unwrap()
            .id;
        let token = SessionStore::new(db.pool().clone(), policy())
            .issue(&owner, Utc::now())
            .await
            .unwrap();
        let security = BrowserSessionSecurity::new(
            "https://tracker.example.test",
            MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
        )
        .unwrap();
        let app = http::router_with_browser_sessions(db.clone(), security, policy());
        Self {
            _directory: directory,
            db,
            owner,
            project,
            token,
            app,
        }
    }

    fn path(&self, query: &str) -> String {
        format!("/api/v1/projects/{}/issues{query}", self.project)
    }

    async fn create(&self, title: &str) -> String {
        IssueStore::new(self.db.pool().clone())
            .create(
                &self.owner,
                &self.project,
                NewIssue {
                    title,
                    description: Some("PRIVATE DESCRIPTION NOT IN SUMMARY"),
                    assignee_user_id: None,
                },
                Utc::now(),
            )
            .await
            .unwrap()
            .id
    }

    async fn get(&self, path: &str, token: Option<&SessionToken>) -> Response {
        self.app
            .clone()
            .oneshot(request(path, token))
            .await
            .unwrap()
    }
}

fn request(path: &str, token: Option<&SessionToken>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(path)
        .header(header::HOST, "tracker.example.test");
    if let Some(token) = token {
        request = request.header(
            header::COOKIE,
            format!("__Host-puxiri_session={}", token.expose()),
        );
    }
    request.body(Body::empty()).unwrap()
}

async fn json(response: Response, status: StatusCode) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY)
    );
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn issue_api_paginates_private_summaries_and_preserves_large_i64_decimal_values() {
    let f = Fixture::new().await;
    let first = f.create("First <script>untrusted title</script>").await;
    let trashed = f.create("Hidden trash").await;
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trashed)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET next_issue_number = 9007199254740993 WHERE id = ?")
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    let large = f.create("Large issue number").await;
    sqlx::query("UPDATE issues SET version = 9223372036854775807, due_date = '2028-02-29', assignee_user_id = ? WHERE id = ?")
        .bind(&f.owner).bind(&large).execute(f.db.pool()).await.unwrap();
    let page = json(
        f.get(&f.path("?page_size=1"), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page["data"][0]["id"], first);
    assert_eq!(page["data"][0]["issue_number"], "1");
    assert_eq!(page["data"][0]["issue_key"], "DEV-1");
    assert_eq!(page["data"][0]["version"], "1");
    assert_eq!(page["next_cursor"], "1");
    assert!(page["data"][0].get("description").is_none());
    assert!(page["data"][0].get("history").is_none());
    assert!(page["data"][0].get("rank").is_none());
    assert!(page["data"][0].get("assignee_user_id").is_none());
    assert!(page["data"][0].get("due_date").is_none());
    let last = json(
        f.get(&f.path("?cursor=1&page_size=1"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(last["data"][0]["id"], large);
    assert_eq!(last["data"][0]["issue_number"], "9007199254740993");
    assert_eq!(last["data"][0]["version"], "9223372036854775807");
    assert_eq!(last["data"][0]["due_date"], "2028-02-29");
    assert_eq!(last["data"][0]["assignee_user_id"], f.owner);
    assert_eq!(last["has_more"], false);
    assert!(last.get("next_cursor").is_none());
    assert!(!last.to_string().contains("PRIVATE DESCRIPTION"));
    let empty = json(
        f.get(&f.path("?cursor=9223372036854775807"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(empty["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn project_membership_not_organization_or_installation_authority_controls_content() {
    let f = Fixture::new().await;
    f.create("Private title must not leak").await;
    let admin = user(&f.db, "admin").await;
    let installation = user(&f.db, "member").await;
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&installation)
        .execute(f.db.pool())
        .await
        .unwrap();
    let guest = user(&f.db, "guest").await;
    member(&f.db, &f.project, &guest, "viewer").await;
    let sessions = SessionStore::new(f.db.pool().clone(), policy());
    for actor in [&admin, &installation] {
        let session = sessions.issue(actor, Utc::now()).await.unwrap();
        let denied = json(
            f.get(&f.path(""), Some(&session.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        let missing = json(
            f.get(
                &format!("/api/v1/projects/{}/issues", Uuid::now_v7()),
                Some(&session.token),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
        for field in ["code", "message", "field_errors", "retryable"] {
            assert_eq!(denied[field], missing[field]);
        }
        assert!(!denied.to_string().contains("Private title"));
    }
    let session = sessions.issue(&guest, Utc::now()).await.unwrap();
    json(
        f.get(&f.path(""), Some(&session.token)).await,
        StatusCode::OK,
    )
    .await;
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    json(
        f.get(&f.path(""), Some(&session.token)).await,
        StatusCode::OK,
    )
    .await;
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&guest)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.get(&f.path(""), Some(&session.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    json(f.get(&f.path(""), None).await, StatusCode::UNAUTHORIZED).await;
}

#[tokio::test]
async fn issue_api_rejects_invalid_selectors_methods_and_transport_without_reflection() {
    let f = Fixture::new().await;
    for query in [
        "?actor_user_id=someone",
        "?include_trashed=true",
        "?page_size=1&page_size=2",
        "?page_size=bad",
        "?cursor=1&cursor=2",
    ] {
        json(
            f.get(&f.path(query), Some(&f.token.token)).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for query in [
        "?cursor=0",
        "?cursor=-1",
        "?cursor=01",
        "?cursor=%2B1",
        "?cursor=",
        "?cursor=9223372036854775808",
        "?page_size=0",
        "?page_size=101",
    ] {
        let error = json(
            f.get(&f.path(query), Some(&f.token.token)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert_eq!(error["code"], "invalid_params");
    }
    for id in ["invalid", "%FF", "00000000000000000000000000000000"] {
        let error = json(
            f.get(
                &format!("/api/v1/projects/{id}/issues"),
                Some(&f.token.token),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert!(error["field_errors"]["project_id"].is_string());
    }
    let mut hostile = request(&f.path(""), Some(&f.token.token));
    hostile.headers_mut().insert(
        header::ORIGIN,
        "https://attacker.example.test".parse().unwrap(),
    );
    json(
        f.app.clone().oneshot(hostile).await.unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    let mut payload = request(&f.path(""), Some(&f.token.token));
    *payload.body_mut() = Body::from("not accepted");
    json(
        f.app.clone().oneshot(payload).await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    for method in [Method::DELETE, Method::PATCH] {
        let mut req = request(&f.path(""), Some(&f.token.token));
        *req.method_mut() = method;
        let response = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD, POST");
        json(response, StatusCode::METHOD_NOT_ALLOWED).await;
    }
    let mut head = request(&f.path(""), Some(&f.token.token));
    *head.method_mut() = Method::HEAD;
    let response = f.app.clone().oneshot(head).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
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
        .oneshot(request(&f.path(""), Some(&f.token.token)))
        .await
        .unwrap();
    json(closed, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn session_bound_issue_queries_revalidate_without_touch_and_fail_closed_on_corrupt_storage() {
    let f = Fixture::new().await;
    let issue = f.create("Corruption fixture").await;
    let reader = SessionIssueReader::new(f.db.pool().clone(), policy());
    let options = IssueListOptions {
        after_issue_number: None,
        page_size: 50,
    };
    let before: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    reader
        .list_project_issues(
            &f.token.token,
            &f.project,
            options,
            Utc::now() + TimeDelta::minutes(10),
        )
        .await
        .unwrap();
    let after: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&f.token.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(before, after);
    assert!(matches!(
        reader
            .list_project_issues(
                &f.token.token,
                &f.project,
                options,
                Utc::now() + TimeDelta::hours(24)
            )
            .await,
        Err(SessionIssueReadError::Unauthenticated)
    ));
    sqlx::query("UPDATE issues SET due_date = '2026-02-31' WHERE id = ?")
        .bind(issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    let error = json(
        f.get(&f.path(""), Some(&f.token.token)).await,
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert_eq!(error["code"], "unexpected_error");
    assert!(!error.to_string().contains("2026-02-31"));
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .list_project_issues(&f.token.token, &f.project, options, Utc::now())
            .await,
        Err(SessionIssueReadError::Unauthenticated)
    ));
    f.db.pool().close().await;
    let error = json(
        f.get(&f.path(""), Some(&f.token.token)).await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error["code"], "storage_unavailable");
}

#[tokio::test]
async fn exact_maximum_page_is_bounded_and_next_page_can_include_new_issues() {
    let f = Fixture::new().await;
    for _ in 0..101 {
        f.create("Bounded list").await;
    }
    let page = json(
        f.get(&f.path("?page_size=100"), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 100);
    assert_eq!(page["next_cursor"], "100");
    f.create("Created after page one").await;
    let page = json(
        f.get(&f.path("?page_size=100&cursor=100"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    assert_eq!(page["data"][0]["issue_number"], "101");
    assert_eq!(page["data"][1]["issue_number"], "102");
    assert_eq!(page["has_more"], false);
    let status: String =
        sqlx::query_scalar("SELECT status_id FROM issues WHERE project_id = ? LIMIT 1")
            .bind(&f.project)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    let board = json(
        f.get(
            &format!(
                "/api/v1/projects/{}/board/{status}?page_size=100",
                f.project
            ),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(board["data"].as_array().unwrap().len(), 100);
    assert_eq!(board["total_count"], "102");
    assert_eq!(board["has_more"], true);
}

#[tokio::test]
async fn direct_detail_and_history_include_retained_trash_but_do_not_embed_unbounded_journals() {
    let f = Fixture::new().await;
    let issue = f.create("Visible private detail").await;
    sqlx::query("UPDATE issues SET description = ?, completed_at = ?, trashed_at = ? WHERE id = ?")
        .bind("é".repeat(32768))
        .bind(Utc::now().to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(&issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    let detail = json(
        f.get(&f.path("/1"), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["id"], issue);
    assert_eq!(detail["description"].as_str().unwrap().len(), 65536);
    assert!(detail["completed_at"].is_string());
    assert!(detail["trashed_at"].is_string());
    assert!(detail.get("history").is_none());
    let history = json(
        f.get(&f.path("/1/history?page_size=100"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(history["data"].as_array().unwrap().len(), 1);
    assert_eq!(history["data"][0]["issue_id"], issue);
    assert_eq!(history["data"][0]["event_kind"], "issue.created");
    assert_eq!(history["data"][0].as_object().unwrap().len(), 6);
    assert!(history.get("next_cursor").is_none());
    assert_eq!(history["has_more"], false);
    let list = json(
        f.get(&f.path(""), Some(&f.token.token)).await,
        StatusCode::OK,
    )
    .await;
    assert!(list["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn history_cursor_boundaries_are_canonical_scoped_and_not_authorization_tokens() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use puxiri::issues::IssueHistoryCursor;
    let f = Fixture::new().await;
    f.create("History fixture").await;
    let history = json(
        f.get(&f.path("/1/history?page_size=1"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    let event = &history["data"][0];
    let encoded = URL_SAFE_NO_PAD.encode(format!(
        "{}/{}",
        event["occurred_at"].as_str().unwrap(),
        event["id"].as_str().unwrap()
    ));
    assert_eq!(encoded.len(), 90);
    assert_eq!(
        IssueHistoryCursor::decode(&encoded).unwrap().encode(),
        encoded
    );
    let last = json(
        f.get(
            &f.path(&format!("/1/history?cursor={encoded}")),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert!(last["data"].as_array().unwrap().is_empty());
    let before = URL_SAFE_NO_PAD
        .encode("2000-01-01T00:00:00.000000000Z/00000000-0000-0000-0000-000000000000");
    let first = json(
        f.get(
            &f.path(&format!("/1/history?cursor={before}")),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["data"].as_array().unwrap().len(), 1);
    let plan = sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM issue_events WHERE project_id = ? AND issue_id = ? AND (occurred_at, id) > (?, ?) ORDER BY occurred_at ASC, id ASC LIMIT ?")
        .bind(&f.project).bind(event["issue_id"].as_str().unwrap()).bind("").bind("").bind(101_i64).fetch_all(f.db.pool()).await.unwrap();
    use sqlx::Row;
    let details: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
    assert!(
        details
            .iter()
            .any(|line| line.contains("issue_events_issue_time_idx"))
    );
    assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
    let admin = user(&f.db, "admin").await;
    let other = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&admin, Utc::now())
        .await
        .unwrap();
    for suffix in ["/1".to_owned(), format!("/1/history?cursor={before}")] {
        json(
            f.get(&f.path(&suffix), Some(&other.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        json(
            f.get(&f.path(&suffix), None).await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }
    for cursor in [
        "bad".to_owned(),
        format!("{encoded}="),
        URL_SAFE_NO_PAD
            .encode("2026-99-99T00:00:00.000000000Z/00000000-0000-0000-0000-000000000000"),
        URL_SAFE_NO_PAD
            .encode("2026-01-01T00:00:00.000000000Z/gggggggg-gggg-gggg-gggg-gggggggggggg"),
    ] {
        json(
            f.get(
                &f.path(&format!("/1/history?cursor={cursor}")),
                Some(&f.token.token),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
}

#[tokio::test]
async fn detail_and_history_reject_invalid_inputs_and_corrupt_storage_without_leaking_content() {
    let f = Fixture::new().await;
    let issue = f.create("Do not leak private detail on errors").await;
    for suffix in [
        "/0",
        "/01",
        "/-1",
        "/9223372036854775808",
        "/%FF",
        "/0/history",
        "/1/history?page_size=0",
        "/1/history?page_size=101",
    ] {
        json(
            f.get(&f.path(suffix), Some(&f.token.token)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for suffix in [
        "/1?cursor=1",
        "/1/history?user_id=other",
        "/1/history?page_size=1&page_size=2",
    ] {
        json(
            f.get(&f.path(suffix), Some(&f.token.token)).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for suffix in ["/999", "/999/history"] {
        let error = json(
            f.get(&f.path(suffix), Some(&f.token.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        assert!(!error.to_string().contains("private detail"));
    }
    for suffix in ["/1", "/1/history"] {
        let mut head = request(&f.path(suffix), Some(&f.token.token));
        *head.method_mut() = Method::HEAD;
        let response = f.app.clone().oneshot(head).await.unwrap();
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
        let mut post = request(&f.path(suffix), Some(&f.token.token));
        *post.method_mut() = Method::POST;
        let response = f.app.clone().oneshot(post).await.unwrap();
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
        json(response, StatusCode::METHOD_NOT_ALLOWED).await;
        json(
            http::router(f.db.clone())
                .oneshot(request(&f.path(suffix), Some(&f.token.token)))
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    sqlx::query(
        "UPDATE issue_events SET occurred_at = 'malformed private timestamp' WHERE issue_id = ?",
    )
    .bind(&issue)
    .execute(f.db.pool())
    .await
    .unwrap();
    for suffix in ["/1", "/1/history"] {
        let error = json(
            f.get(&f.path(suffix), Some(&f.token.token)).await,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        assert!(!error.to_string().contains("private timestamp"));
    }
}

#[tokio::test]
async fn project_catalogs_expose_bounded_schema_with_lossless_positions_and_optional_omission() {
    let f = Fixture::new().await;
    let base = format!("/api/v1/projects/{}", f.project);
    let statuses = json(
        f.get(&format!("{base}/statuses"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    let items = statuses["data"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(
        items
            .iter()
            .filter(|item| item["board_visible"] == true)
            .count(),
        3
    );
    let mut previous = "";
    for status in items {
        let id = status["id"].as_str().unwrap();
        assert!(id > previous);
        previous = id;
        assert_eq!(status["project_id"], f.project);
        assert!(status["position"].is_string());
        assert!(status.get("archived_at").is_none());
        assert!(status.get("icon_key").is_none());
    }
    let types = json(
        f.get(&format!("{base}/issue-types"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(types["data"][0]["name"], "Task");
    assert_eq!(types["data"][0]["system_key"], "task");
    assert!(types["data"][0].get("icon_key").is_none());
    assert!(types["data"][0].get("color_key").is_none());
    let id = items[0]["id"].as_str().unwrap();
    sqlx::query("UPDATE statuses SET position = 9223372036854775807, archived_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issue_types SET icon_key = 'opaque_icon', color_key = 'opaque-color', archived_at = ? WHERE project_id = ?").bind(Utc::now().to_rfc3339()).bind(&f.project).execute(f.db.pool()).await.unwrap();
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    let visible = json(
        f.get(&format!("{base}/statuses"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(visible["data"].as_array().unwrap().len(), 4);
    let all = json(
        f.get(
            &format!("{base}/statuses?include_archived=true"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(all["data"][0]["position"], "9223372036854775807");
    assert!(all["data"][0]["archived_at"].is_string());
    let hidden = json(
        f.get(&format!("{base}/issue-types"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(hidden["data"].as_array().unwrap().is_empty());
    let all = json(
        f.get(
            &format!("{base}/issue-types?include_archived=true"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(all["data"][0]["icon_key"], "opaque_icon");
    assert_eq!(all["data"][0]["color_key"], "opaque-color");
}

#[tokio::test]
async fn catalog_keysets_use_existing_indexes_and_exact_pages_without_expanding_project_scope() {
    use sqlx::Row;
    let f = Fixture::new().await;
    for number in 0..101 {
        sqlx::query("INSERT INTO issue_types (id, project_id, name, created_at, updated_at) VALUES (?, ?, ?, 'now', 'now')")
            .bind(Uuid::now_v7().to_string()).bind(&f.project).bind(format!("Custom {number}")).execute(f.db.pool()).await.unwrap();
    }
    let base = format!("/api/v1/projects/{}", f.project);
    let first = json(
        f.get(
            &format!("{base}/issue-types?page_size=100"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["data"].as_array().unwrap().len(), 100);
    assert_eq!(first["next_cursor"], first["data"][99]["id"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let end = json(
        f.get(
            &format!("{base}/issue-types?page_size=100&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(end["data"].as_array().unwrap().len(), 2);
    assert_eq!(end["has_more"], false);
    assert!(end.get("next_cursor").is_none());
    for query in [
        "EXPLAIN QUERY PLAN SELECT id FROM statuses WHERE project_id = ? AND id > ? AND (? OR archived_at IS NULL) ORDER BY id ASC LIMIT ?",
        "EXPLAIN QUERY PLAN SELECT id FROM issue_types WHERE project_id = ? AND id > ? AND (? OR archived_at IS NULL) ORDER BY id ASC LIMIT ?",
    ] {
        let plan = sqlx::query(query)
            .bind(&f.project)
            .bind(cursor)
            .bind(false)
            .bind(101_i64)
            .fetch_all(f.db.pool())
            .await
            .unwrap();
        let details: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
        assert!(
            details
                .iter()
                .any(|line| line.contains("project_id=? AND id>?")),
            "{details:?}"
        );
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
    }
    let admin = user(&f.db, "admin").await;
    let guest = user(&f.db, "guest").await;
    member(&f.db, &f.project, &guest, "viewer").await;
    let sessions = SessionStore::new(f.db.pool().clone(), policy());
    let denied = sessions.issue(&admin, Utc::now()).await.unwrap();
    let viewer = sessions.issue(&guest, Utc::now()).await.unwrap();
    for endpoint in ["statuses", "issue-types"] {
        let path = format!("{base}/{endpoint}");
        let inaccessible = json(
            f.get(&path, Some(&denied.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        let missing = json(
            f.get(
                &format!("/api/v1/projects/{}/{endpoint}", Uuid::now_v7()),
                Some(&denied.token),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
        for field in ["code", "message", "field_errors", "retryable"] {
            assert_eq!(inaccessible[field], missing[field]);
        }
        json(f.get(&path, Some(&viewer.token)).await, StatusCode::OK).await;
        json(f.get(&path, None).await, StatusCode::UNAUTHORIZED).await;
    }
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&guest)
        .execute(f.db.pool())
        .await
        .unwrap();
    for endpoint in ["statuses", "issue-types"] {
        json(
            f.get(
                &format!("{base}/{endpoint}?include_archived=true"),
                Some(&viewer.token),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    json(
        f.get(&format!("{base}/statuses"), Some(&f.token.token))
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

#[tokio::test]
async fn catalog_routes_reject_invalid_inputs_methods_and_corruption_with_closed_opt_in() {
    let f = Fixture::new().await;
    for endpoint in ["statuses", "issue-types"] {
        let path = format!("/api/v1/projects/{}/{endpoint}", f.project);
        for query in ["?page_size=0", "?page_size=101", "?cursor=bad"] {
            json(
                f.get(&format!("{path}{query}"), Some(&f.token.token)).await,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
        }
        for query in [
            "?include_archived=bad",
            "?actor_user_id=other",
            "?cursor=a&cursor=b",
        ] {
            json(
                f.get(&format!("{path}{query}"), Some(&f.token.token)).await,
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
        json(
            f.get(
                &format!("/api/v1/projects/bad/{endpoint}"),
                Some(&f.token.token),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        let mut head = request(&path, Some(&f.token.token));
        *head.method_mut() = Method::HEAD;
        let response = f.app.clone().oneshot(head).await.unwrap();
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
        let mut post = request(&path, Some(&f.token.token));
        *post.method_mut() = Method::POST;
        let response = f.app.clone().oneshot(post).await.unwrap();
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
        json(response, StatusCode::METHOD_NOT_ALLOWED).await;
        json(
            http::router(f.db.clone())
                .oneshot(request(&path, Some(&f.token.token)))
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    sqlx::query("UPDATE statuses SET archived_at = 'PRIVATE corrupt time' WHERE project_id = ?")
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issue_types SET archived_at = 'PRIVATE corrupt time' WHERE project_id = ?")
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    for endpoint in ["statuses", "issue-types"] {
        let error = json(
            f.get(
                &format!(
                    "/api/v1/projects/{}/{endpoint}?include_archived=true",
                    f.project
                ),
                Some(&f.token.token),
            )
            .await,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        assert!(!error.to_string().contains("PRIVATE"));
    }
}

#[tokio::test]
async fn catalog_record_reads_resolve_archived_references_but_never_foreign_or_private_ids() {
    let f = Fixture::new().await;
    let foreign = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHER", "Other catalog", None, Utc::now())
        .await
        .unwrap();
    let outsider = user(&f.db, "admin").await;
    let denied = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    for endpoint in ["statuses", "issue-types"] {
        let base = format!("/api/v1/projects/{}/{endpoint}", f.project);
        let catalog = json(f.get(&base, Some(&f.token.token)).await, StatusCode::OK).await;
        let id = catalog["data"][0]["id"].as_str().unwrap();
        let path = format!("{base}/{id}");
        let record = json(f.get(&path, Some(&f.token.token)).await, StatusCode::OK).await;
        assert_eq!(record, catalog["data"][0]);
        let query = if endpoint == "statuses" {
            "UPDATE statuses SET archived_at = ? WHERE id = ?"
        } else {
            "UPDATE issue_types SET archived_at = ? WHERE id = ?"
        };
        sqlx::query(query)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
        assert!(
            json(f.get(&path, Some(&f.token.token)).await, StatusCode::OK).await["archived_at"]
                .is_string()
        );
        let unavailable = json(
            f.get(&path, Some(&denied.token)).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        let foreign_record = json(
            f.get(
                &format!("/api/v1/projects/{}/{endpoint}/{id}", foreign.id),
                Some(&f.token.token),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
        let missing = json(
            f.get(&format!("{base}/{}", Uuid::now_v7()), Some(&f.token.token))
                .await,
            StatusCode::NOT_FOUND,
        )
        .await;
        for field in ["code", "message", "field_errors", "retryable"] {
            assert_eq!(unavailable[field], missing[field]);
            assert_eq!(foreign_record[field], missing[field]);
        }
        json(f.get(&path, None).await, StatusCode::UNAUTHORIZED).await;
        json(
            f.get(&format!("{base}/bad"), Some(&f.token.token)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        json(
            f.get(
                &format!("{path}?include_archived=true"),
                Some(&f.token.token),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let mut head = request(&path, Some(&f.token.token));
        *head.method_mut() = Method::HEAD;
        let response = f.app.clone().oneshot(head).await.unwrap();
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
        let mut post = request(&path, Some(&f.token.token));
        *post.method_mut() = Method::POST;
        json(
            f.app.clone().oneshot(post).await.unwrap(),
            StatusCode::METHOD_NOT_ALLOWED,
        )
        .await;
        json(
            http::router(f.db.clone())
                .oneshot(request(&path, Some(&f.token.token)))
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
        sqlx::query(query)
            .bind("PRIVATE corrupt time")
            .bind(id)
            .execute(f.db.pool())
            .await
            .unwrap();
        let error = json(
            f.get(&path, Some(&f.token.token)).await,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        assert!(!error.to_string().contains("PRIVATE"));
    }
    use puxiri::projects::catalog::{CatalogKind, CatalogReadError, SessionProjectCatalogReader};
    let reader = SessionProjectCatalogReader::new(f.db.pool().clone(), policy());
    let selected = Uuid::now_v7().to_string();
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
        assert!(matches!(
            reader
                .get(&f.token.token, &f.project, kind, &selected, Utc::now())
                .await,
            Err(CatalogReadError::Unauthenticated)
        ));
    }
    let fresh = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&f.owner, Utc::now())
        .await
        .unwrap();
    for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
        assert!(matches!(
            reader
                .get(
                    &fresh.token,
                    &f.project,
                    kind,
                    &selected,
                    Utc::now() + TimeDelta::days(8)
                )
                .await,
            Err(CatalogReadError::Unauthenticated)
        ));
    }
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&f.owner)
        .execute(f.db.pool())
        .await
        .unwrap();
    for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
        assert!(matches!(
            reader
                .get(&fresh.token, &f.project, kind, &selected, Utc::now())
                .await,
            Err(CatalogReadError::Unauthenticated)
        ));
    }
}

#[tokio::test]
async fn board_column_pages_use_signed_rank_and_uuid_order_with_current_counts_and_no_trash() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sqlx::Row;
    let f = Fixture::new().await;
    let trash = f.create("Do not count trash").await;
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&trash)
        .execute(f.db.pool())
        .await
        .unwrap();
    let mut expected = Vec::new();
    let mut status = String::new();
    for (title, rank) in [
        ("Minimum", i64::MIN),
        ("Tie A", 0),
        ("Tie B", 0),
        ("Maximum", i64::MAX),
    ] {
        let id = f.create(title).await;
        sqlx::query("UPDATE issues SET rank = ? WHERE id = ?")
            .bind(rank)
            .bind(&id)
            .execute(f.db.pool())
            .await
            .unwrap();
        status = sqlx::query_scalar("SELECT status_id FROM issues WHERE id = ?")
            .bind(&id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
        expected.push((rank, id));
    }
    expected.sort();
    let path = format!("/api/v1/projects/{}/board/{status}", f.project);
    let first = json(
        f.get(&format!("{path}?page_size=2"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["total_count"], "4");
    assert_eq!(first["data"][0]["id"], expected[0].1);
    assert_eq!(first["data"][0]["rank"], i64::MIN.to_string());
    assert_eq!(first["data"][1]["id"], expected[1].1);
    assert!(first["data"][0].get("description").is_none());
    let cursor = first["next_cursor"].as_str().unwrap();
    assert_eq!(
        puxiri::issues::BoardCursor::decode(cursor)
            .unwrap()
            .encode(),
        cursor
    );
    let last = json(
        f.get(
            &format!("{path}?page_size=2&cursor={cursor}"),
            Some(&f.token.token),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(last["data"][0]["id"], expected[2].1);
    assert_eq!(last["data"][1]["id"], expected[3].1);
    assert_eq!(last["data"][1]["rank"], i64::MAX.to_string());
    assert_eq!(last["total_count"], "4");
    assert_eq!(last["has_more"], false);
    assert!(last.get("next_cursor").is_none());
    let boundary =
        URL_SAFE_NO_PAD.encode("9223372036854775807/ffffffff-ffff-ffff-ffff-ffffffffffff");
    let empty = json(
        f.get(&format!("{path}?cursor={boundary}"), Some(&f.token.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(empty["data"].as_array().unwrap().is_empty());
    assert_eq!(empty["total_count"], "4");
    let plan = sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND (rank, id) > (?, ?) ORDER BY rank ASC, id ASC LIMIT ?")
        .bind(&f.project).bind(&status).bind(i64::MIN).bind("").bind(101_i64).fetch_all(f.db.pool()).await.unwrap();
    let detail: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
    assert!(
        detail
            .iter()
            .any(|line| line.contains("issues_project_status_rank_idx")),
        "{detail:?}"
    );
    assert!(!detail.iter().any(|line| line.contains("TEMP B-TREE")));
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    json(f.get(&path, Some(&f.token.token)).await, StatusCode::OK).await;
}

#[tokio::test]
async fn board_column_authorization_and_eligibility_do_not_reveal_foreign_hidden_or_archived_statuses()
 {
    let f = Fixture::new().await;
    f.create("Private board title").await;
    let status: String =
        sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = 'todo'")
            .bind(&f.project)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    let path = format!("/api/v1/projects/{}/board/{status}", f.project);
    let outsider = user(&f.db, "admin").await;
    let guest = user(&f.db, "guest").await;
    member(&f.db, &f.project, &guest, "viewer").await;
    let sessions = SessionStore::new(f.db.pool().clone(), policy());
    let denied = sessions.issue(&outsider, Utc::now()).await.unwrap();
    let viewer = sessions.issue(&guest, Utc::now()).await.unwrap();
    let absent = json(
        f.get(
            &format!("/api/v1/projects/{}/board/{}", f.project, Uuid::now_v7()),
            Some(&f.token.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
    let private = json(
        f.get(&path, Some(&denied.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    for field in ["code", "message", "field_errors", "retryable"] {
        assert_eq!(absent[field], private[field]);
    }
    json(f.get(&path, Some(&viewer.token)).await, StatusCode::OK).await;
    for key in ["backlog", "canceled"] {
        let hidden: String =
            sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = ?")
                .bind(&f.project)
                .bind(key)
                .fetch_one(f.db.pool())
                .await
                .unwrap();
        json(
            f.get(
                &format!("/api/v1/projects/{}/board/{hidden}", f.project),
                Some(&f.token.token),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let other = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHER", "Other board", None, Utc::now())
        .await
        .unwrap();
    json(
        f.get(
            &format!("/api/v1/projects/{}/board/{status}", other.id),
            Some(&f.token.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
    sqlx::query("UPDATE statuses SET archived_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&status)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.get(&path, Some(&f.token.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&guest)
        .execute(f.db.pool())
        .await
        .unwrap();
    json(
        f.get(&path, Some(&viewer.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    json(f.get(&path, None).await, StatusCode::UNAUTHORIZED).await;
}

#[tokio::test]
async fn board_column_rejects_malformed_boundaries_methods_and_corrupt_rows_safely() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let f = Fixture::new().await;
    let issue = f.create("Board corruption fixture").await;
    let status: String = sqlx::query_scalar("SELECT status_id FROM issues WHERE id = ?")
        .bind(&issue)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    let path = format!("/api/v1/projects/{}/board/{status}", f.project);
    for raw in [
        format!("+1/{issue}"),
        format!("01/{issue}"),
        format!("-0/{issue}"),
        format!("9223372036854775808/{issue}"),
        format!("-9223372036854775809/{issue}"),
        "0/bad".to_owned(),
    ] {
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        json(
            f.get(&format!("{path}?cursor={encoded}"), Some(&f.token.token))
                .await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for query in ["?cursor=bad", "?page_size=0", "?page_size=101"] {
        json(
            f.get(&format!("{path}{query}"), Some(&f.token.token)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for query in [
        "?rank=1",
        "?actor_user_id=other",
        "?page_size=1&page_size=2",
    ] {
        json(
            f.get(&format!("{path}{query}"), Some(&f.token.token)).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    json(
        f.get(
            &format!("/api/v1/projects/{}/board/bad", f.project),
            Some(&f.token.token),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let mut head = request(&path, Some(&f.token.token));
    *head.method_mut() = Method::HEAD;
    let response = f.app.clone().oneshot(head).await.unwrap();
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
    let mut post = request(&path, Some(&f.token.token));
    *post.method_mut() = Method::POST;
    json(
        f.app.clone().oneshot(post).await.unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    json(
        http::router(f.db.clone())
            .oneshot(request(&path, Some(&f.token.token)))
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    sqlx::query("UPDATE issues SET due_date = '2026-02-31' WHERE id = ?")
        .bind(&issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    let error = json(
        f.get(&path, Some(&f.token.token)).await,
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert!(!error.to_string().contains("2026-02-31"));
    let reader = SessionIssueReader::new(f.db.pool().clone(), policy());
    let options = puxiri::issues::BoardOptions {
        after: None,
        page_size: 20,
    };
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        reader
            .read_board_column(&f.token.token, &f.project, &status, options, Utc::now())
            .await,
        Err(SessionIssueReadError::Unauthenticated)
    ));
    f.db.pool().close().await;
    json(
        f.get(&path, Some(&f.token.token)).await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
}

#[tokio::test]
async fn board_status_pages_are_globally_position_ordered_filtered_and_private() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sqlx::Row;
    let f = Fixture::new().await;
    let base = format!("/api/v1/projects/{}/board", f.project);
    sqlx::query("UPDATE statuses SET position = CASE system_key WHEN 'todo' THEN 9223372036854775807 ELSE 0 END WHERE project_id = ?")
        .bind(&f.project).execute(f.db.pool()).await.unwrap();
    let selected = sqlx::query("SELECT id, position FROM statuses WHERE project_id = ? AND board_visible = 1 ORDER BY position ASC, id ASC")
        .bind(&f.project).fetch_all(f.db.pool()).await.unwrap();
    assert_eq!(selected.len(), 3);
    let mut cursor = None;
    for row in &selected {
        let path = match cursor.as_deref() {
            Some(cursor) => format!("{base}?page_size=1&cursor={cursor}"),
            None => format!("{base}?page_size=1"),
        };
        let page = json(f.get(&path, Some(&f.token.token)).await, StatusCode::OK).await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        assert_eq!(page["data"][0]["id"], row.get::<String, _>("id"));
        assert_eq!(
            page["data"][0]["position"],
            row.get::<i64, _>("position").to_string()
        );
        assert_eq!(page["data"][0]["board_visible"], true);
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if let Some(value) = cursor.as_deref() {
            assert_eq!(
                puxiri::projects::catalog::BoardStatusCursor::decode(value)
                    .unwrap()
                    .encode(),
                value
            );
        }
    }
    assert!(cursor.is_none());
    let ordered = json(f.get(&base, Some(&f.token.token)).await, StatusCode::OK).await;
    assert_eq!(ordered["data"].as_array().unwrap().len(), 3);
    assert_eq!(ordered["has_more"], false);
    let last: String = selected[2].get("id");
    sqlx::query("UPDATE statuses SET archived_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&last)
        .execute(f.db.pool())
        .await
        .unwrap();
    let page = json(f.get(&base, Some(&f.token.token)).await, StatusCode::OK).await;
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    for value in [
        format!("-1/{last}"),
        format!("-0/{last}"),
        format!("01/{last}"),
        format!("9223372036854775808/{last}"),
        format!("+1/{last}"),
    ] {
        let encoded = URL_SAFE_NO_PAD.encode(value);
        json(
            f.get(&format!("{base}?cursor={encoded}"), Some(&f.token.token))
                .await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for query in ["?page_size=0", "?page_size=101", "?cursor=bad"] {
        json(
            f.get(&format!("{base}{query}"), Some(&f.token.token)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for query in [
        "?include_archived=true",
        "?user_id=other",
        "?cursor=a&cursor=b",
    ] {
        json(
            f.get(&format!("{base}{query}"), Some(&f.token.token)).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let outsider = user(&f.db, "admin").await;
    let denied = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&outsider, Utc::now())
        .await
        .unwrap();
    let private = json(
        f.get(&base, Some(&denied.token)).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    let missing = json(
        f.get(
            &format!("/api/v1/projects/{}/board", Uuid::now_v7()),
            Some(&f.token.token),
        )
        .await,
        StatusCode::NOT_FOUND,
    )
    .await;
    for field in ["code", "message", "field_errors", "retryable"] {
        assert_eq!(private[field], missing[field]);
    }
    json(f.get(&base, None).await, StatusCode::UNAUTHORIZED).await;
    let mut head = request(&base, Some(&f.token.token));
    *head.method_mut() = Method::HEAD;
    let response = f.app.clone().oneshot(head).await.unwrap();
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
    json(
        http::router(f.db.clone())
            .oneshot(request(&base, Some(&f.token.token)))
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    let plan = sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM statuses WHERE project_id = ? AND archived_at IS NULL AND board_visible = 1 AND (position, id) > (?, ?) ORDER BY position ASC, id ASC LIMIT ?")
        .bind(&f.project).bind(0_i64).bind("").bind(101_i64).fetch_all(f.db.pool()).await.unwrap();
    let details: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
    assert!(
        details
            .iter()
            .any(|line| line.contains("statuses_project_order_idx")),
        "{details:?}"
    );
    assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    json(f.get(&base, Some(&f.token.token)).await, StatusCode::OK).await;
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.token.id, Utc::now())
        .await
        .unwrap();
    let reader =
        puxiri::projects::catalog::SessionProjectCatalogReader::new(f.db.pool().clone(), policy());
    assert!(matches!(
        reader
            .board_statuses(&f.token.token, &f.project, None, 4, Utc::now())
            .await,
        Err(puxiri::projects::catalog::CatalogReadError::Unauthenticated)
    ));
}
