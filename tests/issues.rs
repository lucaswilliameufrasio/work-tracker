use chrono::{DateTime, Utc};
use puxiri::{
    db::Database,
    issues::{IssueCreationError, IssuePriority, IssueStore, NewIssue},
    projects::ProjectStore,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

fn at(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&Utc)
}

async fn create_organization(database: &Database) {
    sqlx::query(
        "INSERT INTO organizations \
         (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, 'Issue test', 'en', 'UTC', \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .execute(database.pool())
    .await
    .unwrap();
}

async fn create_user(database: &Database, state: &str, organization_role: &str) -> String {
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, 'Issue test user', ?, \
                 '2026-10-12T00:00:00.000000000Z', \
                 '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(state)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members \
         (organization_id, user_id, role, created_at) \
         VALUES (1, ?, ?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(&user_id)
    .bind(organization_role)
    .execute(database.pool())
    .await
    .unwrap();
    user_id
}

async fn create_project(database: &Database, owner_id: &str, key: &str) -> String {
    ProjectStore::new(database.pool().clone())
        .create(
            owner_id,
            key,
            "Issue test project",
            None,
            at("2026-10-12T12:00:00Z"),
        )
        .await
        .unwrap()
        .id
}

async fn add_project_member(database: &Database, project_id: &str, user_id: &str, role: &str) {
    sqlx::query(
        "INSERT INTO project_members \
         (project_id, organization_id, user_id, role, created_at) \
         VALUES (?, 1, ?, ?, '2026-10-12T00:00:00.000000000Z')",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(role)
    .execute(database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn creation_uses_stable_default_keys_and_atomically_records_content_free_history() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let guest_assignee = create_user(&database, "active", "guest").await;
    let project_id = create_project(&database, &owner, "DEV").await;
    add_project_member(&database, &project_id, &guest_assignee, "member").await;

    sqlx::query("UPDATE issue_types SET name = 'Work item' WHERE project_id = ?")
        .bind(&project_id)
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET name = 'Ready' WHERE project_id = ? AND system_key = 'todo'")
        .bind(&project_id)
        .execute(database.pool())
        .await
        .unwrap();

    let created_at = at("2026-10-12T13:14:15.123456789Z");
    let issue = IssueStore::new(database.pool().clone())
        .create(
            &owner,
            &project_id,
            NewIssue {
                title: "  Fix release  ",
                description: Some("# Notes\nPreserve the submitted Markdown."),
                assignee_user_id: Some(&guest_assignee),
            },
            created_at,
        )
        .await
        .unwrap();

    assert_eq!(issue.issue_key, "DEV-1");
    assert_eq!(issue.issue_number, 1);
    assert_eq!(issue.title, "Fix release");
    assert_eq!(
        issue.description.as_deref(),
        Some("# Notes\nPreserve the submitted Markdown.")
    );
    assert_eq!(
        issue.assignee_user_id.as_deref(),
        Some(guest_assignee.as_str())
    );
    assert_eq!(issue.creator_user_id, owner);
    assert_eq!(issue.priority, IssuePriority::None);
    assert_eq!(issue.rank, 1_024);
    assert_eq!(issue.version, 1);
    assert_eq!(issue.created_at, created_at);

    let row = sqlx::query(
        "SELECT issue_number, issue_type_id, status_id, title, description, \
                assignee_user_id, creator_user_id, priority, due_date, rank, \
                version, completed_at, trashed_at, created_at, updated_at \
         FROM issues WHERE id = ?",
    )
    .bind(&issue.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("issue_number").unwrap(), 1);
    assert_eq!(
        row.try_get::<String, _>("issue_type_id").unwrap(),
        issue.issue_type_id
    );
    assert_eq!(
        row.try_get::<String, _>("status_id").unwrap(),
        issue.status_id
    );
    assert_eq!(row.try_get::<String, _>("title").unwrap(), "Fix release");
    assert_eq!(
        row.try_get::<Option<String>, _>("description")
            .unwrap()
            .as_deref(),
        Some("# Notes\nPreserve the submitted Markdown.")
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("assignee_user_id")
            .unwrap()
            .as_deref(),
        Some(guest_assignee.as_str())
    );
    assert_eq!(row.try_get::<String, _>("creator_user_id").unwrap(), owner);
    assert_eq!(row.try_get::<String, _>("priority").unwrap(), "none");
    assert_eq!(row.try_get::<Option<String>, _>("due_date").unwrap(), None);
    assert_eq!(row.try_get::<i64, _>("rank").unwrap(), 1_024);
    assert_eq!(row.try_get::<i64, _>("version").unwrap(), 1);
    assert_eq!(
        row.try_get::<Option<String>, _>("completed_at").unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("trashed_at").unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<String, _>("created_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );
    assert_eq!(
        row.try_get::<String, _>("updated_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );
    assert!(
        sqlx::query("UPDATE issues SET issue_number = 99 WHERE id = ?")
            .bind(&issue.id)
            .execute(database.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE issues SET creator_user_id = ? WHERE id = ?")
            .bind(&guest_assignee)
            .bind(&issue.id)
            .execute(database.pool())
            .await
            .is_err()
    );

    let defaults = sqlx::query(
        "SELECT issue_types.system_key AS type_key, issue_types.name AS type_name, \
                statuses.system_key AS status_key, statuses.name AS status_name \
         FROM issue_types JOIN statuses \
           ON statuses.project_id = issue_types.project_id \
         WHERE issue_types.id = ? AND statuses.id = ?",
    )
    .bind(&issue.issue_type_id)
    .bind(&issue.status_id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(defaults.try_get::<String, _>("type_key").unwrap(), "task");
    assert_eq!(
        defaults.try_get::<String, _>("type_name").unwrap(),
        "Work item"
    );
    assert_eq!(defaults.try_get::<String, _>("status_key").unwrap(), "todo");
    assert_eq!(
        defaults.try_get::<String, _>("status_name").unwrap(),
        "Ready"
    );

    let project_counter: i64 =
        sqlx::query_scalar("SELECT next_issue_number FROM projects WHERE id = ?")
            .bind(&project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(project_counter, 2);
    assert!(
        sqlx::query("UPDATE projects SET next_issue_number = 1 WHERE id = ?")
            .bind(&project_id)
            .execute(database.pool())
            .await
            .is_err()
    );

    let event = sqlx::query(
        "SELECT project_id, issue_id, actor_user_id, event_kind, occurred_at \
         FROM issue_events WHERE issue_id = ?",
    )
    .bind(&issue.id)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        event.try_get::<String, _>("project_id").unwrap(),
        project_id
    );
    assert_eq!(event.try_get::<String, _>("issue_id").unwrap(), issue.id);
    assert_eq!(event.try_get::<String, _>("actor_user_id").unwrap(), owner);
    assert_eq!(
        event.try_get::<String, _>("event_kind").unwrap(),
        "issue.created"
    );
    assert_eq!(
        event.try_get::<String, _>("occurred_at").unwrap(),
        "2026-10-12T13:14:15.123456789Z"
    );
}

#[tokio::test]
async fn title_and_description_limits_are_enforced_before_persistence() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let project_id = create_project(&database, &owner, "LIM").await;
    let store = IssueStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    for title in ["", "   ", &"🧩".repeat(241)] {
        assert!(matches!(
            store
                .create(
                    &owner,
                    &project_id,
                    NewIssue {
                        title,
                        description: None,
                        assignee_user_id: None,
                    },
                    now,
                )
                .await,
            Err(IssueCreationError::InvalidTitle)
        ));
    }
    assert!(matches!(
        store
            .create(
                &owner,
                &project_id,
                NewIssue {
                    title: "Oversized description",
                    description: Some(&"x".repeat(65_537)),
                    assignee_user_id: None,
                },
                now,
            )
            .await,
        Err(IssueCreationError::DescriptionTooLong)
    ));

    let accepted_title = "🧩".repeat(240);
    let accepted_description = "x".repeat(65_536);
    let issue = store
        .create(
            &owner,
            &project_id,
            NewIssue {
                title: &accepted_title,
                description: Some(&accepted_description),
                assignee_user_id: None,
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(issue.issue_number, 1);
    assert_eq!(issue.title.chars().count(), 240);
    assert_eq!(issue.description.as_ref().unwrap().len(), 65_536);

    let issue_count: i64 = sqlx::query_scalar("SELECT count(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(issue_count, 1);
}

#[tokio::test]
async fn current_project_membership_and_assignee_eligibility_are_required() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let member = create_user(&database, "active", "member").await;
    let viewer = create_user(&database, "active", "member").await;
    let guest_viewer = create_user(&database, "active", "guest").await;
    let banned_member = create_user(&database, "active", "member").await;
    let organization_admin = create_user(&database, "active", "admin").await;
    let project_id = create_project(&database, &owner, "ACL").await;
    add_project_member(&database, &project_id, &member, "member").await;
    add_project_member(&database, &project_id, &viewer, "viewer").await;
    add_project_member(&database, &project_id, &guest_viewer, "viewer").await;
    add_project_member(&database, &project_id, &banned_member, "member").await;
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&banned_member)
        .execute(database.pool())
        .await
        .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    for actor in [&viewer, &organization_admin, &banned_member] {
        assert!(matches!(
            store
                .create(
                    actor,
                    &project_id,
                    NewIssue {
                        title: "Must be denied",
                        description: None,
                        assignee_user_id: None,
                    },
                    now,
                )
                .await,
            Err(IssueCreationError::Forbidden)
        ));
    }
    assert!(matches!(
        store
            .create(
                &owner,
                &project_id,
                NewIssue {
                    title: "Cannot assign viewer",
                    description: None,
                    assignee_user_id: Some(&guest_viewer),
                },
                now,
            )
            .await,
        Err(IssueCreationError::AssigneeUnavailable)
    ));
    assert!(matches!(
        store
            .create(
                &owner,
                &project_id,
                NewIssue {
                    title: "Cannot assign banned member",
                    description: None,
                    assignee_user_id: Some(&banned_member),
                },
                now,
            )
            .await,
        Err(IssueCreationError::AssigneeUnavailable)
    ));

    let issue = store
        .create(
            &member,
            &project_id,
            NewIssue {
                title: "Project members may create",
                description: None,
                assignee_user_id: None,
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(issue.issue_number, 1);

    sqlx::query("UPDATE projects SET archived_at = ? WHERE id = ?")
        .bind("2026-10-12T13:00:00.000000000Z")
        .bind(&project_id)
        .execute(database.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .create(
                &owner,
                &project_id,
                NewIssue {
                    title: "Archived project",
                    description: None,
                    assignee_user_id: None,
                },
                now,
            )
            .await,
        Err(IssueCreationError::Forbidden)
    ));

    let issue_count: i64 = sqlx::query_scalar("SELECT count(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(issue_count, 1);
    let next_issue_number: i64 =
        sqlx::query_scalar("SELECT next_issue_number FROM projects WHERE id = ?")
            .bind(&project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(next_issue_number, 2);
}

#[tokio::test]
async fn concurrent_creation_allocates_unique_never_reused_numbers_and_spaced_ranks() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let project_id = create_project(&database, &owner, "RACE").await;
    let store = IssueStore::new(database.pool().clone());
    let now = at("2026-10-12T13:14:15Z");

    let (first, second, third, fourth) = tokio::join!(
        store.create(
            &owner,
            &project_id,
            NewIssue {
                title: "Concurrent 1",
                description: None,
                assignee_user_id: None,
            },
            now
        ),
        store.create(
            &owner,
            &project_id,
            NewIssue {
                title: "Concurrent 2",
                description: None,
                assignee_user_id: None,
            },
            now
        ),
        store.create(
            &owner,
            &project_id,
            NewIssue {
                title: "Concurrent 3",
                description: None,
                assignee_user_id: None,
            },
            now
        ),
        store.create(
            &owner,
            &project_id,
            NewIssue {
                title: "Concurrent 4",
                description: None,
                assignee_user_id: None,
            },
            now
        )
    );
    let mut issue_numbers = [
        first.unwrap().issue_number,
        second.unwrap().issue_number,
        third.unwrap().issue_number,
        fourth.unwrap().issue_number,
    ];
    issue_numbers.sort_unstable();
    assert_eq!(issue_numbers, [1, 2, 3, 4]);

    let ranks = sqlx::query_scalar::<_, i64>(
        "SELECT rank FROM issues WHERE project_id = ? ORDER BY issue_number",
    )
    .bind(&project_id)
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(ranks, [1_024, 2_048, 3_072, 4_096]);

    let next_issue_number: i64 =
        sqlx::query_scalar("SELECT next_issue_number FROM projects WHERE id = ?")
            .bind(&project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(next_issue_number, 5);

    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM issue_events WHERE project_id = ?")
            .bind(&project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(event_count, 4);
}

#[tokio::test]
async fn history_failure_rolls_back_issue_and_project_counter() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let project_id = create_project(&database, &owner, "ROLL").await;
    sqlx::query(
        "CREATE TRIGGER reject_issue_creation_history \
         BEFORE INSERT ON issue_events \
         BEGIN SELECT RAISE(ABORT, 'forced history failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();
    let store = IssueStore::new(database.pool().clone());

    assert!(matches!(
        store
            .create(
                &owner,
                &project_id,
                NewIssue {
                    title: "Rollback history failure",
                    description: None,
                    assignee_user_id: None,
                },
                at("2026-10-12T13:14:15Z"),
            )
            .await,
        Err(IssueCreationError::Database(_))
    ));

    let issue_count: i64 = sqlx::query_scalar("SELECT count(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_events")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let next_issue_number: i64 =
        sqlx::query_scalar("SELECT next_issue_number FROM projects WHERE id = ?")
            .bind(&project_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(issue_count, 0);
    assert_eq!(event_count, 0);
    assert_eq!(next_issue_number, 1);

    sqlx::query("DROP TRIGGER reject_issue_creation_history")
        .execute(database.pool())
        .await
        .unwrap();
    let issue = store
        .create(
            &owner,
            &project_id,
            NewIssue {
                title: "Retry after rollback",
                description: None,
                assignee_user_id: None,
            },
            at("2026-10-12T13:14:16Z"),
        )
        .await
        .unwrap();
    assert_eq!(issue.issue_number, 1);
}

#[tokio::test]
async fn missing_or_archived_default_schema_fails_without_consuming_issue_number() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let project_id = create_project(&database, &owner, "SEED").await;
    sqlx::query(
        "UPDATE statuses SET archived_at = '2026-10-12T13:00:00.000000000Z' \
         WHERE project_id = ? AND system_key = 'todo'",
    )
    .bind(&project_id)
    .execute(database.pool())
    .await
    .unwrap();
    let store = IssueStore::new(database.pool().clone());
    let input = NewIssue {
        title: "Needs default status",
        description: None,
        assignee_user_id: None,
    };

    assert!(matches!(
        store
            .create(&owner, &project_id, input, at("2026-10-12T13:14:15Z"))
            .await,
        Err(IssueCreationError::DefaultSchemaUnavailable)
    ));
    sqlx::query(
        "UPDATE statuses SET archived_at = NULL WHERE project_id = ? AND system_key = 'todo'",
    )
    .bind(&project_id)
    .execute(database.pool())
    .await
    .unwrap();
    let issue = store
        .create(&owner, &project_id, input, at("2026-10-12T13:14:16Z"))
        .await
        .unwrap();
    assert_eq!(issue.issue_number, 1);
}

#[tokio::test]
async fn composite_foreign_keys_reject_type_or_status_from_another_project() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    create_organization(&database).await;
    let owner = create_user(&database, "active", "owner").await;
    let first_project = create_project(&database, &owner, "ONE").await;
    let second_project = create_project(&database, &owner, "TWO").await;
    let type_and_status = sqlx::query(
        "SELECT issue_types.id AS issue_type_id, statuses.id AS status_id \
         FROM issue_types JOIN statuses \
           ON statuses.project_id = issue_types.project_id \
         WHERE issue_types.project_id = ? \
           AND issue_types.system_key = 'task' AND statuses.system_key = 'todo'",
    )
    .bind(&second_project)
    .fetch_one(database.pool())
    .await
    .unwrap();
    let foreign_type_id: String = type_and_status.try_get("issue_type_id").unwrap();
    let foreign_status_id: String = type_and_status.try_get("status_id").unwrap();

    assert!(
        sqlx::query(
            "INSERT INTO issues \
         (id, project_id, issue_number, issue_type_id, status_id, title, \
          creator_user_id, priority, rank, version, created_at, updated_at) \
         VALUES (?, ?, 1, ?, ?, 'Cross-project reference', ?, 'none', 1024, 1, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&first_project)
        .bind(foreign_type_id)
        .bind(foreign_status_id)
        .bind(&owner)
        .bind("2026-10-12T13:14:15.000000000Z")
        .bind("2026-10-12T13:14:15.000000000Z")
        .execute(database.pool())
        .await
        .is_err()
    );

    let issue_count: i64 = sqlx::query_scalar("SELECT count(*) FROM issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(issue_count, 0);
}
