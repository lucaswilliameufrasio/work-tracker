use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::Utc;
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    issues::{IssueStore, NewIssue, comments::CommentStore},
    projects::ProjectStore,
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(chrono::TimeDelta::minutes(5)).unwrap()
}

async fn user(database: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Comment fixture', 'active', 'now', 'now')",
    )
    .bind(&id)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, 'now')",
    )
    .bind(&id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
    id
}

async fn member(database: &Database, project: &str, user: &str, role: &str) {
    sqlx::query(
        "INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, ?, 'now')",
    )
    .bind(project)
    .bind(user)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
}

struct Fixture {
    _directory: TempDir,
    database: Database,
    owner: String,
    viewer: String,
    member: String,
    project: String,
    issue_number: i64,
    comments: CommentStore,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        sqlx::query(
            "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'Comments', 'pt-BR', 'America/Sao_Paulo', 'now', 'now')",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let owner = user(&database, "owner").await;
        let project = ProjectStore::new(database.pool().clone())
            .create(&owner, "COM", "Comentários", None, Utc::now())
            .await
            .unwrap();
        let issue = IssueStore::new(database.pool().clone())
            .create(
                &owner,
                &project.id,
                NewIssue {
                    title: "Primeira",
                    description: None,
                    assignee_user_id: None,
                },
                Utc::now(),
            )
            .await
            .unwrap();
        let viewer = user(&database, "member").await;
        member(&database, &project.id, &viewer, "viewer").await;
        let participant = user(&database, "member").await;
        member(&database, &project.id, &participant, "member").await;
        Self {
            comments: CommentStore::new(database.pool().clone()),
            _directory: directory,
            database,
            owner,
            viewer,
            member: participant,
            project: project.id,
            issue_number: issue.issue_number,
        }
    }
}

