use chrono::Utc;
use puxiri::{
    db::Database,
    issues::{
        IssuePriority, IssueReadError, IssueSearchCursor, IssueSearchOptions, IssueStore, NewIssue,
    },
    projects::ProjectStore,
};
use sqlx::{
    ConnectOptions, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tempfile::TempDir;
use uuid::Uuid;

async fn migration_38_database(directory: &TempDir) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(directory.path().join("issue-search-upgrade.sqlite3"))
        .create_if_missing(true)
        .foreign_keys(true)
        .disable_statement_logging();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let mut paths =
        std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.file_name().unwrap() == "0039_issue_text_search.sql" {
            break;
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(path).unwrap()))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

async fn seed_owner(pool: &SqlitePool) -> (String, String) {
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Search tests', 'en', 'UTC', 'now', 'now')")
        .execute(pool)
        .await
        .unwrap();
    let owner = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Search owner', 'active', 'now', 'now')")
        .bind(&owner)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')")
        .bind(&owner)
        .execute(pool)
        .await
        .unwrap();
    let project = ProjectStore::new(pool.clone())
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    (owner, project.id)
}

fn search_options<'a>(
    query: Option<&'a str>,
    priority: Option<IssuePriority>,
    after: Option<&'a IssueSearchCursor>,
    page_size: u16,
) -> IssueSearchOptions<'a> {
    IssueSearchOptions {
        query,
        priority,
        after,
        page_size,
    }
}

