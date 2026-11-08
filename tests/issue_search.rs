use chrono::{NaiveDate, Utc};
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
        due_date: None,
        issue_type_id: None,
        status_id: None,
        assignee_user_id: None,
        after,
        page_size,
    }
}

fn search_options_with_due_date<'a>(
    query: Option<&'a str>,
    priority: Option<IssuePriority>,
    due_date: NaiveDate,
    after: Option<&'a IssueSearchCursor>,
    page_size: u16,
) -> IssueSearchOptions<'a> {
    IssueSearchOptions {
        query,
        priority,
        due_date: Some(due_date),
        issue_type_id: None,
        status_id: None,
        assignee_user_id: None,
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

#[tokio::test]
async fn issue_search_filters_exact_due_date_with_text_priority_pagination_and_index() {
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
        "Shared urgent due",
        "Shared high later",
        "Other urgent due",
        "Shared without due date",
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
    let due_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
    for (issue_id, date, priority) in [
        (&issue_ids[0], "2026-10-30", "urgent"),
        (&issue_ids[1], "2026-10-31", "high"),
        (&issue_ids[2], "2026-10-30", "urgent"),
    ] {
        sqlx::query("UPDATE issues SET due_date = ?, priority = ? WHERE id = ?")
            .bind(date)
            .bind(priority)
            .bind(issue_id)
            .execute(database.pool())
            .await
            .unwrap();
    }

    let first = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_due_date(None, None, due_date, None, 1),
        )
        .await
        .unwrap();
    assert_eq!(first.items[0].id, issue_ids[0]);
    assert_eq!(first.items[0].due_date, Some(due_date));
    let second = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_due_date(None, None, due_date, first.next_cursor.as_ref(), 10),
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].id, issue_ids[2]);
    assert!(second.next_cursor.is_none());

    let combined = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_due_date(
                Some("Shared"),
                Some(IssuePriority::Urgent),
                due_date,
                None,
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(combined.items.len(), 1);
    assert_eq!(combined.items[0].id, issue_ids[0]);
    let key_mismatch = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_due_date(
                Some("FIND-2"),
                Some(IssuePriority::High),
                due_date,
                None,
                10,
            ),
        )
        .await
        .unwrap();
    assert!(key_mismatch.items.is_empty());
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_due_date(Some("  "), None, due_date, None, 10,),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM issues \
         WHERE project_id = ? AND due_date = ? AND trashed_at IS NULL \
           AND (issue_number, id) > (?, ?) \
         ORDER BY issue_number ASC, id ASC LIMIT ?",
    )
    .bind(&project.id)
    .bind("2026-10-30")
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
        plan_details.contains("issues_project_due_date_search_idx"),
        "unexpected query plan: {plan_details}"
    );
}

fn search_options_with_issue_type<'a>(
    query: Option<&'a str>,
    priority: Option<IssuePriority>,
    due_date: Option<NaiveDate>,
    issue_type_id: &'a str,
    after: Option<&'a IssueSearchCursor>,
    page_size: u16,
) -> IssueSearchOptions<'a> {
    IssueSearchOptions {
        query,
        priority,
        due_date,
        issue_type_id: Some(issue_type_id),
        status_id: None,
        assignee_user_id: None,
        after,
        page_size,
    }
}

