use chrono::Utc;
use puxiri::{
    access::ProjectRole,
    db::Database,
    projects::{ProjectListOptions, ProjectReadError, ProjectStore},
};
use tempfile::TempDir;
use uuid::Uuid;

async fn organization(database: &Database) {
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Listing tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool()).await.unwrap();
}

async fn user(database: &Database, org_role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Listing user', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')")
        .bind(&id).bind(org_role).execute(database.pool()).await.unwrap();
    id
}

async fn member(database: &Database, project: &str, user: &str, role: &str) {
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, ?, 'now')")
        .bind(project).bind(user).bind(role).execute(database.pool()).await.unwrap();
}

#[tokio::test]
async fn listing_requires_explicit_membership_even_for_org_and_installation_admins() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let org_admin = user(&database, "admin").await;
    let other_owner = user(&database, "owner").await;
    let operations = user(&database, "member").await;
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, 'now')")
        .bind(&operations)
        .execute(database.pool())
        .await
        .unwrap();
    let store = ProjectStore::new(database.pool().clone());
    let project = store
        .create(
            &owner,
            "READ",
            "Readable project",
            Some("Private description"),
            Utc::now(),
        )
        .await
        .unwrap();
    let other = store
        .create(&owner, "OTHER", "Other project", None, Utc::now())
        .await
        .unwrap();
    for actor in [&org_admin, &operations, &other_owner] {
        assert!(
            store
                .list_accessible(actor, ProjectListOptions::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        member(&database, &project.id, actor, "viewer").await;
        let page = store
            .list_accessible(actor, ProjectListOptions::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, project.id);
        assert_eq!(page.items[0].name, "Readable project");
        assert_eq!(page.items[0].role, ProjectRole::Viewer);
        assert_eq!(page.items[0].archived_at, None);
        assert_eq!(page.next_after_project_key, None);
    }
    for (org_role, project_role, expected) in [
        ("member", "manager", ProjectRole::Manager),
        ("member", "member", ProjectRole::Member),
        ("guest", "viewer", ProjectRole::Viewer),
    ] {
        let actor = user(&database, org_role).await;
        member(&database, &other.id, &actor, project_role).await;
        let page = store
            .list_accessible(&actor, ProjectListOptions::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, other.id);
        assert_eq!(page.items[0].role, expected);
    }
    let ordinary = user(&database, "member").await;
    assert!(
        store
            .list_accessible(&ordinary, ProjectListOptions::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(audit_count, 2);
}

#[tokio::test]
async fn archives_are_hidden_by_default_and_all_filters_keep_current_access() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let viewer = user(&database, "guest").await;
    let store = ProjectStore::new(database.pool().clone());
    let project = store
        .create(&owner, "HIDE", "Archived", None, Utc::now())
        .await
        .unwrap();
    member(&database, &project.id, &viewer, "viewer").await;
    store
        .set_archived(&owner, &project.id, true, Utc::now())
        .await
        .unwrap();
    assert!(
        store
            .list_accessible(&viewer, ProjectListOptions::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let options = ProjectListOptions {
        include_archived: true,
        ..ProjectListOptions::default()
    };
    let archived = store.list_accessible(&viewer, options).await.unwrap();
    assert_eq!(archived.items.len(), 1);
    assert!(archived.items[0].archived_at.is_some());
    store
        .set_archived(&owner, &project.id, false, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        store
            .list_accessible(&viewer, ProjectListOptions::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(&project.id)
        .bind(&viewer)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .list_accessible(&viewer, options)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for state in ["banned", "disabled"] {
        sqlx::query("UPDATE users SET state = ? WHERE id = ?")
            .bind(state)
            .bind(&owner)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(matches!(
            store.list_accessible(&owner, options).await,
            Err(ProjectReadError::Unavailable)
        ));
    }
    assert!(matches!(
        store
            .list_accessible(&Uuid::now_v7().to_string(), options)
            .await,
        Err(ProjectReadError::Unavailable)
    ));
    sqlx::query("DELETE FROM organization_members WHERE user_id = ?")
        .bind(&viewer)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.list_accessible(&viewer, options).await,
        Err(ProjectReadError::Unavailable)
    ));
}

#[tokio::test]
async fn pages_use_stable_project_keys_and_apply_access_and_archive_before_lookahead() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let actor = user(&database, "member").await;
    let store = ProjectStore::new(database.pool().clone());
    for key in ["CC", "AA", "DD", "BB", "EE"] {
        let project = store
            .create(&owner, key, "Same mutable name", None, Utc::now())
            .await
            .unwrap();
        if key != "BB" {
            member(&database, &project.id, &actor, "member").await;
        }
        if key == "CC" {
            store
                .set_archived(&owner, &project.id, true, Utc::now())
                .await
                .unwrap();
        }
    }
    let first = store
        .list_accessible(
            &actor,
            ProjectListOptions {
                page_size: 2,
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|project| project.project_key.as_str())
            .collect::<Vec<_>>(),
        ["AA", "DD"]
    );
    assert_eq!(first.next_after_project_key.as_deref(), Some("DD"));
    // Renaming cannot reorder existing keys or widen scope.
    sqlx::query("UPDATE projects SET name = 'Renamed' WHERE project_key = 'DD'")
        .execute(database.pool())
        .await
        .unwrap();
    let last = store
        .list_accessible(
            &actor,
            ProjectListOptions {
                after_project_key: first.next_after_project_key.as_deref(),
                page_size: 2,
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].project_key, "EE");
    assert_eq!(last.next_after_project_key, None);
    let after_hidden = store
        .list_accessible(
            &actor,
            ProjectListOptions {
                after_project_key: Some("BB"),
                include_archived: true,
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        after_hidden
            .items
            .iter()
            .map(|project| project.project_key.as_str())
            .collect::<Vec<_>>(),
        ["CC", "DD", "EE"]
    );
    let beyond = store
        .list_accessible(
            &actor,
            ProjectListOptions {
                after_project_key: Some("ZZ"),
                ..ProjectListOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(beyond.items.is_empty());
    assert_eq!(beyond.next_after_project_key, None);
}

#[tokio::test]
async fn invalid_bounds_and_malformed_persisted_archive_state_fail_explicitly() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    organization(&database).await;
    let owner = user(&database, "owner").await;
    let store = ProjectStore::new(database.pool().clone());
    store
        .create(&owner, "BAD", "Malformed archive", None, Utc::now())
        .await
        .unwrap();
    for page_size in [0, 101, u16::MAX] {
        assert!(matches!(
            store
                .list_accessible(
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
    for key in ["", "A", "abcdefg", "aa", "AA' OR 1=1--", " É"] {
        assert!(matches!(
            store
                .list_accessible(
                    &owner,
                    ProjectListOptions {
                        after_project_key: Some(key),
                        ..ProjectListOptions::default()
                    }
                )
                .await,
            Err(ProjectReadError::InvalidCursor)
        ));
    }
    sqlx::query("UPDATE projects SET archived_at = 'malformed' WHERE project_key = 'BAD'")
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .list_accessible(
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
