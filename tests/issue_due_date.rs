use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use puxiri::{
    auth::{
        master_key::MasterKeyStore,
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    issues::{
        BoardOptions, IssueStore, NewIssue,
        fields::{IssueDueDateOutcome, IssueFieldError, SessionIssueFieldWriter, SetIssueDueDate},
        session::{IssueListOptions, SessionIssueReadError, SessionIssueReader},
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
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Due-date fixture', 'en', 'UTC', 'now', 'now')")
            .execute(db.pool())
            .await
            .unwrap();
        let actor = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Due-date editor', 'active', 'now', 'now')")
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
            .create(&actor, "DUE", "Due-date fixture", None, Utc::now())
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
        due_date: Option<NaiveDate>,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<IssueDueDateOutcome, IssueFieldError> {
        self.writer
            .set_due_date(
                &self.session.token,
                &self.project,
                1,
                key,
                SetIssueDueDate {
                    expected_version,
                    due_date,
                },
                now,
            )
            .await
    }

    async fn counts(&self) -> (i64, Option<String>, i64, i64, i64) {
        sqlx::query_as(
            "SELECT version, due_date, \
                    (SELECT COUNT(*) FROM issue_events WHERE issue_id = issues.id), \
                    (SELECT COUNT(*) FROM outbox_events WHERE issue_id = issues.id), \
                    (SELECT COUNT(*) FROM issue_due_date_receipts WHERE issue_id = issues.id) \
             FROM issues WHERE id = ?",
        )
        .bind(&self.issue)
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }
}

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
}

#[tokio::test]
async fn due_date_changes_clear_replay_and_noop_preserve_original_receipts() {
    let fixture = Fixture::new().await;
    let first_key = Uuid::now_v7().to_string();
    let selected = date("2028-02-29");
    let first = fixture
        .set(1, Some(selected), &first_key, Utc::now())
        .await
        .unwrap();
    assert!(first.receipt.changed);
    assert_eq!(first.receipt.result_version, 2);
    assert_eq!(first.receipt.due_date, Some(selected));

    let replay = fixture
        .set(1, Some(selected), &first_key, Utc::now())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    assert!(matches!(
        fixture
            .set(1, Some(date("2028-03-01")), &first_key, Utc::now())
            .await,
        Err(IssueFieldError::IdempotencyConflict)
    ));

    let no_op = fixture
        .set(2, Some(selected), &Uuid::now_v7().to_string(), Utc::now())
        .await
        .unwrap();
    assert!(!no_op.receipt.changed);
    assert_eq!(no_op.receipt.result_version, 2);
    assert_eq!(
        fixture.counts().await,
        (2, Some("2028-02-29".to_owned()), 2, 2, 2)
    );

    let cleared = fixture
        .set(2, None, &Uuid::now_v7().to_string(), Utc::now())
        .await
        .unwrap();
    assert!(cleared.receipt.changed);
    assert_eq!(cleared.receipt.due_date, None);
    assert_eq!(cleared.receipt.result_version, 3);
    assert_eq!(fixture.counts().await, (3, None, 3, 3, 3));

    let original_replay = fixture
        .set(1, Some(selected), &first_key, Utc::now())
        .await
        .unwrap();
    assert!(original_replay.replayed);
    assert_eq!(original_replay.receipt, first.receipt);

    let detail = IssueStore::new(fixture.db.pool().clone())
        .get_project_issue(&fixture.actor, &fixture.project, 1)
        .await
        .unwrap();
    assert_eq!(detail.issue.due_date, None);
    assert_eq!(
        detail.history[1].change,
        Some(puxiri::issues::IssueFieldChange::DueDate {
            before: None,
            after: Some(selected),
        })
    );
    assert_eq!(
        detail.history[2].change,
        Some(puxiri::issues::IssueFieldChange::DueDate {
            before: Some(selected),
            after: None,
        })
    );
    let _keep_directory_alive = &fixture.directory;
}

#[tokio::test]
async fn organization_local_overdue_changes_at_iana_midnight_not_utc_midnight() {
    let fixture = Fixture::new().await;
    sqlx::query("UPDATE organizations SET timezone = 'America/Los_Angeles' WHERE id = 1")
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET due_date = '2026-03-07' WHERE id = ?")
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let before_midnight = Utc.with_ymd_and_hms(2026, 3, 8, 7, 59, 59).unwrap();
    let sessions = SessionStore::new(fixture.db.pool().clone(), policy());
    let session = sessions
        .issue(&fixture.actor, before_midnight)
        .await
        .unwrap();
    let reader = SessionIssueReader::new(fixture.db.pool().clone(), policy());
    let before = reader
        .get_project_issue(&session.token, &fixture.project, 1, before_midnight)
        .await
        .unwrap();
    assert!(!before.is_overdue);
    let before_list = reader
        .list_project_issues(
            &session.token,
            &fixture.project,
            IssueListOptions {
                after_issue_number: None,
                page_size: 10,
            },
            before_midnight,
        )
        .await
        .unwrap();
    assert!(!before_list.items[0].is_overdue);
    let status_id: String = sqlx::query_scalar("SELECT status_id FROM issues WHERE id = ?")
        .bind(&fixture.issue)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    let before_board = reader
        .read_board_column(
            &session.token,
            &fixture.project,
            &status_id,
            BoardOptions {
                after: None,
                page_size: 10,
            },
            before_midnight,
        )
        .await
        .unwrap();
    assert!(!before_board.items[0].is_overdue);

    let after_midnight = Utc.with_ymd_and_hms(2026, 3, 8, 8, 0, 0).unwrap();
    let after = reader
        .get_project_issue(&session.token, &fixture.project, 1, after_midnight)
        .await
        .unwrap();
    assert!(after.is_overdue);
    let after_list = reader
        .list_project_issues(
            &session.token,
            &fixture.project,
            IssueListOptions {
                after_issue_number: None,
                page_size: 10,
            },
            after_midnight,
        )
        .await
        .unwrap();
    assert!(after_list.items[0].is_overdue);
    let after_board = reader
        .read_board_column(
            &session.token,
            &fixture.project,
            &status_id,
            BoardOptions {
                after: None,
                page_size: 10,
            },
            after_midnight,
        )
        .await
        .unwrap();
    assert!(after_board.items[0].is_overdue);

    let done_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND category = 'done' LIMIT 1",
    )
    .bind(&fixture.project)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE issues SET status_id = ? WHERE id = ?")
        .bind(done_status)
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let done = reader
        .get_project_issue(&session.token, &fixture.project, 1, after_midnight)
        .await
        .unwrap();
    assert!(!done.is_overdue);

    let canceled_status: String = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE project_id = ? AND category = 'canceled' LIMIT 1",
    )
    .bind(&fixture.project)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE issues SET status_id = ? WHERE id = ?")
        .bind(canceled_status)
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let canceled = reader
        .get_project_issue(&session.token, &fixture.project, 1, after_midnight)
        .await
        .unwrap();
    assert!(!canceled.is_overdue);
}

