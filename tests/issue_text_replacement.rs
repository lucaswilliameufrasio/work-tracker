use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::{MasterKey, MasterKeyStore},
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    issues::{
        IssueStore, NewIssue,
        text::{IssueTextError, ReplaceIssueText, SessionIssueTextWriter},
    },
    projects::ProjectStore,
};
use tempfile::TempDir;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}
fn input(version: i64, title: &str) -> ReplaceIssueText<'_> {
    ReplaceIssueText {
        expected_version: version,
        title,
        description: Some("Original description"),
    }
}

async fn project_member(f: &Fixture, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Another synthetic editor', 'active', 'now', 'now')").bind(&id).execute(f.db.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'member', 'now')").bind(&id).execute(f.db.pool()).await.unwrap();
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, ?, 'now')").bind(&f.project).bind(&id).bind(role).execute(f.db.pool()).await.unwrap();
    id
}
struct Fixture {
    dir: TempDir,
    db: Database,
    actor: String,
    project: String,
    issue: String,
    session: IssuedSession,
    master: MasterKey,
    writer: SessionIssueTextWriter,
}
impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        let actor = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Text fixture', 'en', 'UTC', 'now', 'now')").execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic editor', 'active', 'now', 'now')").bind(&actor).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, 'owner', 'now')").bind(&actor).execute(db.pool()).await.unwrap();
        let project = ProjectStore::new(db.pool().clone())
            .create(&actor, "EDIT", "Text fixture", None, Utc::now())
            .await
            .unwrap()
            .id;
        let issue = IssueStore::new(db.pool().clone())
            .create(
                &actor,
                &project,
                NewIssue {
                    title: "Original",
                    description: Some("Original description"),
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
        let master = MasterKeyStore::load_or_create(&dir.path().join("master.key")).unwrap();
        let writer = SessionIssueTextWriter::new(db.pool().clone(), policy(), master.clone());
        Self {
            dir,
            db,
            actor,
            project,
            issue,
            session,
            master,
            writer,
        }
    }
    async fn counts(&self) -> (i64, i64, i64, i64) {
        sqlx::query_as("SELECT version, (SELECT COUNT(*) FROM issue_events), (SELECT COUNT(*) FROM outbox_events), (SELECT COUNT(*) FROM issue_text_receipts) FROM issues WHERE id = ?").bind(&self.issue).fetch_one(self.db.pool()).await.unwrap()
    }
}

#[tokio::test]
async fn replacement_journals_each_changed_field_and_replays_original_receipt_after_later_edits() {
    let f = Fixture::new().await;
    let before: (String, String, String, i64, i64, String) = sqlx::query_as("SELECT status_id, issue_type_id, priority, rank, issue_number, creator_user_id FROM issues WHERE id = ?").bind(&f.issue).fetch_one(f.db.pool()).await.unwrap();
    let key = Uuid::now_v7().to_string();
    let first = f
        .writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, " Changed "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(first.receipt.changed);
    assert_eq!(first.receipt.result_version, 2);
    assert!(!first.replayed);
    let description_only = ReplaceIssueText {
        expected_version: 2,
        title: "Changed",
        description: None,
    };
    f.writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &Uuid::now_v7().to_string(),
            description_only,
            Utc::now(),
        )
        .await
        .unwrap();
    f.writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &Uuid::now_v7().to_string(),
            input(3, "Latest"),
            Utc::now(),
        )
        .await
        .unwrap();
    let replay = f
        .writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, " Changed "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(f.counts().await, (4, 4, 4, 3));
    let after = sqlx::query_as("SELECT status_id, issue_type_id, priority, rank, issue_number, creator_user_id FROM issues WHERE id = ?").bind(&f.issue).fetch_one(f.db.pool()).await.unwrap();
    assert_eq!(before, after);
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT event_kind FROM issue_events ORDER BY occurred_at, id")
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    assert_eq!(
        kinds,
        [
            "issue.created",
            "issue.title_updated",
            "issue.description_updated",
            "issue.text_updated"
        ]
    );
    let text: (String, Option<String>) =
        sqlx::query_as("SELECT title, description FROM issues WHERE id = ?")
            .bind(&f.issue)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(
        text,
        ("Latest".to_owned(), Some("Original description".to_owned()))
    );
}

