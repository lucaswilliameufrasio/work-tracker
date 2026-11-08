use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use chrono::{TimeDelta, Utc};
use http_body_util::BodyExt;
use puxiri::{
    access::ProjectRole,
    auth::{
        SessionToken,
        master_key::MasterKeyStore,
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    http::{self, browser_session::BrowserSessionSecurity},
    projects::{ProjectStore, membership::ProjectMembershipStore},
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

async fn create_user(database: &Database, name: &str, organization_role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', 'now', 'now')",
    )
    .bind(&user_id)
    .bind(name)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, 'now')",
    )
    .bind(&user_id)
    .bind(organization_role)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn add_email(database: &Database, user_id: &str) {
    let email = format!("{}@example.test", user_id);
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 1, '2026-10-24T12:00:00Z', '2026-10-24T12:00:00Z')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(&email)
    .bind(email.to_ascii_lowercase())
    .execute(database.pool())
    .await
    .unwrap();
}

struct Fixture {
    _directory: TempDir,
    app: Router,
    project_id: String,
    owner_id: String,
    owner: IssuedSession,
    member: IssuedSession,
    outsider: IssuedSession,
    members: Vec<(String, ProjectRole, bool)>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::query(
            "INSERT INTO organizations \
             (id, name, locale, timezone, created_at, updated_at) \
             VALUES (1, 'Project member tests', 'en', 'UTC', 'now', 'now')",
        )
        .execute(database.pool())
        .await
        .unwrap();

        let owner_id = create_user(&database, "Owner", "owner").await;
        let project_id = ProjectStore::new(database.pool().clone())
            .create(&owner_id, "TEAM", "Member API", None, Utc::now())
            .await
            .unwrap()
            .id;
        let memberships = ProjectMembershipStore::new(database.pool().clone());
        let member_id = create_user(&database, "Member", "member").await;
        memberships
            .set_role(
                &owner_id,
                &project_id,
                &member_id,
                ProjectRole::Member,
                Utc::now(),
            )
            .await
            .unwrap();
        let viewer_id = create_user(&database, "Viewer", "member").await;
        memberships
            .set_role(
                &owner_id,
                &project_id,
                &viewer_id,
                ProjectRole::Viewer,
                Utc::now(),
            )
            .await
            .unwrap();
        let guest_id = create_user(&database, "Guest", "guest").await;
        memberships
            .set_role(
                &owner_id,
                &project_id,
                &guest_id,
                ProjectRole::Member,
                Utc::now(),
            )
            .await
            .unwrap();
        let disabled_id = create_user(&database, "Disabled", "member").await;
        memberships
            .set_role(
                &owner_id,
                &project_id,
                &disabled_id,
                ProjectRole::Member,
                Utc::now(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
            .bind(&disabled_id)
            .execute(database.pool())
            .await
            .unwrap();
        let outsider_id = create_user(&database, "Outsider", "member").await;

        for user_id in [&owner_id, &member_id, &viewer_id, &guest_id, &disabled_id] {
            add_email(&database, user_id).await;
        }
        let sessions = SessionStore::new(database.pool().clone(), policy());
        let owner = sessions.issue(&owner_id, Utc::now()).await.unwrap();
        let member = sessions.issue(&member_id, Utc::now()).await.unwrap();
        let outsider = sessions.issue(&outsider_id, Utc::now()).await.unwrap();
        let security = BrowserSessionSecurity::new(
            "https://tracker.example.test",
            MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
        )
        .unwrap();
        let app = http::router_with_browser_sessions(database.clone(), security, policy());
        Self {
            _directory: directory,
            app,
            project_id,
            owner_id,
            owner,
            member,
            outsider,
            members: vec![
                (member_id, ProjectRole::Member, true),
                (viewer_id, ProjectRole::Viewer, false),
                (guest_id, ProjectRole::Member, true),
                (disabled_id, ProjectRole::Member, false),
            ],
        }
    }

    fn path(&self) -> String {
        format!("/api/v1/projects/{}/members", self.project_id)
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
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn roster_is_session_bound_keyset_paged_and_privacy_filtered() {
    let fixture = Fixture::new().await;
    let mut cursor = None;
    let mut observed = Vec::new();
    let mut page_number = 0;
    loop {
        let query = cursor
            .as_ref()
            .map(|value: &String| format!("&cursor={value}"))
            .unwrap_or_default();
        let page = json(
            fixture
                .get(
                    &format!("{}?page_size=2{query}", fixture.path()),
                    Some(&fixture.owner.token),
                )
                .await,
            StatusCode::OK,
        )
        .await;
        let data = page["data"].as_array().unwrap();
        assert!(!data.is_empty());
        assert!(data.len() <= 2);
        for pair in data.windows(2) {
            assert!(pair[0]["user_id"].as_str() < pair[1]["user_id"].as_str());
        }
        for record in data {
            observed.push(record["user_id"].as_str().unwrap().to_owned());
            assert!(record.get("primary_email").is_some());
        }
        page_number += 1;
        if page["has_more"] == false {
            assert!(page.get("next_cursor").is_none());
            break;
        }
        cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
        assert_eq!(cursor.as_deref(), data.last().unwrap()["user_id"].as_str());
    }
    assert_eq!(page_number, 3);
    observed.sort();
    observed.dedup();
    assert_eq!(observed.len(), 5);
    assert!(observed.contains(&fixture.owner_id));

    for (user_id, role, can_be_assigned) in &fixture.members {
        let record = json(
            fixture
                .get(
                    &format!("{}/{}", fixture.path(), user_id),
                    Some(&fixture.owner.token),
                )
                .await,
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            record["role"],
            match role {
                ProjectRole::Manager => "manager",
                ProjectRole::Member => "member",
                ProjectRole::Viewer => "viewer",
            }
        );
        assert_eq!(record["can_be_assigned"], *can_be_assigned);
    }

    let member_view = json(
        fixture
            .get(&fixture.path(), Some(&fixture.member.token))
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(
        member_view["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| {
                record.get("primary_email").is_none() && record.get("can_be_assigned").is_some()
            })
    );
    let current_member = json(
        fixture
            .get(
                &format!("{}/{}", fixture.path(), fixture.owner_id),
                Some(&fixture.member.token),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(current_member["can_be_assigned"], true);
    assert!(current_member.get("primary_email").is_none());

    assert_eq!(
        fixture
            .get(&fixture.path(), Some(&fixture.outsider.token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture.get(&fixture.path(), None).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn roster_rejects_invalid_page_size_unknown_query_and_single_member_query() {
    let fixture = Fixture::new().await;
    for (path, expected_status) in [
        (
            format!("{}?page_size=0", fixture.path()),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{}?unknown=true", fixture.path()),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = fixture.get(&path, Some(&fixture.owner.token)).await;
        assert_eq!(response.status(), expected_status, "{path}");
    }
    let response = fixture
        .get(
            &format!("{}/{}?page_size=1", fixture.path(), fixture.owner_id),
            Some(&fixture.owner.token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
