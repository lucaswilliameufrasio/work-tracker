use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{SessionPolicy, SessionStore},
    },
    issues::{IssueStore, NewIssue, creation::SessionIssueWriter},
    projects::ProjectStore,
};
use tempfile::TempDir;
use uuid::Uuid;

type OutboxSnapshot = (String, String, i64, String, String, String, i64, String);
type ReceiptSnapshot = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
);
type StructuredEventSnapshot = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[tokio::test]
async fn migration_preserves_existing_receipts_and_envelopes_and_supports_repeated_content_free_changes()
 {
    let directory = TempDir::new().unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("upgrade.sqlite3"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let mut paths =
        std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.file_name().unwrap() == "0032_repeated_issue_text_history.sql" {
            break;
        }
        // Audited repository migrations, only in this isolated file-backed DB.
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(path).unwrap()))
            .execute(&pool)
            .await
            .unwrap();
    }
    let actor = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'History upgrade', 'en', 'UTC', 'now', 'now')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic editor', 'active', 'now', 'now')").bind(&actor).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')").bind(&actor).execute(&pool).await.unwrap();
    let project = ProjectStore::new(pool.clone())
        .create(&actor, "EDIT", "History upgrade", None, Utc::now())
        .await
        .unwrap();
    let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
    let session = SessionStore::new(pool.clone(), policy)
        .issue(&actor, Utc::now())
        .await
        .unwrap();
    let master = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
    let outcome = SessionIssueWriter::new(pool.clone(), policy, master)
        .create(
            &session.token,
            &project.id,
            &Uuid::now_v7().to_string(),
            NewIssue {
                title: "Current private text",
                description: Some("Current description"),
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let issue = &outcome.receipt().issue_id;
    let store = IssueStore::new(pool.clone());
    let outbox_sql = "SELECT id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at FROM outbox_events ORDER BY id";
    let outbox_before: Vec<OutboxSnapshot> =
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap();
    let receipts_sql = "SELECT actor_user_id, operation, hex(key_digest), hex(request_fingerprint), hex(master_key_id), project_id, issue_id, issue_number, created_at FROM issue_creation_receipts";
    let receipts_before: Vec<ReceiptSnapshot> =
        sqlx::query_as(receipts_sql).fetch_all(&pool).await.unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0032_repeated_issue_text_history.sql"
    ))
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        outbox_before,
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        receipts_before,
        sqlx::query_as(receipts_sql).fetch_all(&pool).await.unwrap()
    );
    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0033_issue_text_receipts.sql"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        outbox_before,
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        receipts_before,
        sqlx::query_as(receipts_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM issue_text_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    for (index, kind) in [
        "issue.title_updated",
        "issue.description_updated",
        "issue.text_updated",
        "issue.title_updated",
    ]
    .into_iter()
    .enumerate()
    {
        let timestamp = (Utc::now() + TimeDelta::seconds(index as i64 + 1))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, ?, ?)").bind(Uuid::now_v7().to_string()).bind(&project.id).bind(issue).bind(&actor).bind(kind).bind(&timestamp).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, ?, 1, ?, ?, ?, ?, ?)").bind(Uuid::now_v7().to_string()).bind(kind).bind(&project.id).bind(issue).bind(&actor).bind(index as i64 + 2).bind(&timestamp).execute(&pool).await.unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM issue_events WHERE issue_id = ?")
            .bind(issue)
            .fetch_one(&pool)
            .await
            .unwrap(),
        5
    );
    for (kind, version) in [
        ("issue.created", 2_i64),
        ("issue.text_updated", 1),
        ("unknown", 2),
    ] {
        assert!(sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, ?, 1, ?, ?, ?, ?, 'now')").bind(Uuid::now_v7().to_string()).bind(kind).bind(&project.id).bind(issue).bind(&actor).bind(version).execute(&pool).await.is_err());
    }
    assert!(sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, 'issue.created', 'now')").bind(Uuid::now_v7().to_string()).bind(&project.id).bind(issue).bind(&actor).execute(&pool).await.is_err());

    let events_sql = "SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at FROM issue_events ORDER BY id";
    let events_before: Vec<(String, String, String, String, String, String)> =
        sqlx::query_as(events_sql).fetch_all(&pool).await.unwrap();
    let outbox_before_moves: Vec<OutboxSnapshot> =
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0034_issue_moves.sql"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        events_before,
        sqlx::query_as(events_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        outbox_before_moves,
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM issue_move_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let moved_at =
        (Utc::now() + TimeDelta::seconds(10)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, 'issue.moved', ?)")
        .bind(Uuid::now_v7().to_string())
        .bind(&project.id)
        .bind(issue)
        .bind(&actor)
        .bind(&moved_at)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, 'issue.moved', 1, ?, ?, ?, 6, ?)")
        .bind(Uuid::now_v7().to_string())
        .bind(&project.id)
        .bind(issue)
        .bind(&actor)
        .bind(&moved_at)
        .execute(&pool)
        .await
        .unwrap();
    let events_before_structured: Vec<(String, String, String, String, String, String)> =
        sqlx::query_as(events_sql).fetch_all(&pool).await.unwrap();
    let outbox_before_structured: Vec<OutboxSnapshot> =
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0035_issue_structured_fields.sql"
    ))
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        events_before_structured,
        sqlx::query_as(events_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        outbox_before_structured,
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap()
    );
    let legacy_history = store
        .get_project_issue(&actor, &project.id, 1)
        .await
        .unwrap()
        .history;
    assert_eq!(legacy_history.len(), 6);

    let priority_at =
        (Utc::now() + TimeDelta::seconds(11)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query(
        "INSERT INTO issue_events \
         (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
          field_key, before_value_json, after_value_json) \
         VALUES (?, ?, ?, ?, 'issue.field_updated', ?, 'priority', '\"none\"', '\"high\"')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&project.id)
    .bind(issue)
    .bind(&actor)
    .bind(&priority_at)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, 'issue.field_updated', 1, ?, ?, ?, 7, ?)")
        .bind(Uuid::now_v7().to_string())
        .bind(&project.id)
        .bind(issue)
        .bind(&actor)
        .bind(&priority_at)
        .execute(&pool)
        .await
        .unwrap();
    let history = store
        .get_project_issue(&actor, &project.id, 1)
        .await
        .unwrap()
        .history;
    assert_eq!(history.len(), 7);
    assert_eq!(
        history.last().unwrap().change,
        Some(puxiri::issues::IssueFieldChange::Priority {
            before: puxiri::issues::IssuePriority::None,
            after: puxiri::issues::IssuePriority::High,
        })
    );
    for (field_key, before, after) in [
        ("priority", "invalid", "\"urgent\""),
        ("priority", "[]", "\"urgent\""),
        ("priority", "\"custom\"", "\"urgent\""),
        ("due_date", "\"none\"", "\"urgent\""),
        ("priority", "\"high\"", "\"high\""),
    ] {
        assert!(sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, field_key, before_value_json, after_value_json) VALUES (?, ?, ?, ?, 'issue.field_updated', 'now', ?, ?, ?)")
            .bind(Uuid::now_v7().to_string())
            .bind(&project.id)
            .bind(issue)
            .bind(&actor)
            .bind(field_key)
            .bind(before)
            .bind(after)
            .execute(&pool)
            .await
            .is_err());
    }
    assert!(sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, 'issue.field_updated', 'now')")
        .bind(Uuid::now_v7().to_string())
        .bind(&project.id)
        .bind(issue)
        .bind(&actor)
        .execute(&pool)
        .await
        .is_err());
    assert!(
        sqlx::query("UPDATE outbox_events SET issue_version = issue_version")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM issue_creation_receipts")
            .execute(&pool)
            .await
            .is_err()
    );
    let structured_events_sql = "SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at, field_key, before_value_json, after_value_json FROM issue_events ORDER BY id";
    let structured_events_before: Vec<StructuredEventSnapshot> =
        sqlx::query_as(structured_events_sql)
            .fetch_all(&pool)
            .await
            .unwrap();
    let due_migration_outbox_before: Vec<OutboxSnapshot> =
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0036_issue_due_dates.sql"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        structured_events_before,
        sqlx::query_as(structured_events_sql)
            .fetch_all(&pool)
            .await
            .unwrap()
    );
    assert_eq!(
        due_migration_outbox_before,
        sqlx::query_as(outbox_sql).fetch_all(&pool).await.unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM issue_priority_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let due_at =
        (Utc::now() + TimeDelta::seconds(12)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query(
        "INSERT INTO issue_events \
         (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
          field_key, before_value_json, after_value_json) \
         VALUES (?, ?, ?, ?, 'issue.field_updated', ?, 'due_date', 'null', '\"2028-02-29\"')",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&project.id)
    .bind(issue)
    .bind(&actor)
    .bind(due_at)
    .execute(&pool)
    .await
    .unwrap();
    let history = store
        .get_project_issue(&actor, &project.id, 1)
        .await
        .unwrap()
        .history;
    assert_eq!(history.len(), 8);
    assert_eq!(
        history.last().unwrap().change,
        Some(puxiri::issues::IssueFieldChange::DueDate {
            before: None,
            after: Some(chrono::NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()),
        })
    );
    for (before, after) in [("\"none\"", "\"2028-02-29\""), ("null", "null")] {
        assert!(sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, field_key, before_value_json, after_value_json) VALUES (?, ?, ?, ?, 'issue.field_updated', 'now', 'due_date', ?, ?)")
            .bind(Uuid::now_v7().to_string())
            .bind(&project.id)
            .bind(issue)
            .bind(&actor)
            .bind(before)
            .bind(after)
            .execute(&pool)
            .await
            .is_err());
    }
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}
