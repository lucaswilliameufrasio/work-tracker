use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        master_key::{MasterKey, MasterKeyStore},
        session::{IssuedSession, SessionPolicy, SessionStore},
    },
    db::Database,
    issues::{
        IssueCreationError, IssueStore, NewIssue,
        creation::{IssueCreationOutcome, SessionIssueCreationError, SessionIssueWriter},
    },
    projects::ProjectStore,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

fn policy() -> SessionPolicy {
    SessionPolicy::initial(TimeDelta::minutes(5)).unwrap()
}
fn input(title: &str) -> NewIssue<'_> {
    NewIssue {
        title,
        description: Some("Private low entropy description"),
        assignee_user_id: None,
    }
}

async fn user(db: &Database, role: &str) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Synthetic mutation fixture', 'active', 'now', 'now')").bind(&id).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')").bind(&id).bind(role).execute(db.pool()).await.unwrap();
    id
}

struct Fixture {
    dir: TempDir,
    db: Database,
    owner: String,
    project: String,
    session: IssuedSession,
    master: MasterKey,
    writer: SessionIssueWriter,
}

impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Creation fixture', 'en', 'UTC', 'now', 'now')").execute(db.pool()).await.unwrap();
        let owner = user(&db, "owner").await;
        let project = ProjectStore::new(db.pool().clone())
            .create(
                &owner,
                "DEV",
                "Synthetic mutation project",
                None,
                Utc::now(),
            )
            .await
            .unwrap()
            .id;
        let master = MasterKeyStore::load_or_create(&dir.path().join("master.key")).unwrap();
        let session = SessionStore::new(db.pool().clone(), policy())
            .issue(&owner, Utc::now())
            .await
            .unwrap();
        let writer = SessionIssueWriter::new(db.pool().clone(), policy(), master.clone());
        Self {
            dir,
            db,
            owner,
            project,
            session,
            master,
            writer,
        }
    }

    async fn counts(&self) -> (i64, i64, i64, i64, i64) {
        let row = sqlx::query("SELECT (SELECT COUNT(*) FROM issues), (SELECT COUNT(*) FROM issue_events), (SELECT COUNT(*) FROM outbox_events), (SELECT COUNT(*) FROM issue_creation_receipts), (SELECT next_issue_number FROM projects WHERE id = ?)")
            .bind(&self.project).fetch_one(self.db.pool()).await.unwrap();
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
    }
}

