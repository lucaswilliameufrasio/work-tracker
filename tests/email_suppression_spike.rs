//! Real SQLite cross-provider projection contract. Authentication is covered by
//! each HTTP ingress suite; these inputs represent already-authenticated events.
#[path = "support/cloudflare_feedback.rs"]
mod cloudflare;
#[path = "support/resend_feedback.rs"]
mod resend;
#[path = "support/sendgrid_feedback.rs"]
mod sendgrid;
#[path = "support/ses_feedback.rs"]
mod ses;
#[path = "support/email_suppression.rs"]
mod suppression;
use puxiri::db::Database;
use serde_json::json;
use tempfile::TempDir;

const ADDRESS: &str = "recipient@example.invalid";
async fn setup() -> (TempDir, Database) {
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    resend::schema(&db).await;
    sendgrid::schema(&db).await;
    ses::schema(&db).await;
    cloudflare::schema(&db).await;
    sqlx::raw_sql(
        "CREATE TABLE webhook_inbox(endpoint TEXT,id TEXT,payload BLOB);
        CREATE TABLE cloudflare_inbox(config TEXT,event_id TEXT,event_json BLOB);
        CREATE TABLE fixture_inbox(id TEXT PRIMARY KEY);
        INSERT INTO feedback_sends VALUES('send','config','message','recipient@example.invalid');
        INSERT INTO sg_feedback_sends VALUES('config','attempt','recipient@example.invalid');
        INSERT INTO ses_feedback_sends VALUES('config','message','recipient@example.invalid');
        INSERT INTO cf_feedback_sends VALUES('config','message','recipient@example.invalid');",
    )
    .execute(db.pool())
    .await
    .unwrap();
    resend::reconcile(&db).await.unwrap();
    cloudflare::reconcile(&db).await.unwrap();
    (directory, db)
}
async fn pending(db: &Database, id: &str, address: &str) {
    sqlx::query("INSERT INTO feedback_pending VALUES(?,?,'ready')")
        .bind(id)
        .bind(address)
        .execute(db.pool())
        .await
        .unwrap();
}
async fn project(
    db: &Database,
    provider: u8,
    id: &str,
    complaint: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = db.pool().begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT INTO fixture_inbox VALUES(?) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    match provider {
        0 => resend::apply(&mut tx,"config",id,&serde_json::to_vec(&json!({"type":if complaint {"email.complained"} else {"email.bounced"},"data":{"email_id":"message","to":[ADDRESS],"bounce":{"type":"Permanent","subType":"General"}}})).unwrap()).await?,
        1 => sendgrid::apply(&mut tx,"config",id,&serde_json::to_vec(&json!({"event":if complaint {"spamreport"} else {"bounce"},"puxiri_attempt":"attempt","email":ADDRESS,"type":"bounce","bounce_classification":"Invalid Address"})).unwrap()).await?,
        2 => ses::apply(&mut tx,"config",id,&json!({"notificationType":if complaint {"Complaint"} else {"Bounce"},"mail":{"sendingAccountId":"123456789012","messageId":"message","destination":[ADDRESS]},"complaint":{"feedbackId":id,"complaintFeedbackType":"abuse","complainedRecipients":[{"emailAddress":ADDRESS}]},"bounce":{"feedbackId":id,"bounceType":"Permanent","bounceSubType":"General","bouncedRecipients":[{"emailAddress":ADDRESS,"action":"failed","status":"5.1.1"}]}}).to_string()).await?,
        3 => cloudflare::apply(&mut tx,"config",id,&serde_json::to_vec(&json!({"type":if complaint {"cf.email.sending.message.complained"} else {"cf.email.sending.message.bounced"},"payload":{"messageId":"message","recipient":ADDRESS,"terminal":true,"delivery":{"status":if complaint {"complained"} else {"bounced"},"smtpEnhancedStatusCode":"5.1.1"},"complaint":{"type":"abuse"},"bounce":{"type":"hard","classification":"permanent_failure"}}})).unwrap()).await?,
        _ => unreachable!(),
    }
    tx.commit().await
}