#[tokio::test]
async fn issue_search_filters_exact_issue_type_including_archived_and_rejects_foreign_types() {
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
    let projects = ProjectStore::new(database.pool().clone());
    let project = projects
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    let other = projects
        .create(&owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let mut issue_ids = Vec::new();
    for title in [
        "Shared bug one",
        "Shared bug two",
        "Shared task",
        "Trashed bug",
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
    let bug_type = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO issue_types (id, project_id, name, created_at, updated_at) VALUES (?, ?, 'Bug', 'now', 'now')",
    )
    .bind(&bug_type)
    .bind(&project.id)
    .execute(database.pool())
    .await
    .unwrap();
    for issue_id in [&issue_ids[0], &issue_ids[1], &issue_ids[3]] {
        sqlx::query("UPDATE issues SET issue_type_id = ? WHERE id = ?")
            .bind(&bug_type)
            .bind(issue_id)
            .execute(database.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&issue_ids[3])
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issue_types SET archived_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&bug_type)
        .execute(database.pool())
        .await
        .unwrap();
    let foreign_type: String = sqlx::query_scalar(
        "SELECT id FROM issue_types WHERE project_id = ? AND system_key = 'task'",
    )
    .bind(&other.id)
    .fetch_one(database.pool())
    .await
    .unwrap();

    let first = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_issue_type(None, None, None, &bug_type, None, 1),
        )
        .await
        .unwrap();
    assert_eq!(first.items[0].id, issue_ids[0]);
    assert_eq!(first.items[0].issue_type_id, bug_type);
    let second = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_issue_type(
                None,
                None,
                None,
                &bug_type,
                first.next_cursor.as_ref(),
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].id, issue_ids[1]);
    assert!(second.next_cursor.is_none());

    let combined = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_issue_type(
                Some("Shared"),
                Some(IssuePriority::None),
                None,
                &bug_type,
                None,
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(combined.items.len(), 2);
    let task_type: String = sqlx::query_scalar(
        "SELECT id FROM issue_types WHERE project_id = ? AND system_key = 'task'",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let task_only = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_issue_type(None, None, None, &task_type, None, 10),
        )
        .await
        .unwrap();
    assert_eq!(task_only.items.len(), 1);
    assert_eq!(task_only.items[0].id, issue_ids[2]);
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_issue_type(Some("  "), None, None, &bug_type, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_issue_type(None, None, None, &foreign_type, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueType)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_issue_type(None, None, None, "not-a-type", None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueType)
    ));

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM issues \
         WHERE project_id = ? AND issue_type_id = ? AND trashed_at IS NULL \
           AND (issue_number, id) > (?, ?) \
         ORDER BY issue_number ASC, id ASC LIMIT ?",
    )
    .bind(&project.id)
    .bind(&bug_type)
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
        plan_details.contains("issues_project_type_search_idx"),
        "unexpected query plan: {plan_details}"
    );
}

fn search_options_with_status<'a>(
    query: Option<&'a str>,
    priority: Option<IssuePriority>,
    due_date: Option<NaiveDate>,
    status_id: &'a str,
    after: Option<&'a IssueSearchCursor>,
    page_size: u16,
) -> IssueSearchOptions<'a> {
    IssueSearchOptions {
        query,
        priority,
        due_date,
        issue_type_id: None,
        status_id: Some(status_id),
        assignee_user_id: None,
        after,
        page_size,
    }
}

#[tokio::test]
async fn issue_search_filters_exact_status_including_archived_and_rejects_foreign_statuses() {
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
    let projects = ProjectStore::new(database.pool().clone());
    let project = projects
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    let other = projects
        .create(&owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let mut issue_ids = Vec::new();
    for title in [
        "Shared active one",
        "Shared active two",
        "Shared todo",
        "Trashed active",
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
    let active_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND system_key = 'in_progress'",
    )
    .bind(&project.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    for issue_id in [&issue_ids[0], &issue_ids[1], &issue_ids[3]] {
        sqlx::query("UPDATE issues SET status_id = ? WHERE id = ?")
            .bind(&active_status)
            .bind(issue_id)
            .execute(database.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&issue_ids[3])
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET archived_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&active_status)
        .execute(database.pool())
        .await
        .unwrap();
    let foreign_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND system_key = 'in_progress'",
    )
    .bind(&other.id)
    .fetch_one(database.pool())
    .await
    .unwrap();

    let first = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_status(None, None, None, &active_status, None, 1),
        )
        .await
        .unwrap();
    assert_eq!(first.items[0].id, issue_ids[0]);
    assert_eq!(first.items[0].status_id, active_status);
    let second = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_status(
                None,
                None,
                None,
                &active_status,
                first.next_cursor.as_ref(),
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].id, issue_ids[1]);
    assert!(second.next_cursor.is_none());

    let combined = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_status(
                Some("Shared"),
                Some(IssuePriority::None),
                None,
                &active_status,
                None,
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(combined.items.len(), 2);
    let todo_status: String =
        sqlx::query_scalar("SELECT id FROM statuses WHERE project_id = ? AND system_key = 'todo'")
            .bind(&project.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let todo_only = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_status(None, None, None, &todo_status, None, 10),
        )
        .await
        .unwrap();
    assert_eq!(todo_only.items.len(), 1);
    assert_eq!(todo_only.items[0].id, issue_ids[2]);
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_status(Some("  "), None, None, &active_status, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_status(None, None, None, &foreign_status, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueStatus)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_status(None, None, None, "not-a-status", None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueStatus)
    ));

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM issues \
         WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL \
           AND (issue_number, id) > (?, ?) \
         ORDER BY issue_number ASC, id ASC LIMIT ?",
    )
    .bind(&project.id)
    .bind(&active_status)
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
        plan_details.contains("issues_project_status_search_idx"),
        "unexpected query plan: {plan_details}"
    );
}

