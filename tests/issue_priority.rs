use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    issues::{
        IssuePriority, IssueStore, NewIssue,
        fields::{IssueFieldError, SessionIssueFieldWriter, SetIssuePriority},
    },
    projects::ProjectStore,
};
use tempfile::TempDir;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}

struct Fixture {
    directory: TempDir,
    db: Database,
    actor: String,
    project: String,
    issue: String,
    session: IssuedSession,
    writer: SessionIssueFieldWriter,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = Database::open(directory.path()).await.unwrap();
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Priority fixture', 'en', 'UTC', 'now', 'now')")
            .execute(db.pool())
            .await
            .unwrap();
        let actor = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Priority editor', 'active', 'now', 'now')")
            .bind(&actor)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')")
            .bind(&actor)
            .execute(db.pool())
            .await
            .unwrap();
        let project = ProjectStore::new(db.pool().clone())
            .create(&actor, "PRIO", "Priority fixture", None, Utc::now())
            .await
            .unwrap()
            .id;
        let issue = IssueStore::new(db.pool().clone())
            .create(
                &actor,
                &project,
                NewIssue {
                    title: "Issue",
                    description: Some("Current description"),
                    assignee_user_id: None,
                },
                Utc::now(),
            )
            .await
            .unwrap()
            .id;
        let session = SessionStore::new(db.pool().clone(), policy())
            .issue(&actor, Utc::now())
            .await
            .unwrap();
        let master = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
        let writer = SessionIssueFieldWriter::new(db.pool().clone(), policy(), master);
        Self {
            directory,
            db,
            actor,
            project,
            issue,
            session,
            writer,
        }
    }

    async fn set(
        &self,
        expected_version: i64,
        priority: IssuePriority,
        key: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<puxiri::issues::fields::IssuePriorityOutcome, IssueFieldError> {
        self.writer
            .set_priority(
                &self.session.token,
                &self.project,
                1,
                key,
                SetIssuePriority {
                    expected_version,
                    priority,
                },
                now,
            )
            .await
    }

    async fn counts(&self) -> (i64, String, i64, i64, i64, i64) {
        sqlx::query_as(
            "SELECT version, priority, \
                    (SELECT COUNT(*) FROM issue_events WHERE issue_id = issues.id), \
                    (SELECT COUNT(*) FROM outbox_events WHERE issue_id = issues.id), \
                    (SELECT COUNT(*) FROM issue_priority_receipts WHERE issue_id = issues.id), \
                    (SELECT COUNT(*) FROM issue_text_receipts WHERE issue_id = issues.id) \
             FROM issues WHERE id = ?",
        )
        .bind(&self.issue)
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn priority_change_uses_expected_version_receipts_and_structured_history() {
    let fixture = Fixture::new().await;
    let original = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT status_id, issue_type_id, title, rank FROM issues WHERE id = ?",
    )
    .bind(&fixture.issue)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    let first_key = Uuid::now_v7().to_string();
    let first_attempt = fixture.set(1, IssuePriority::High, &first_key, Utc::now());
    let second_attempt = fixture.set(1, IssuePriority::High, &first_key, Utc::now());
    let (first_attempt, second_attempt) = tokio::join!(first_attempt, second_attempt);
    let first_attempt = first_attempt.unwrap();
    let second_attempt = second_attempt.unwrap();
    assert_ne!(first_attempt.replayed, second_attempt.replayed);
    assert_eq!(first_attempt.receipt, second_attempt.receipt);
    let first = if first_attempt.replayed {
        second_attempt
    } else {
        first_attempt
    };
    assert!(first.receipt.changed);
    assert_eq!(first.receipt.result_version, 2);
    assert_eq!(first.receipt.priority, IssuePriority::High);
    assert_eq!(fixture.counts().await, (2, "high".to_owned(), 2, 2, 1, 0));

    let history = IssueStore::new(fixture.db.pool().clone())
        .get_project_issue(&fixture.actor, &fixture.project, 1)
        .await
        .unwrap()
        .history;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history.last().unwrap().change,
        Some(puxiri::issues::IssueFieldChange::Priority {
            before: IssuePriority::None,
            after: IssuePriority::High,
        })
    );
    let details = IssueStore::new(fixture.db.pool().clone())
        .get_project_issue(&fixture.actor, &fixture.project, 1)
        .await
        .unwrap()
        .issue;
    assert_eq!(
        (
            details.status_id,
            details.issue_type_id,
            details.title,
            details.rank
        ),
        original
    );

    let replay = fixture
        .set(1, IssuePriority::High, &first_key, Utc::now())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    assert!(matches!(
        fixture
            .set(1, IssuePriority::Low, &first_key, Utc::now())
            .await,
        Err(IssueFieldError::IdempotencyConflict)
    ));

    let no_op_key = Uuid::now_v7().to_string();
    let no_op = fixture
        .set(2, IssuePriority::High, &no_op_key, Utc::now())
        .await
        .unwrap();
    assert!(!no_op.replayed);
    assert!(!no_op.receipt.changed);
    assert_eq!(no_op.receipt.result_version, 2);
    assert_eq!(fixture.counts().await, (2, "high".to_owned(), 2, 2, 2, 0));
    assert!(matches!(
        fixture
            .set(
                1,
                IssuePriority::Low,
                &Uuid::now_v7().to_string(),
                Utc::now()
            )
            .await,
        Err(IssueFieldError::VersionConflict)
    ));

    let second_key = Uuid::now_v7().to_string();
    fixture
        .set(2, IssuePriority::Low, &second_key, Utc::now())
        .await
        .unwrap();
    let original_replay = fixture
        .set(1, IssuePriority::High, &first_key, Utc::now())
        .await
        .unwrap();
    assert!(original_replay.replayed);
    assert_eq!(original_replay.receipt, first.receipt);
    assert_eq!(fixture.counts().await, (3, "low".to_owned(), 3, 3, 3, 0));
}

