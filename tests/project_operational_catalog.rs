use chrono::Utc;
use puxiri::{
    db::Database,
    issues::{IssueReadError, IssueStore, NewIssue},
    projects::{ProjectListOptions, ProjectReadError, ProjectStore},
};
use tempfile::TempDir;
use uuid::Uuid;

async fn organization(database: &Database) {
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Catalog tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool()).await.unwrap();
}

async fn user(database: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Catalog user', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')")
        .bind(&id).bind(role).execute(database.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn operational_catalog_counts_retained_rows_without_granting_issue_access() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let creator = user(&database, "owner").await;
    let admin = user(&database, "admin").await;
    let owner = user(&database, "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    let project = store
        .create(
            &creator,
            "OPS",
            "Operational name",
            Some("Description not in catalog"),
            Utc::now(),
        )
        .await
        .unwrap();
    let issues = IssueStore::new(database.pool().clone());
    let issue = issues
        .create(
            &creator,
            &project.id,
            NewIssue {
                title: "Private issue title",
                description: Some("Private issue content"),
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = '2026-10-15T00:00:00Z' WHERE id = ?")
        .bind(&issue.id)
        .execute(database.pool())
        .await
        .unwrap();
    for actor in [&admin, &owner] {
        let page = store
            .list_operational_catalog(actor, ProjectListOptions::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, project.id);
        assert_eq!(page.items[0].name, "Operational name");
        assert_eq!(page.items[0].membership_count, 1);
        assert_eq!(page.items[0].issue_count, 1);
        assert!(page.items[0].archived_at.is_none());
        assert!(matches!(
            issues
                .get_project_issue(actor, &project.id, issue.issue_number)
                .await,
            Err(IssueReadError::Unavailable)
        ));
        assert!(
            store
                .list_accessible(actor, ProjectListOptions::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }
    let disabled_member = user(&database, "member").await;
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'member', 'now')")
        .bind(&project.id).bind(&disabled_member).execute(database.pool()).await.unwrap();
    sqlx::query("UPDATE users SET state = 'disabled' WHERE id = ?")
        .bind(&disabled_member)
        .execute(database.pool())
        .await
        .unwrap();
    issues
        .create(
            &creator,
            &project.id,
            NewIssue {
                title: "Another private issue",
                description: None,
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let current = store
        .list_operational_catalog(&admin, ProjectListOptions::default())
        .await
        .unwrap();
    assert_eq!(current.items[0].membership_count, 2);
    assert_eq!(current.items[0].issue_count, 2);
    // Only creation events exist: operational reads never mutate or self-grant.
    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn operational_authority_is_current_and_never_inferred_from_project_or_installation_roles() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    let project = store
        .create(&owner, "AUTH", "Catalog authority", None, Utc::now())
        .await
        .unwrap();
    for role in ["member", "guest"] {
        let actor = user(&database, role).await;
        let project_role = if role == "guest" { "viewer" } else { "manager" };
        sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, ?, 'now')")
            .bind(&project.id).bind(&actor).bind(project_role).execute(database.pool()).await.unwrap();
        if role == "member" {
            sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
                .bind(&actor)
                .execute(database.pool())
                .await
                .unwrap();
        }
        assert!(matches!(
            store
                .list_operational_catalog(&actor, ProjectListOptions::default())
                .await,
            Err(ProjectReadError::Unavailable)
        ));
    }
    let admin = user(&database, "admin").await;
    store
        .list_operational_catalog(&admin, ProjectListOptions::default())
        .await
        .unwrap();
    sqlx::query("UPDATE organization_members SET role = 'member' WHERE user_id = ?")
        .bind(&admin)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .list_operational_catalog(&admin, ProjectListOptions::default())
            .await,
        Err(ProjectReadError::Unavailable)
    ));
    for state in ["banned", "disabled"] {
        sqlx::query("UPDATE users SET state = ? WHERE id = ?")
            .bind(state)
            .bind(&owner)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(matches!(
            store
                .list_operational_catalog(&owner, ProjectListOptions::default())
                .await,
            Err(ProjectReadError::Unavailable)
        ));
    }
    assert!(matches!(
        store
            .list_operational_catalog(&Uuid::now_v7().to_string(), ProjectListOptions::default())
            .await,
        Err(ProjectReadError::Unavailable)
    ));
}

#[tokio::test]
async fn operational_catalog_paginates_filters_archives_and_rejects_invalid_or_corrupt_input() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    assert!(
        store
            .list_operational_catalog(&owner, ProjectListOptions::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for key in ["CC", "AA", "BB"] {
        let project = store
            .create(&owner, key, "Catalog project", None, Utc::now())
            .await
            .unwrap();
        if key == "BB" {
            store
                .set_archived(&owner, &project.id, true, Utc::now())
                .await
                .unwrap();
        }
    }
    let first = store
        .list_operational_catalog(
            &owner,
            ProjectListOptions {
                page_size: 1,
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.items[0].project_key, "AA");
    assert_eq!(first.items[0].issue_count, 0);
    assert_eq!(first.next_after_project_key.as_deref(), Some("AA"));
    let second = store
        .list_operational_catalog(
            &owner,
            ProjectListOptions {
                page_size: 1,
                after_project_key: first.next_after_project_key.as_deref(),
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(second.items[0].project_key, "CC");
    assert!(second.next_after_project_key.is_none());
    let all = store
        .list_operational_catalog(
            &owner,
            ProjectListOptions {
                include_archived: true,
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(all.items.len(), 3);
    assert_eq!(all.items[1].project_key, "BB");
    assert!(all.items[1].archived_at.is_some());
    for page_size in [0, 101] {
        assert!(matches!(
            store
                .list_operational_catalog(
                    &owner,
                    ProjectListOptions {
                        page_size,
                        ..ProjectListOptions::default()
                    }
                )
                .await,
            Err(ProjectReadError::InvalidPageSize)
        ));
    }
    assert!(matches!(
        store
            .list_operational_catalog(
                &owner,
                ProjectListOptions {
                    after_project_key: Some("bad cursor"),
                    ..ProjectListOptions::default()
                }
            )
            .await,
        Err(ProjectReadError::InvalidCursor)
    ));
    sqlx::query("UPDATE projects SET archived_at = 'malformed' WHERE project_key = 'BB'")
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .list_operational_catalog(
                &owner,
                ProjectListOptions {
                    include_archived: true,
                    ..ProjectListOptions::default()
                }
            )
            .await,
        Err(ProjectReadError::CorruptData)
    ));
}
