//! Dev-only projection after configured queue scope/inbox validation.
//! Queue access is the trust boundary; payload metadata is not a signature.
use crate::suppression;
use puxiri::db::Database;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

pub async fn schema(database: &Database) {
    suppression::schema(database).await;
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS cf_feedback_sends(config TEXT NOT NULL,message_id TEXT NOT NULL,recipient TEXT NOT NULL,PRIMARY KEY(config,message_id));
        CREATE TABLE IF NOT EXISTS cf_feedback_projection(config TEXT NOT NULL,event_id TEXT NOT NULL,disposition TEXT NOT NULL,PRIMARY KEY(config,event_id));
        CREATE TABLE IF NOT EXISTS cf_feedback_suppression(recipient TEXT PRIMARY KEY,reason TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cf_feedback_audit(config TEXT NOT NULL,event_id TEXT NOT NULL,message_id TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(config,event_id));")
        .execute(database.pool()).await.unwrap();
}

pub async fn apply(
    transaction: &mut Transaction<'_, Sqlite>,
    config: &str,
    id: &str,
    bytes: &[u8],
) -> Result<(), sqlx::Error> {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT disposition FROM cf_feedback_projection WHERE config=? AND event_id=?",
    )
    .bind(config)
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await?;
    if previous.as_deref() == Some("processed") {
        return Ok(());
    }
    let event: Value = serde_json::from_slice(bytes).expect("validated queue JSON");
    let message_id = event
        .pointer("/payload/messageId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let recipient: Option<String> = sqlx::query_scalar(
        "SELECT recipient FROM cf_feedback_sends WHERE config=? AND message_id=?",
    )
    .bind(config)
    .bind(message_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let disposition = if let Some(recipient) = recipient {
        if event.pointer("/payload/recipient").and_then(Value::as_str) != Some(recipient.as_str()) {
            "recipient_mismatch"
        } else {
            let terminal =
                event.pointer("/payload/terminal").and_then(Value::as_bool) == Some(true);
            let status = event
                .pointer("/payload/delivery/status")
                .and_then(Value::as_str);
            let reason = match event.get("type").and_then(Value::as_str) {
                Some("cf.email.sending.message.complained")
                    if terminal
                        && status == Some("complained")
                        && event
                            .pointer("/payload/complaint/type")
                            .and_then(Value::as_str)
                            == Some("abuse") =>
                {
                    Some("complaint")
                }
                Some("cf.email.sending.message.bounced")
                    if terminal
                        && status == Some("bounced")
                        && event
                            .pointer("/payload/bounce/type")
                            .and_then(Value::as_str)
                            == Some("hard")
                        && event
                            .pointer("/payload/bounce/classification")
                            .and_then(Value::as_str)
                            == Some("permanent_failure")
                        && event
                            .pointer("/payload/delivery/smtpEnhancedStatusCode")
                            .and_then(Value::as_str)
                            == Some("5.1.1") =>
                {
                    Some("permanent_invalid")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                sqlx::query("INSERT INTO cf_feedback_suppression(recipient,reason) VALUES(?,?) ON CONFLICT(recipient) DO UPDATE SET reason=CASE WHEN excluded.reason='complaint' THEN 'complaint' ELSE cf_feedback_suppression.reason END")
                    .bind(&recipient).bind(reason).execute(&mut **transaction).await?;
                sqlx::query("INSERT INTO cf_feedback_audit(config,event_id,message_id,reason) VALUES(?,?,?,?)")
                    .bind(config).bind(id).bind(message_id).bind(reason).execute(&mut **transaction).await?;
                suppression::block(
                    transaction,
                    &recipient,
                    if reason == "complaint" {
                        suppression::Reason::Complaint
                    } else {
                        suppression::Reason::PermanentInvalid
                    },
                )
                .await?;
            }
            "processed"
        }
    } else {
        "awaiting_message"
    };
    sqlx::query("INSERT INTO cf_feedback_projection(config,event_id,disposition) VALUES(?,?,?) ON CONFLICT(config,event_id) DO UPDATE SET disposition=excluded.disposition")
        .bind(config).bind(id).bind(disposition).execute(&mut **transaction).await?;
    Ok(())
}

pub async fn reconcile(database: &Database) -> Result<(), sqlx::Error> {
    // Needed after an already-acked event arrives before send receipt storage.
    // This bounded fixture does not implement fair production scheduling.
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<(String,String,Vec<u8>)> = sqlx::query_as("SELECT config,event_id,event_json FROM cloudflare_inbox i WHERE NOT EXISTS(SELECT 1 FROM cf_feedback_projection p WHERE p.config=i.config AND p.event_id=i.event_id AND p.disposition='processed') LIMIT 100")
        .fetch_all(&mut *transaction).await?;
    for (config, id, bytes) in rows {
        apply(&mut transaction, &config, &id, &bytes).await?;
    }
    transaction.commit().await
}