#[tokio::test]
async fn migration_backfills_search_documents_and_insert_update_delete_triggers_stay_consistent() {
    let directory = TempDir::new().unwrap();
    let pool = migration_38_database(&directory).await;
    let (owner, project_id) = seed_owner(&pool).await;
    let store = IssueStore::new(pool.clone());
    let legacy = store
        .create(
            &owner,
            &project_id,
            NewIssue {
                title: "Legacy migration phrase",
                description: Some("backfilled description needle"),
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let original_documents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0039_issue_text_search.sql"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let documents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_search_documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(documents, original_documents);
    let title_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"legacy\" AND \"migration\"'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(title_hits, 1);
    let description_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"backfilled\" AND \"needle\"'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(description_hits, 1);
    let backfilled = store
        .search_project_issues(&owner, &project_id, "legacy phrase", None, 10)
        .await
        .unwrap();
    assert_eq!(backfilled.items[0].id, legacy.id);

    sqlx::query("UPDATE issues SET title = 'Replaced title', description = 'fresh search token' WHERE id = ?")
        .bind(&legacy.id)
        .execute(&pool)
        .await
        .unwrap();
    let old_hits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"legacy\"'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let new_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"fresh\" AND \"token\"'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((old_hits, new_hits), (0, 1));

    let status_id: String =
        sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = 'todo'")
            .bind(&project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let issue_type_id: String = sqlx::query_scalar(
        "SELECT id FROM issue_types WHERE project_id = ? AND system_key = 'task'",
    )
    .bind(&project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let disposable = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO issues (id, project_id, issue_number, issue_type_id, status_id, title, description, creator_user_id, rank, version, created_at, updated_at) VALUES (?, ?, 900, ?, ?, 'Inserted trigger token', 'description', ?, 1024, 1, '2026-10-24T00:00:00.000000000Z', '2026-10-24T00:00:00.000000000Z')")
        .bind(&disposable)
        .bind(&project_id)
        .bind(issue_type_id)
        .bind(status_id)
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
    let inserted_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"inserted\" AND \"trigger\"'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(inserted_hits, 1);
    sqlx::query("DELETE FROM issues WHERE id = ?")
        .bind(&disposable)
        .execute(&pool)
        .await
        .unwrap();
    let deleted_hits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM issues_fts WHERE issues_fts MATCH '\"inserted\"'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let remaining_documents: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM issue_search_documents WHERE issue_id = ?")
            .bind(&disposable)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((deleted_hits, remaining_documents), (0, 0));
}

#[tokio::test]
async fn issue_search_uses_literal_terms_exact_keys_and_stable_bounded_pages() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Search tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool())
        .await
        .unwrap();
    let owner = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Search owner', 'active', 'now', 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    for (title, description) in [
        ("Alpha beta first", Some("description only token")),
        ("Alpha beta second", None),
        ("Alpha without partner", None),
        ("Operator OR private", None),
    ] {
        store
            .create(
                &owner,
                &project.id,
                NewIssue {
                    title,
                    description,
                    assignee_user_id: None,
                },
                Utc::now(),
            )
            .await
            .unwrap();
    }
    let exact = store
        .search_project_issues(&owner, &project.id, "find-2", None, 10)
        .await
        .unwrap();
    assert_eq!(
        exact
            .items
            .iter()
            .map(|issue| issue.issue_key.as_str())
            .collect::<Vec<_>>(),
        ["FIND-2"]
    );
    let leading_zero_key = store
        .search_project_issues(&owner, &project.id, "FIND-002", None, 10)
        .await
        .unwrap();
    assert!(leading_zero_key.items.is_empty());
    let terms = store
        .search_project_issues(&owner, &project.id, "ALPHA beta", None, 1)
        .await
        .unwrap();
    assert_eq!(terms.items.len(), 1);
    assert_eq!(terms.items[0].issue_number, 1);
    let cursor = terms.next_cursor.unwrap();
    let decoded = IssueSearchCursor::decode(&cursor.encode()).unwrap();
    let second = store
        .search_project_issues(&owner, &project.id, "ALPHA beta", Some(&decoded), 1)
        .await
        .unwrap();
    assert_eq!(second.items[0].issue_number, 2);
    assert!(second.next_cursor.is_none());
    let quoted_operator = store
        .search_project_issues(&owner, &project.id, "OR private", None, 10)
        .await
        .unwrap();
    assert_eq!(quoted_operator.items.len(), 1);
    assert_eq!(quoted_operator.items[0].issue_number, 4);
    let description = store
        .search_project_issues(&owner, &project.id, "description token", None, 10)
        .await
        .unwrap();
    assert_eq!(description.items.len(), 1);
    assert_eq!(description.items[0].issue_number, 1);
    for query in ["", " \t", "one\ntwo", &"x".repeat(513)] {
        assert!(matches!(
            store
                .search_project_issues(&owner, &project.id, query, None, 10)
                .await,
            Err(IssueReadError::InvalidSearchQuery)
        ));
    }
    let key_shaped_but_invalid = store
        .search_project_issues(&owner, &project.id, "FIND-00000000000000000000", None, 10)
        .await
        .unwrap();
    assert!(key_shaped_but_invalid.items.is_empty());
    assert!(matches!(
        store
            .search_project_issues(&owner, &project.id, "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen", None, 10)
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));
    assert!(matches!(
        IssueSearchCursor::decode("bm90LWEtY3Vyc29y"),
        Err(IssueReadError::InvalidSearchCursor)
    ));
    assert!(matches!(
        IssueSearchCursor::decode(&"A".repeat(100_000)),
        Err(IssueReadError::InvalidSearchCursor)
    ));
    assert!(matches!(
        store
            .search_project_issues(&owner, &project.id, "alpha", None, 0)
            .await,
        Err(IssueReadError::InvalidPageSize)
    ));
}

#[tokio::test]
async fn issue_search_filters_priority_with_or_without_text_and_uses_a_bounded_index() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Search tests', 'en', 'UTC', 'now', 'now')")
        .execute(database.pool())
        .await
        .unwrap();
    let owner = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Search owner', 'active', 'now', 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')")
        .bind(&owner)
        .execute(database.pool())
        .await
        .unwrap();
    let project = ProjectStore::new(database.pool().clone())
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let mut issue_ids = Vec::new();
    for title in [
        "Shared urgent work",
        "Shared high work",
        "Other urgent work",
        "Shared without priority",
    ] {
        issue_ids.push(
            store
                .create(
                    &owner,
                    &project.id,
                    NewIssue {
                        title,
                        description: None,
                        assignee_user_id: None,
                    },
                    Utc::now(),
                )
                .await
                .unwrap()
                .id,
        );
    }
    for (issue_id, priority) in [
        (&issue_ids[0], "urgent"),
        (&issue_ids[1], "high"),
        (&issue_ids[2], "urgent"),
    ] {
        sqlx::query("UPDATE issues SET priority = ? WHERE id = ?")
            .bind(priority)
            .bind(issue_id)
            .execute(database.pool())
            .await
            .unwrap();
    }

    let urgent = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options(None, Some(IssuePriority::Urgent), None, 1),
        )
        .await
        .unwrap();
    assert_eq!(urgent.items[0].id, issue_ids[0]);
    let next = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options(
                None,
                Some(IssuePriority::Urgent),
                urgent.next_cursor.as_ref(),
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0].id, issue_ids[2]);
    assert!(next.next_cursor.is_none());

    let text_and_priority = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options(Some("Shared"), Some(IssuePriority::Urgent), None, 10),
        )
        .await
        .unwrap();
    assert_eq!(
        text_and_priority
            .items
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        [issue_ids[0].as_str()]
    );
    let key_and_priority = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options(Some("find-2"), Some(IssuePriority::High), None, 10),
        )
        .await
        .unwrap();
    assert_eq!(key_and_priority.items[0].id, issue_ids[1]);
    let mismatched_key_priority = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options(Some("FIND-2"), Some(IssuePriority::Urgent), None, 10),
        )
        .await
        .unwrap();
    assert!(mismatched_key_priority.items.is_empty());

    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options(None, None, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options(Some("  "), Some(IssuePriority::Urgent), None, 10),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM issues \
         WHERE project_id = ? AND priority = ? AND trashed_at IS NULL \
           AND (issue_number, id) > (?, ?) \
         ORDER BY issue_number ASC, id ASC LIMIT ?",
    )
    .bind(&project.id)
    .bind("urgent")
    .bind(0_i64)
    .bind("")
    .bind(51_i64)
    .fetch_all(database.pool())
    .await
    .unwrap();
    let plan_details = plan
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        plan_details.contains("issues_project_priority_search_idx"),
        "unexpected query plan: {plan_details}"
    );
}