#[tokio::test]
async fn creation_replays_across_sessions_and_restart_without_retaining_free_text_in_receipts_or_events()
 {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let created = f
        .writer
        .create(
            &f.session.token,
            &f.project,
            &key,
            input(" Private title "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(matches!(created, IssueCreationOutcome::Created(_)));
    let receipt = created.receipt().clone();
    assert_eq!(
        (
            receipt.issue_number,
            receipt.issue_key.as_str(),
            receipt.created_version
        ),
        (1, "DEV-1", 1)
    );
    assert_eq!(f.counts().await, (1, 1, 1, 1, 2));
    let second = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&f.owner, Utc::now())
        .await
        .unwrap();
    let replay = f
        .writer
        .create(
            &second.token,
            &f.project,
            &key,
            input(" Private title "),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(replay, IssueCreationOutcome::Replayed(receipt.clone()));
    let stored = sqlx::query("SELECT actor_user_id, operation, key_digest, request_fingerprint, master_key_id, project_id, issue_id, issue_number FROM issue_creation_receipts")
        .fetch_one(f.db.pool()).await.unwrap();
    assert_eq!(stored.get::<Vec<u8>, _>("key_digest").len(), 32);
    assert_eq!(stored.get::<Vec<u8>, _>("request_fingerprint").len(), 32);
    assert_eq!(stored.get::<Vec<u8>, _>("master_key_id").len(), 16);
    for query in [
        "PRAGMA table_info(issue_creation_receipts)",
        "PRAGMA table_info(outbox_events)",
    ] {
        let fields = sqlx::query(query).fetch_all(f.db.pool()).await.unwrap();
        for field in fields {
            let name: String = field.get("name");
            assert!(
                !["title", "description", "payload", "idempotency_key", "body"]
                    .contains(&name.as_str())
            );
        }
    }
    let event = sqlx::query("SELECT event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version FROM outbox_events").fetch_one(f.db.pool()).await.unwrap();
    assert_eq!(event.get::<String, _>("event_kind"), "issue.created");
    assert_eq!(event.get::<i64, _>("payload_version"), 1);
    assert_eq!(event.get::<String, _>("issue_id"), receipt.issue_id);
    assert_eq!(event.get::<String, _>("actor_user_id"), f.owner);
    assert_eq!(event.get::<i64, _>("issue_version"), 1);
    assert!(
        sqlx::query("UPDATE outbox_events SET issue_version = 1")
            .execute(f.db.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE issue_creation_receipts SET issue_number = issue_number")
            .execute(f.db.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM issue_creation_receipts")
            .execute(f.db.pool())
            .await
            .is_err()
    );
    f.db.close().await;
    drop(f.db);
    let reopened = Database::open(f.dir.path()).await.unwrap();
    let writer = SessionIssueWriter::new(
        reopened.pool().clone(),
        policy(),
        MasterKeyStore::load_or_create(&f.dir.path().join("master.key")).unwrap(),
    );
    assert_eq!(
        writer
            .create(
                &second.token,
                &f.project,
                &key,
                input(" Private title "),
                Utc::now()
            )
            .await
            .unwrap(),
        IssueCreationOutcome::Replayed(receipt)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues")
        .fetch_one(reopened.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn receipt_scope_is_principal_and_operation_not_project_or_session_and_different_payloads_conflict()
 {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    f.writer
        .create(
            &f.session.token,
            &f.project,
            &key,
            input("Original"),
            Utc::now(),
        )
        .await
        .unwrap();
    for request in [
        input("Other title"),
        NewIssue {
            title: "Original",
            description: None,
            assignee_user_id: None,
        },
        input(" Original"),
    ] {
        assert!(matches!(
            f.writer
                .create(&f.session.token, &f.project, &key, request, Utc::now())
                .await,
            Err(SessionIssueCreationError::IdempotencyConflict)
        ));
    }
    let other_project = ProjectStore::new(f.db.pool().clone())
        .create(&f.owner, "OTHER", "Another scope", None, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &other_project.id,
                &key,
                input("Original"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::IdempotencyConflict)
    ));
    let member = user(&f.db, "guest").await;
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'member', 'now')").bind(&f.project).bind(&member).execute(f.db.pool()).await.unwrap();
    let token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&member, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &token.token,
                &f.project,
                &key,
                input("Original"),
                Utc::now()
            )
            .await
            .unwrap(),
        IssueCreationOutcome::Created(_)
    ));
    assert_eq!(f.counts().await, (2, 2, 2, 2, 3));
}

#[tokio::test]
async fn concurrent_duplicate_requests_commit_one_issue_one_history_one_event_and_one_receipt() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let (left, right) = tokio::join!(
        f.writer.create(
            &f.session.token,
            &f.project,
            &key,
            input("Concurrent"),
            Utc::now()
        ),
        f.writer.create(
            &f.session.token,
            &f.project,
            &key,
            input("Concurrent"),
            Utc::now()
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.receipt(), right.receipt());
    assert!(matches!(
        (&left, &right),
        (
            IssueCreationOutcome::Created(_),
            IssueCreationOutcome::Replayed(_)
        ) | (
            IssueCreationOutcome::Replayed(_),
            IssueCreationOutcome::Created(_)
        )
    ));
    assert_eq!(f.counts().await, (1, 1, 1, 1, 2));
}

#[tokio::test]
async fn write_failures_at_history_outbox_and_receipt_leave_no_partial_issue_or_consumed_number() {
    for (create, drop) in [
        (
            "CREATE TRIGGER fail_history BEFORE INSERT ON issue_events BEGIN SELECT RAISE(FAIL, 'fixture history failure'); END",
            "DROP TRIGGER fail_history",
        ),
        (
            "CREATE TRIGGER fail_outbox BEFORE INSERT ON outbox_events BEGIN SELECT RAISE(FAIL, 'fixture outbox failure'); END",
            "DROP TRIGGER fail_outbox",
        ),
        (
            "CREATE TRIGGER fail_receipt BEFORE INSERT ON issue_creation_receipts BEGIN SELECT RAISE(FAIL, 'fixture receipt failure'); END",
            "DROP TRIGGER fail_receipt",
        ),
    ] {
        let f = Fixture::new().await;
        let key = Uuid::now_v7().to_string();
        sqlx::raw_sql(create).execute(f.db.pool()).await.unwrap();
        assert!(
            f.writer
                .create(
                    &f.session.token,
                    &f.project,
                    &key,
                    input("Rollback"),
                    Utc::now()
                )
                .await
                .is_err()
        );
        assert_eq!(f.counts().await, (0, 0, 0, 0, 1));
        sqlx::raw_sql(drop).execute(f.db.pool()).await.unwrap();
        assert!(matches!(
            f.writer
                .create(
                    &f.session.token,
                    &f.project,
                    &key,
                    input("Rollback"),
                    Utc::now()
                )
                .await
                .unwrap(),
            IssueCreationOutcome::Created(_)
        ));
        assert_eq!(f.counts().await, (1, 1, 1, 1, 2));
    }
}

#[tokio::test]
async fn current_session_permission_and_project_archive_are_checked_even_on_exact_replay() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    let member = user(&f.db, "member").await;
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'member', 'now')").bind(&f.project).bind(&member).execute(f.db.pool()).await.unwrap();
    let token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&member, Utc::now())
        .await
        .unwrap();
    f.writer
        .create(
            &token.token,
            &f.project,
            &key,
            input("Permission"),
            Utc::now(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE project_members SET role = 'viewer' WHERE project_id = ? AND user_id = ?")
        .bind(&f.project)
        .bind(&member)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &token.token,
                &f.project,
                &key,
                input("Permission"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unavailable)
    ));
    let own_key = Uuid::now_v7().to_string();
    f.writer
        .create(
            &f.session.token,
            &f.project,
            &own_key,
            input("Archive"),
            Utc::now(),
        )
        .await
        .unwrap();
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, true, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &own_key,
                input("Archive"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unavailable)
    ));
    ProjectStore::new(f.db.pool().clone())
        .set_archived(&f.owner, &f.project, false, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &own_key,
                input("Archive"),
                Utc::now() + TimeDelta::days(8)
            )
            .await,
        Err(SessionIssueCreationError::Unauthenticated)
    ));
    SessionStore::new(f.db.pool().clone(), policy())
        .revoke(&f.owner, &f.session.id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &own_key,
                input("Archive"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unauthenticated)
    ));
    let fresh = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&f.owner, Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&f.owner)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &fresh.token,
                &f.project,
                &own_key,
                input("Archive"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unauthenticated)
    ));
    assert_eq!(f.counts().await, (2, 2, 2, 2, 3));
}