fn search_options_with_assignee<'a>(
    query: Option<&'a str>,
    priority: Option<IssuePriority>,
    assignee_user_id: &'a str,
    after: Option<&'a IssueSearchCursor>,
    page_size: u16,
) -> IssueSearchOptions<'a> {
    IssueSearchOptions {
        query,
        priority,
        due_date: None,
        issue_type_id: None,
        status_id: None,
        assignee_user_id: Some(assignee_user_id),
        after,
        page_size,
    }
}

#[tokio::test]
async fn issue_search_filters_exact_assignee_including_ineligible_members_and_rejects_foreign_users()
 {
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
    let projects = ProjectStore::new(database.pool().clone());
    let project = projects
        .create(&owner, "FIND", "Search", None, Utc::now())
        .await
        .unwrap();
    let other = projects
        .create(&owner, "OTHR", "Other", None, Utc::now())
        .await
        .unwrap();
    let assignee = Uuid::now_v7().to_string();
    let untouched_viewer = Uuid::now_v7().to_string();
    let foreign = Uuid::now_v7().to_string();
    for (user_id, name) in [
        (&assignee, "Former member"),
        (&untouched_viewer, "Viewer"),
        (&foreign, "Foreign"),
    ] {
        sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, ?, 'active', 'now', 'now')")
            .bind(user_id)
            .bind(name)
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'member', 'now')")
            .bind(user_id)
            .execute(database.pool())
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'member', 'now')")
        .bind(&project.id)
        .bind(&assignee)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')")
        .bind(&project.id)
        .bind(&untouched_viewer)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'member', 'now')")
        .bind(&other.id)
        .bind(&foreign)
        .execute(database.pool())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let mut issue_ids = Vec::new();
    for title in [
        "Shared assigned one",
        "Shared assigned two",
        "Shared owner",
        "Trashed assigned",
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
    for issue_id in [&issue_ids[0], &issue_ids[1], &issue_ids[3]] {
        sqlx::query("UPDATE issues SET assignee_user_id = ? WHERE id = ?")
            .bind(&assignee)
            .bind(issue_id)
            .execute(database.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE issues SET assignee_user_id = ? WHERE id = ?")
        .bind(&owner)
        .bind(&issue_ids[2])
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind("2026-10-24T00:00:00.000000000Z")
        .bind(&issue_ids[3])
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&project.id)
        .bind(&assignee)
        .execute(database.pool())
        .await
        .unwrap();

    let first = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_assignee(None, None, &assignee, None, 1),
        )
        .await
        .unwrap();
    assert_eq!(first.items[0].id, issue_ids[0]);
    assert_eq!(
        first.items[0].assignee_user_id.as_deref(),
        Some(assignee.as_str())
    );
    let second = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_assignee(None, None, &assignee, first.next_cursor.as_ref(), 10),
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].id, issue_ids[1]);
    assert!(second.next_cursor.is_none());

    let combined = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_assignee(
                Some("Shared"),
                Some(IssuePriority::None),
                &assignee,
                None,
                10,
            ),
        )
        .await
        .unwrap();
    assert_eq!(combined.items.len(), 2);
    let owner_only = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_assignee(None, None, &owner, None, 10),
        )
        .await
        .unwrap();
    assert_eq!(owner_only.items.len(), 1);
    assert_eq!(owner_only.items[0].id, issue_ids[2]);
    let untouched = store
        .search_project_issues_with_options(
            &owner,
            &project.id,
            search_options_with_assignee(None, None, &untouched_viewer, None, 10),
        )
        .await
        .unwrap();
    assert!(untouched.items.is_empty());
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_assignee(Some("  "), None, &assignee, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidSearchQuery)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_assignee(None, None, &foreign, None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueAssignee)
    ));
    assert!(matches!(
        store
            .search_project_issues_with_options(
                &owner,
                &project.id,
                search_options_with_assignee(None, None, "not-an-assignee", None, 10),
            )
            .await,
        Err(IssueReadError::InvalidIssueAssignee)
    ));

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM issues \
         WHERE project_id = ? AND assignee_user_id = ? AND trashed_at IS NULL \
           AND (issue_number, id) > (?, ?) \
         ORDER BY issue_number ASC, id ASC LIMIT ?",
    )
    .bind(&project.id)
    .bind(&assignee)
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
        plan_details.contains("issues_project_assignee_search_idx"),
        "unexpected query plan: {plan_details}"
    );
}