#[tokio::test]
async fn failed_outbox_rolls_back_priority_history_version_and_receipt_then_retry_commits_once() {
    let fixture = Fixture::new().await;
    sqlx::query("CREATE TRIGGER reject_priority_outbox BEFORE INSERT ON outbox_events WHEN NEW.event_kind = 'issue.field_updated' BEGIN SELECT RAISE(FAIL, 'fixture'); END")
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let key = Uuid::now_v7().to_string();
    assert!(matches!(
        fixture
            .set(1, IssuePriority::Urgent, &key, Utc::now())
            .await,
        Err(IssueFieldError::Database(_))
    ));
    assert_eq!(fixture.counts().await, (1, "none".to_owned(), 1, 1, 0, 0));
    sqlx::query("DROP TRIGGER reject_priority_outbox")
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let saved = fixture
        .set(1, IssuePriority::Urgent, &key, Utc::now())
        .await
        .unwrap();
    assert!(saved.receipt.changed);
    assert_eq!(fixture.counts().await, (2, "urgent".to_owned(), 2, 2, 1, 0));
}

#[tokio::test]
async fn priority_mutation_revalidates_membership_project_state_and_trash() {
    let fixture = Fixture::new().await;
    let viewer = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Viewer', 'active', 'now', 'now')")
        .bind(&viewer)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'guest', 'now')")
        .bind(&viewer)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')")
        .bind(&fixture.project)
        .bind(&viewer)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let viewer_session = SessionStore::new(fixture.db.pool().clone(), policy())
        .issue(&viewer, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .writer
            .set_priority(
                &viewer_session.token,
                &fixture.project,
                1,
                &Uuid::now_v7().to_string(),
                SetIssuePriority {
                    expected_version: 1,
                    priority: IssuePriority::High,
                },
                Utc::now(),
            )
            .await,
        Err(IssueFieldError::Unavailable)
    ));
    assert_eq!(fixture.counts().await, (1, "none".to_owned(), 1, 1, 0, 0));

    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .set(
                1,
                IssuePriority::High,
                &Uuid::now_v7().to_string(),
                Utc::now()
            )
            .await,
        Err(IssueFieldError::Unavailable)
    ));
    sqlx::query("UPDATE issues SET trashed_at = NULL WHERE id = ?")
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    ProjectStore::new(fixture.db.pool().clone())
        .set_archived(&fixture.actor, &fixture.project, true, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .set(
                1,
                IssuePriority::High,
                &Uuid::now_v7().to_string(),
                Utc::now()
            )
            .await,
        Err(IssueFieldError::Unavailable)
    ));
    assert_eq!(fixture.counts().await, (1, "none".to_owned(), 1, 1, 0, 0));
    let _keep_directory_alive = &fixture.directory;
}

#[tokio::test]
async fn priority_replay_revalidates_current_membership_and_session() {
    let fixture = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let committed = fixture
        .set(1, IssuePriority::High, &key, Utc::now())
        .await
        .unwrap();
    assert!(committed.receipt.changed);

    let second_manager = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Second manager', 'active', 'now', 'now')")
        .bind(&second_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'member', 'now')")
        .bind(&second_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'manager', 'now')")
        .bind(&fixture.project)
        .bind(&second_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    sqlx::query(
        "UPDATE project_members SET role = 'viewer' \
         WHERE project_id = ? AND user_id = ?",
    )
    .bind(&fixture.project)
    .bind(&fixture.actor)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    assert!(matches!(
        fixture.set(1, IssuePriority::High, &key, Utc::now()).await,
        Err(IssueFieldError::Unavailable)
    ));

    sqlx::query(
        "UPDATE project_members SET role = 'manager' \
         WHERE project_id = ? AND user_id = ?",
    )
    .bind(&fixture.project)
    .bind(&fixture.actor)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    SessionStore::new(fixture.db.pool().clone(), policy())
        .revoke(&fixture.actor, &fixture.session.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        fixture.set(1, IssuePriority::High, &key, Utc::now()).await,
        Err(IssueFieldError::Unauthenticated)
    ));
    assert_eq!(fixture.counts().await, (2, "high".to_owned(), 2, 2, 1, 0));
}