#[tokio::test]
async fn author_edits_manager_deletes_and_viewer_cannot_comment() {
    let fixture = Fixture::new().await;
    let now = Utc::now();
    let Err(rejected) = fixture
        .comments
        .create(
            &fixture.viewer,
            &fixture.project,
            fixture.issue_number,
            &Uuid::now_v7().to_string(),
            "não posso",
            now,
        )
        .await
    else {
        panic!("viewer must not comment");
    };
    assert!(matches!(
        rejected,
        puxiri::issues::comments::CommentError::Forbidden
    ));

    let key = Uuid::now_v7().to_string();
    let created = fixture
        .comments
        .create(
            &fixture.owner,
            &fixture.project,
            fixture.issue_number,
            &key,
            "  texto inicial  ",
            now,
        )
        .await
        .unwrap();
    assert!(!created.replayed);
    assert_eq!(created.result_version, Some(1));
    let replay = fixture
        .comments
        .create(
            &fixture.owner,
            &fixture.project,
            fixture.issue_number,
            &key,
            "texto inicial",
            now,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.comment_id, created.comment_id);
    let Err(conflict) = fixture
        .comments
        .create(
            &fixture.owner,
            &fixture.project,
            fixture.issue_number,
            &key,
            "outro texto",
            now,
        )
        .await
    else {
        panic!("same key must not rewrite");
    };
    assert!(matches!(
        conflict,
        puxiri::issues::comments::CommentError::IdempotencyConflict
    ));

    let Err(foreign_edit) = fixture
        .comments
        .edit(
            puxiri::issues::comments::CommentEdit {
                actor_user_id: &fixture.member,
                project_id: &fixture.project,
                issue_number: fixture.issue_number,
                comment_id: &created.comment_id,
                idempotency_key: &Uuid::now_v7().to_string(),
                expected_version: 1,
                body: "reescrever",
            },
            now,
        )
        .await
    else {
        panic!("another member must not rewrite");
    };
    assert!(matches!(
        foreign_edit,
        puxiri::issues::comments::CommentError::Forbidden
    ));
    let edited = fixture
        .comments
        .edit(
            puxiri::issues::comments::CommentEdit {
                actor_user_id: &fixture.owner,
                project_id: &fixture.project,
                issue_number: fixture.issue_number,
                comment_id: &created.comment_id,
                idempotency_key: &Uuid::now_v7().to_string(),
                expected_version: 1,
                body: "texto revisado",
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(edited.result_version, Some(2));
    let page = fixture
        .comments
        .list(
            &fixture.viewer,
            &fixture.project,
            fixture.issue_number,
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.comments.len(), 1);
    assert_eq!(page.comments[0].body, "texto revisado");
    assert!(!page.comments[0].can_edit);
    assert!(!page.comments[0].can_delete);
    assert!(
        page.events
            .iter()
            .any(|event| event.event_kind == "comment.edited")
    );
    let removed_body = page.comments[0].body.clone();

    let member_comment = fixture
        .comments
        .create(
            &fixture.member,
            &fixture.project,
            fixture.issue_number,
            &Uuid::now_v7().to_string(),
            "nota do membro",
            now,
        )
        .await
        .unwrap();
    fixture
        .comments
        .remove(
            &fixture.owner,
            &fixture.project,
            fixture.issue_number,
            &member_comment.comment_id,
            &Uuid::now_v7().to_string(),
            now,
        )
        .await
        .unwrap();
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT event_kind FROM issue_comment_events WHERE comment_id = ?")
            .bind(&member_comment.comment_id)
            .fetch_all(fixture.database.pool())
            .await
            .unwrap();
    assert_eq!(
        kinds,
        vec!["comment.created".to_owned(), "comment.removed".to_owned()]
    );
    assert!(!kinds.iter().any(|kind| kind.contains(&removed_body)));
}

#[tokio::test]
async fn http_create_hides_the_body_from_the_receipt_and_rejects_a_viewer() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Comments', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let owner = user(&database, "owner").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "WEB", "Web comments", None, Utc::now())
        .await
        .unwrap();
    let issue = IssueStore::new(database.pool().clone())
        .create(
            &owner,
            &project.id,
            NewIssue {
                title: "Visível",
                description: None,
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let viewer = user(&database, "member").await;
    member(&database, &project.id, &viewer, "viewer").await;
    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let owner_session = sessions.issue(&owner, Utc::now()).await.unwrap();
    let viewer_session = sessions.issue(&viewer, Utc::now()).await.unwrap();
    let app = http::router_with_browser_sessions(database.clone(), security.clone(), policy());
    let key = Uuid::now_v7().to_string();
    let created = app
        .clone()
        .oneshot(comment_request(
            &security,
            &owner_session.token,
            "POST",
            &format!(
                "/api/v1/projects/{}/issues/{}/comments",
                project.id, issue.issue_number
            ),
            &key,
            Some(r#"{"body":"privado"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let bytes = created.into_body().collect().await.unwrap().to_bytes();
    let receipt: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(receipt.get("body").is_none());
    assert_eq!(receipt["changed"], true);
    let denied = app
        .oneshot(comment_request(
            &security,
            &viewer_session.token,
            "POST",
            &format!(
                "/api/v1/projects/{}/issues/{}/comments",
                project.id, issue.issue_number
            ),
            &Uuid::now_v7().to_string(),
            Some(r#"{"body":"não"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

fn comment_request(
    security: &BrowserSessionSecurity,
    token: &puxiri::auth::SessionToken,
    method: &str,
    path: &str,
    key: &str,
    body: Option<&str>,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "tracker.example.test")
        .header(header::ORIGIN, "https://tracker.example.test")
        .header(
            header::COOKIE,
            format!("__Host-puxiri_session={}", token.expose()),
        )
        .header("x-csrf-token", security.csrf_token(token).expose())
        .header("idempotency-key", key)
        .body(Body::from(body.unwrap_or("").to_owned()))
        .unwrap();
    if body.is_some() {
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
    }
    request
}