#[tokio::test]
async fn no_op_receipt_preserves_version_and_timestamp_and_cannot_later_turn_into_a_write() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let before: String = sqlx::query_scalar("SELECT updated_at FROM issues WHERE id = ?")
        .bind(&f.issue)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    let noop = f
        .writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, " Original "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(!noop.receipt.changed);
    assert_eq!(f.counts().await, (1, 1, 1, 1));
    let after: String = sqlx::query_scalar("SELECT updated_at FROM issues WHERE id = ?")
        .bind(&f.issue)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(before, after);
    f.writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &Uuid::now_v7().to_string(),
            input(1, "Changed"),
            Utc::now(),
        )
        .await
        .unwrap();
    let replay = f
        .writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, " Original "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(replay.receipt, noop.receipt);
    assert!(replay.replayed);
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Original"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::IdempotencyConflict)
    ));
    assert_eq!(f.counts().await, (2, 2, 2, 2));
}

#[tokio::test]
async fn concurrent_duplicate_and_competing_version_writes_have_one_mutation() {
    for duplicate in [true, false] {
        let f = Fixture::new().await;
        let left_key = Uuid::now_v7().to_string();
        let right_key = if duplicate {
            left_key.clone()
        } else {
            Uuid::now_v7().to_string()
        };
        let (left, right) = tokio::join!(
            f.writer.replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &left_key,
                input(1, "Changed"),
                Utc::now()
            ),
            f.writer.replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &right_key,
                input(1, "Changed"),
                Utc::now()
            )
        );
        if duplicate {
            let left = left.unwrap();
            let right = right.unwrap();
            assert_ne!(left.replayed, right.replayed);
            assert_eq!(left.receipt, right.receipt);
        } else {
            assert!(matches!(
                (&left, &right),
                (Ok(_), Err(IssueTextError::VersionConflict))
                    | (Err(IssueTextError::VersionConflict), Ok(_))
            ));
        }
        assert_eq!(f.counts().await, (2, 2, 2, 1));
    }
}

#[tokio::test]
async fn failed_history_outbox_or_receipt_rolls_back_text_version_and_journal() {
    for (create, drop) in [
        (
            "CREATE TRIGGER text_fail BEFORE INSERT ON issue_events BEGIN SELECT RAISE(FAIL, 'fixture'); END",
            "DROP TRIGGER text_fail",
        ),
        (
            "CREATE TRIGGER text_fail BEFORE INSERT ON outbox_events BEGIN SELECT RAISE(FAIL, 'fixture'); END",
            "DROP TRIGGER text_fail",
        ),
        (
            "CREATE TRIGGER text_fail BEFORE INSERT ON issue_text_receipts BEGIN SELECT RAISE(FAIL, 'fixture'); END",
            "DROP TRIGGER text_fail",
        ),
    ] {
        let f = Fixture::new().await;
        let key = Uuid::now_v7().to_string();
        sqlx::raw_sql(create).execute(f.db.pool()).await.unwrap();
        assert!(
            f.writer
                .replace(
                    &f.session.token,
                    &f.project,
                    &f.issue,
                    &key,
                    input(1, "Changed"),
                    Utc::now()
                )
                .await
                .is_err()
        );
        assert_eq!(f.counts().await, (1, 1, 1, 0));
        let title: String = sqlx::query_scalar("SELECT title FROM issues WHERE id = ?")
            .bind(&f.issue)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
        assert_eq!(title, "Original");
        sqlx::raw_sql(drop).execute(f.db.pool()).await.unwrap();
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(f.counts().await, (2, 2, 2, 1));
    }
}

#[tokio::test]
async fn current_authority_archive_trash_and_fingerprint_key_are_checked_on_replay() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    f.writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, "Changed"),
            Utc::now(),
        )
        .await
        .unwrap();
    let other = MasterKeyStore::load_or_create(&f.dir.path().join("other.key")).unwrap();
    assert_ne!(f.master.key_id(), other.key_id());
    let writer = SessionIssueTextWriter::new(f.db.pool().clone(), policy(), other);
    assert!(matches!(
        writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::FingerprintKeyUnavailable)
    ));
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.actor, &f.project, true, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Unavailable)
    ));
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.actor, &f.project, false, Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET trashed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&f.issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Unavailable)
    ));
    sqlx::query("UPDATE issues SET trashed_at = NULL WHERE id = ?")
        .bind(&f.issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    project_member(&f, "manager").await;
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE user_id = ?")
        .bind(&f.actor)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Unavailable)
    ));
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.actor, &f.session.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Unauthenticated)
    ));
    assert_eq!(f.counts().await, (2, 2, 2, 1));
}