#[tokio::test]
async fn invalid_requests_and_unavailable_schema_or_assignee_do_not_reserve_keys_or_create_outbox()
{
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                "bad",
                input("Valid"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::InvalidIdempotencyKey)
    ));
    assert!(matches!(
        f.writer
            .create(&f.session.token, &f.project, &key, input(" "), Utc::now())
            .await,
        Err(SessionIssueCreationError::Creation(
            IssueCreationError::InvalidTitle
        ))
    ));
    let too_big = "é".repeat(32769);
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &key,
                NewIssue {
                    title: "Valid",
                    description: Some(&too_big),
                    assignee_user_id: None
                },
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Creation(
            IssueCreationError::DescriptionTooLong
        ))
    ));
    let viewer = user(&f.db, "member").await;
    sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')").bind(&f.project).bind(&viewer).execute(f.db.pool()).await.unwrap();
    let viewer_token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&viewer, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &viewer_token.token,
                &f.project,
                &key,
                input("Valid"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unavailable)
    ));
    let admin = user(&f.db, "admin").await;
    let admin_token = SessionStore::new(f.db.pool().clone(), policy())
        .issue(&admin, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &admin_token.token,
                &f.project,
                &key,
                input("Valid"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unavailable)
    ));
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &key,
                NewIssue {
                    title: "Valid",
                    description: None,
                    assignee_user_id: Some(&viewer)
                },
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Creation(
            IssueCreationError::AssigneeUnavailable
        ))
    ));
    sqlx::query("UPDATE issue_types SET archived_at = ? WHERE project_id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(&f.project)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &key,
                input("Valid"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Creation(
            IssueCreationError::DefaultSchemaUnavailable
        ))
    ));
    assert_eq!(f.counts().await, (0, 0, 0, 0, 1));
}

#[tokio::test]
async fn master_key_change_cannot_turn_an_existing_receipt_into_a_fresh_creation() {
    let f = Fixture::new().await;
    let key = Uuid::now_v7().to_string();
    f.writer
        .create(
            &f.session.token,
            &f.project,
            &key,
            input("Protected fingerprint"),
            Utc::now(),
        )
        .await
        .unwrap();
    let other_key =
        MasterKeyStore::load_or_create(&f.dir.path().join("different-master.key")).unwrap();
    assert_ne!(other_key.key_id(), f.master.key_id());
    let writer = SessionIssueWriter::new(f.db.pool().clone(), policy(), other_key);
    assert!(matches!(
        writer
            .create(
                &f.session.token,
                &f.project,
                &key,
                input("Protected fingerprint"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::FingerprintKeyUnavailable)
    ));
    assert_eq!(f.counts().await, (1, 1, 1, 1, 2));
}

#[tokio::test]
async fn legacy_internal_creation_also_persists_outbox_and_deleted_resource_receipts_never_resurrect_it()
 {
    let f = Fixture::new().await;
    IssueStore::new(f.db.pool().clone())
        .create(
            &f.owner,
            &f.project,
            input("Legacy trusted command"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(f.counts().await, (1, 1, 1, 0, 2));
    let key = Uuid::now_v7().to_string();
    let outcome = f
        .writer
        .create(
            &f.session.token,
            &f.project,
            &key,
            input("Receipt tombstone"),
            Utc::now(),
        )
        .await
        .unwrap();
    let receipt = outcome.receipt();
    // Controlled fixture models future purge; no purge product command is enabled.
    sqlx::query("DELETE FROM outbox_events WHERE issue_id = ?")
        .bind(&receipt.issue_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM issue_events WHERE issue_id = ?")
        .bind(&receipt.issue_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM issues WHERE id = ?")
        .bind(&receipt.issue_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        f.writer
            .create(
                &f.session.token,
                &f.project,
                &key,
                input("Receipt tombstone"),
                Utc::now()
            )
            .await,
        Err(SessionIssueCreationError::Unavailable)
    ));
    assert_eq!(f.counts().await, (1, 1, 1, 1, 3));
}
