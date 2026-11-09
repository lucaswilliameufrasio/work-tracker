use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::{NaiveDate, Utc};
use http_body_util::BodyExt;
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    issues::{IssueStore, NewIssue, session::SessionIssueReader},
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
         VALUES (?, 'Assigned fixture', 'active', 'now', 'now')",
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

async fn stamp(database: &Database, issue_id: &str, due: Option<&str>, updated_at: &str) {
    sqlx::query("UPDATE issues SET due_date = ?, updated_at = ? WHERE id = ?")
        .bind(due)
        .bind(updated_at)
        .bind(issue_id)
        .execute(database.pool())
        .await
        .unwrap();
}

struct Fixture {
    _directory: TempDir,
    database: Database,
    owner: String,
    member: String,
    project: String,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        sqlx::query(
            "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'Items', 'pt-BR', 'America/Sao_Paulo', 'now', 'now')",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let owner = user(&database, "owner").await;
        let project = ProjectStore::new(database.pool().clone())
            .create(&owner, "ITM", "Itens", None, Utc::now())
            .await
            .unwrap();
        let participant = user(&database, "member").await;
        member(&database, &project.id, &participant, "member").await;
        Self {
            _directory: directory,
            database,
            owner,
            member: participant,
            project: project.id,
        }
    }

    async fn issue(&self, actor: &str, assignee: Option<&str>, title: &str) -> String {
        IssueStore::new(self.database.pool().clone())
            .create(
                actor,
                &self.project,
                NewIssue {
                    title,
                    description: None,
                    assignee_user_id: assignee,
                },
                Utc::now(),
            )
            .await
            .unwrap()
            .id
    }
}

#[tokio::test]
async fn assigned_items_follow_due_date_and_skip_inaccessible_work() {
    let fixture = Fixture::new().await;
    let same_day_older = fixture
        .issue(&fixture.owner, Some(&fixture.member), "mesmo dia antigo")
        .await;
    let same_day_newer = fixture
        .issue(&fixture.owner, Some(&fixture.member), "mesmo dia novo")
        .await;
    let later = fixture
        .issue(&fixture.owner, Some(&fixture.member), "depois")
        .await;
    let undated = fixture
        .issue(&fixture.owner, Some(&fixture.member), "sem prazo")
        .await;
    let trashed = fixture
        .issue(&fixture.owner, Some(&fixture.member), "lixo")
        .await;
    let owned_by_other = fixture
        .issue(&fixture.owner, Some(&fixture.owner), "de outra pessoa")
        .await;
    let created_only = fixture.issue(&fixture.member, None, "só criei").await;
    stamp(
        &fixture.database,
        &same_day_older,
        Some("2026-11-10"),
        "2026-11-02T00:00:00.000000000Z",
    )
    .await;
    stamp(
        &fixture.database,
        &same_day_newer,
        Some("2026-11-10"),
        "2026-11-03T00:00:00.000000000Z",
    )
    .await;
    stamp(
        &fixture.database,
        &later,
        Some("2026-12-01"),
        "2026-11-03T00:00:00.000000000Z",
    )
    .await;
    stamp(
        &fixture.database,
        &undated,
        None,
        "2026-11-04T00:00:00.000000000Z",
    )
    .await;
    stamp(
        &fixture.database,
        &trashed,
        Some("2026-01-01"),
        "2026-11-01T00:00:00.000000000Z",
    )
    .await;
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind("2026-11-01T00:00:00.000000000Z")
        .bind(&trashed)
        .execute(fixture.database.pool())
        .await
        .unwrap();
    let archived = ProjectStore::new(fixture.database.pool().clone())
        .create(&fixture.owner, "ARC", "Arquivo", None, Utc::now())
        .await
        .unwrap();
    member(&fixture.database, &archived.id, &fixture.member, "member").await;
    let hidden = IssueStore::new(fixture.database.pool().clone())
        .create(
            &fixture.owner,
            &archived.id,
            NewIssue {
                title: "arquivo",
                description: None,
                assignee_user_id: Some(&fixture.member),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    ProjectStore::new(fixture.database.pool().clone())
        .set_archived(&fixture.owner, &archived.id, true, Utc::now())
        .await
        .unwrap();
    let sessions = SessionStore::new(fixture.database.pool().clone(), policy());
    let session = sessions.issue(&fixture.member, Utc::now()).await.unwrap();
    let reader = SessionIssueReader::new(fixture.database.pool().clone(), policy());
    let first = reader
        .list_assigned_issues(&session.token, None, 2, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        vec![same_day_newer.as_str(), same_day_older.as_str()]
    );
    assert!(
        first
            .items
            .iter()
            .all(|issue| issue.due_date == Some(NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()))
    );
    let cursor = first.next_cursor.unwrap().encode();
    let second = reader
        .list_assigned_issues(&session.token, Some(&cursor), 2, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(|issue| issue.title.as_str())
            .collect::<Vec<_>>(),
        vec!["depois", "sem prazo"]
    );
    assert!(second.next_cursor.is_none());
    let titles: Vec<_> = first
        .items
        .iter()
        .chain(second.items.iter())
        .map(|issue| issue.id.as_str())
        .collect();
    assert!(!titles.contains(&trashed.as_str()));
    assert!(!titles.contains(&owned_by_other.as_str()));
    assert!(!titles.contains(&created_only.as_str()));
    assert!(!titles.contains(&hidden.id.as_str()));
}

#[tokio::test]
async fn http_lists_only_the_signed_in_assignee() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Items', 'en', 'UTC', 'now', 'now')",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let owner = user(&database, "owner").await;
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "WEB", "Web items", None, Utc::now())
        .await
        .unwrap();
    let participant = user(&database, "member").await;
    member(&database, &project.id, &participant, "member").await;
    let issue = IssueStore::new(database.pool().clone())
        .create(
            &owner,
            &project.id,
            NewIssue {
                title: "Meu item",
                description: Some("não listar"),
                assignee_user_id: Some(&participant),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let security = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    let sessions = SessionStore::new(database.pool().clone(), policy());
    let session = sessions.issue(&participant, Utc::now()).await.unwrap();
    let app = http::router_with_browser_sessions(database, security, policy());
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/me/issues")
                .header(header::HOST, "tracker.example.test")
                .header(header::ORIGIN, "https://tracker.example.test")
                .header(
                    header::COOKIE,
                    format!("__Host-puxiri_session={}", session.token.expose()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["id"], issue.id);
    assert_eq!(page["data"][0]["issue_key"], issue.issue_key);
    assert!(page["data"][0].get("description").is_none());
    assert!(page.get("next_cursor").is_none());
}
