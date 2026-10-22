use chrono::Utc;
use puxiri::{
    issues::{IssueStore, NewIssue},
    projects::ProjectStore,
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn migration_preserves_existing_issues_and_history_without_fabricating_old_outbox_events() {
    let directory = TempDir::new().unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(directory.path().join("upgrade.sqlite3"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let migration_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut paths = std::fs::read_dir(migration_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.file_name().unwrap() == "0031_issue_creation_receipts_and_outbox.sql" {
            break;
        }
        // Audited repository migration SQL, executed only in this isolated DB.
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(path).unwrap()))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Upgrade fixture', 'en', 'UTC', 'now', 'now')").execute(&pool).await.unwrap();
    let owner = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Upgrade owner', 'active', 'now', 'now')").bind(&owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')").bind(&owner).execute(&pool).await.unwrap();
    let now = Utc::now();
    let time = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let project = ProjectStore::new(pool.clone())
        .create(&owner, "OLD", "Upgrade project", None, now)
        .await
        .unwrap();
    let issue_type: String = sqlx::query_scalar(
        "SELECT id FROM issue_types WHERE project_id = ? AND system_key = 'task'",
    )
    .bind(&project.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String =
        sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = 'todo'")
            .bind(&project.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let issue = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO issues (id, project_id, issue_number, issue_type_id, status_id, title, description, creator_user_id, rank, created_at, updated_at) VALUES (?, ?, 1, ?, ?, 'Existing title', 'Existing description', ?, 1024, ?, ?)")
        .bind(&issue).bind(&project.id).bind(issue_type).bind(status).bind(&owner).bind(&time).bind(&time).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, 'issue.created', ?)")
        .bind(Uuid::now_v7().to_string()).bind(&project.id).bind(&issue).bind(&owner).bind(&time).execute(&pool).await.unwrap();
    sqlx::query("UPDATE projects SET next_issue_number = 2 WHERE id = ?")
        .bind(&project.id)
        .execute(&pool)
        .await
        .unwrap();
    let store = IssueStore::new(pool.clone());
    let before = store
        .get_project_issue(&owner, &project.id, 1)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0031_issue_creation_receipts_and_outbox.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        before,
        store
            .get_project_issue(&owner, &project.id, 1)
            .await
            .unwrap()
    );
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox, 0);
    let created = store
        .create(
            &owner,
            &project.id,
            NewIssue {
                title: "Created after upgrade",
                description: None,
                assignee_user_id: None,
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(created.issue_number, 2);
    let ids: Vec<String> = sqlx::query_scalar("SELECT issue_id FROM outbox_events")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ids, [created.id]);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}