#[tokio::test]
async fn due_date_mutations_and_replays_revalidate_current_membership_project_and_trash() {
    let fixture = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let committed = fixture
        .set(1, Some(date("2028-01-01")), &key, Utc::now())
        .await
        .unwrap();
    assert!(committed.receipt.changed);

    let viewer = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Read-only viewer', 'active', 'now', 'now')")
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
            .set_due_date(
                &viewer_session.token,
                &fixture.project,
                1,
                &Uuid::now_v7().to_string(),
                SetIssueDueDate {
                    expected_version: 2,
                    due_date: Some(date("2029-01-01")),
                },
                Utc::now(),
            )
            .await,
        Err(IssueFieldError::Unavailable)
    ));

    let replacement_manager = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Replacement manager', 'active', 'now', 'now')")
        .bind(&replacement_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'member', 'now')")
        .bind(&replacement_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'manager', 'now')")
        .bind(&fixture.project)
        .bind(&replacement_manager)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&fixture.project)
        .bind(&fixture.actor)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .set(1, Some(date("2028-01-01")), &key, Utc::now())
            .await,
        Err(IssueFieldError::Unavailable)
    ));
    sqlx::query("UPDATE project_members SET role = 'manager' WHERE project_id = ? AND user_id = ?")
        .bind(&fixture.project)
        .bind(&fixture.actor)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .bind(&fixture.issue)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .set(
                2,
                Some(date("2029-01-01")),
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
                2,
                Some(date("2029-01-01")),
                &Uuid::now_v7().to_string(),
                Utc::now()
            )
            .await,
        Err(IssueFieldError::Unavailable)
    ));
    assert_eq!(
        fixture.counts().await,
        (2, Some("2028-01-01".to_owned()), 2, 2, 1)
    );
    let _keep_directory_alive = &fixture.directory;
}

#[tokio::test]
async fn invalid_persisted_organization_timezone_fails_closed_for_reads_and_writes() {
    let fixture = Fixture::new().await;
    sqlx::query("UPDATE organizations SET timezone = 'Invalid/Zone' WHERE id = 1")
        .execute(fixture.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .set(
                1,
                Some(date("2028-01-01")),
                &Uuid::now_v7().to_string(),
                Utc::now(),
            )
            .await,
        Err(IssueFieldError::OrganizationTimezoneUnavailable)
    ));

    let reader = SessionIssueReader::new(fixture.db.pool().clone(), policy());
    assert!(matches!(
        reader
            .get_project_issue(&fixture.session.token, &fixture.project, 1, Utc::now())
            .await,
        Err(SessionIssueReadError::Issue(
            puxiri::issues::IssueReadError::OrganizationTimezoneUnavailable
        ))
    ));
    assert_eq!(fixture.counts().await, (1, None, 1, 1, 0));
    let _keep_directory_alive = &fixture.directory;
}

#[tokio::test]
async fn failed_outbox_rolls_back_due_date_history_version_and_receipt() {
    let fixture = Fixture::new().await;
    sqlx::query("CREATE TRIGGER reject_due_date_outbox BEFORE INSERT ON outbox_events WHEN NEW.event_kind = 'issue.field_updated' BEGIN SELECT RAISE(FAIL, 'fixture'); END")
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let key = Uuid::now_v7().to_string();
    assert!(matches!(
        fixture
            .set(1, Some(date("2028-01-01")), &key, Utc::now())
            .await,
        Err(IssueFieldError::Database(_))
    ));
    assert_eq!(fixture.counts().await, (1, None, 1, 1, 0));
    sqlx::query("DROP TRIGGER reject_due_date_outbox")
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let saved = fixture
        .set(1, Some(date("2028-01-01")), &key, Utc::now())
        .await
        .unwrap();
    assert!(saved.receipt.changed);
    assert_eq!(
        fixture.counts().await,
        (2, Some("2028-01-01".to_owned()), 2, 2, 1)
    );
}