#[tokio::test]
async fn key_scope_is_actor_operation_across_target_issues_without_cross_actor_collisions() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    f.writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, "Changed"),
            Utc::now(),
        )
        .await
        .unwrap();
    let other_issue = IssueStore::new(f.db.pool().clone())
        .create(
            &f.actor,
            &f.project,
            NewIssue {
                title: "Other issue",
                description: None,
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &other_issue.id,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::IdempotencyConflict)
    ));
    let member = project_member(&f, "member").await;
    let session = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&member, Utc::now())
        .await
        .unwrap();
    let outcome = f
        .writer
        .replace(
            &session.token,
            &f.project,
            &f.issue,
            &key,
            input(2, "Another actor"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(!outcome.replayed);
    assert_eq!(outcome.receipt.result_version, 3);
    let created = puxiri::issues::creation::SessionIssueWriter::new(
        f.db.pool().clone(),
        policy(),
        f.master.clone(),
    )
    .create(
        &f.session.token,
        &f.project,
        &key,
        NewIssue {
            title: "Independent creation namespace",
            description: None,
            assignee_user_id: None,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(created.receipt().issue_number, 3);
}

#[tokio::test]
async fn validation_version_exhaustion_and_clock_regression_leave_no_receipt() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(0, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::InvalidVersion)
    ));
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                "bad",
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::InvalidIdempotencyKey)
    ));
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, " "),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Validation(_))
    ));
    let big = "é".repeat(32769);
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                ReplaceIssueText {
                    description: Some(&big),
                    ..input(1, "Changed")
                },
                Utc::now()
            )
            .await,
        Err(IssueTextError::Validation(_))
    ));
    sqlx::query("UPDATE issues SET updated_at = ? WHERE id = ?")
        .bind(
            (Utc::now() + TimeDelta::minutes(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        )
        .bind(&f.issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::TimeRegression)
    ));
    sqlx::query("UPDATE issues SET version = 9223372036854775807 WHERE id = ?")
        .bind(&f.issue)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .replace(
                &f.session.token,
                &f.project,
                &f.issue,
                &key,
                input(i64::MAX, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::VersionExhausted)
    ));
    assert_eq!(f.counts().await, (i64::MAX, 1, 1, 0));
}

#[tokio::test]
async fn receipts_survive_cross_session_restart_and_are_immutable_without_free_text() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let first = f
        .writer
        .replace(
            &f.session.token,
            &f.project,
            &f.issue,
            &key,
            input(1, "Changed"),
            Utc::now(),
        )
        .await
        .unwrap();
    let second = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&f.actor, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        f.writer
            .replace(
                &second.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await
            .unwrap()
            .receipt,
        first.receipt
    );
    assert!(
        sqlx::query("UPDATE issue_text_receipts SET changed = changed")
            .execute(f.db.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM issue_text_receipts")
            .execute(f.db.pool())
            .await
            .is_err()
    );
    let fields: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('issue_text_receipts')")
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    for name in ["title", "description", "body", "payload", "idempotency_key"] {
        assert!(!fields.iter().any(|field| field == name));
    }
    f.db.close().await;
    drop(f.db);
    let db = Database::open(f.dir.path()).await.unwrap();
    let writer = SessionIssueTextWriter::new(db.pool().clone(), policy(), f.master);
    let replay = writer
        .replace(
            &second.token,
            &f.project,
            &f.issue,
            &key,
            input(1, "Changed"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    sqlx::query("DELETE FROM outbox_events WHERE issue_id = ?")
        .bind(&f.issue)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM issue_events WHERE issue_id = ?")
        .bind(&f.issue)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM issues WHERE id = ?")
        .bind(&f.issue)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        writer
            .replace(
                &second.token,
                &f.project,
                &f.issue,
                &key,
                input(1, "Changed"),
                Utc::now()
            )
            .await,
        Err(IssueTextError::Unavailable)
    ));
}