#[tokio::test]
async fn every_provider_blocks_new_dispatch_after_switch_and_preserves_other_addresses() {
    for provider in 0..4 {
        let (_directory, db) = setup().await;
        pending(&db, "before", ADDRESS).await;
        project(&db, provider, "block", false).await.unwrap();
        assert!(!resend::authorize_dispatch(&db, "before").await.unwrap());
        // New job on any provider uses the same installation/address gate.
        pending(&db, "switched-provider", ADDRESS).await;
        assert!(
            !resend::authorize_dispatch(&db, "switched-provider")
                .await
                .unwrap()
        );
        pending(&db, "different", "new-address@example.invalid").await;
        assert!(resend::authorize_dispatch(&db, "different").await.unwrap());
        let reason: String = sqlx::query_scalar("SELECT reason FROM email_suppression")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(reason, "permanent_invalid");
        db.close().await;
    }
}

#[tokio::test]
async fn concurrent_providers_converge_and_complaint_precedence_survives_reopen() {
    let (directory, db) = setup().await;
    let (a, b, c, d) = tokio::join!(
        project(&db, 0, "r", false),
        project(&db, 1, "sg", true),
        project(&db, 2, "ses", false),
        project(&db, 3, "cf", false)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    d.unwrap();
    project(&db, 0, "r", false).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM email_suppression")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT reason FROM email_suppression")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "complaint"
    );
    db.close().await;
    drop(db);
    let reopened = Database::open(directory.path()).await.unwrap();
    pending(&reopened, "after-restart", ADDRESS).await;
    assert!(
        !resend::authorize_dispatch(&reopened, "after-restart")
            .await
            .unwrap()
    );
    reopened.close().await;
}

#[tokio::test]
async fn shared_policy_failure_rolls_back_provider_audit_inbox_and_pending_for_all_providers() {
    for (provider, audit) in [
        (0, "SELECT count(*) FROM feedback_audit"),
        (1, "SELECT count(*) FROM sg_feedback_audit"),
        (2, "SELECT count(*) FROM ses_feedback_audit"),
        (3, "SELECT count(*) FROM cf_feedback_audit"),
    ] {
        let (_directory, db) = setup().await;
        pending(&db, "pending", ADDRESS).await;
        sqlx::raw_sql("CREATE TRIGGER fail_policy BEFORE UPDATE ON feedback_pending BEGIN SELECT RAISE(ABORT,'shared policy failure'); END")
            .execute(db.pool()).await.unwrap();
        let error = project(&db, provider, "failed", true).await.unwrap_err();
        assert!(error.to_string().contains("shared policy failure"));
        for query in [
            "SELECT count(*) FROM email_suppression",
            "SELECT count(*) FROM fixture_inbox",
            audit,
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(query)
                    .fetch_one(db.pool())
                    .await
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM feedback_pending")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "ready"
        );
        sqlx::query("DROP TRIGGER fail_policy")
            .execute(db.pool())
            .await
            .unwrap();
        project(&db, provider, "failed", true).await.unwrap();
        assert!(!resend::authorize_dispatch(&db, "pending").await.unwrap());
        db.close().await;
    }
}

#[tokio::test]
async fn racing_dispatch_and_feedback_serializes_without_claiming_remote_cancellation() {
    let (_directory, db) = setup().await;
    pending(&db, "racing", ADDRESS).await;
    let (authorized, effect) = tokio::join!(
        resend::authorize_dispatch(&db, "racing"),
        project(&db, 3, "complaint", true)
    );
    effect.unwrap();
    let authorized = authorized.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM feedback_pending WHERE id='racing'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        state,
        if authorized {
            "dispatch_authorized"
        } else {
            "suppressed"
        }
    );
    pending(&db, "future", ADDRESS).await;
    assert!(!resend::authorize_dispatch(&db, "future").await.unwrap());
    assert!(!resend::authorize_dispatch(&db, "missing").await.unwrap());
    db.close().await;
}
