//! Dev-only projection. Topic scope comes from authenticated SNS, not Message.
use puxiri::db::Database;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

pub async fn schema(database: &Database) {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS ses_feedback_sends(topic TEXT NOT NULL,ses_id TEXT NOT NULL,recipient TEXT NOT NULL,PRIMARY KEY(topic,ses_id));
        CREATE TABLE IF NOT EXISTS ses_feedback_projection(topic TEXT NOT NULL,sns_id TEXT NOT NULL,disposition TEXT NOT NULL,PRIMARY KEY(topic,sns_id));
        CREATE TABLE IF NOT EXISTS ses_feedback_suppression(recipient TEXT PRIMARY KEY,reason TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS ses_feedback_audit(topic TEXT NOT NULL,feedback_id TEXT NOT NULL,ses_id TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(topic,feedback_id));")
        .execute(database.pool()).await.unwrap();
}
pub async fn apply(
    transaction: &mut Transaction<'_, Sqlite>,
    topic: &str,
    sns_id: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT disposition FROM ses_feedback_projection WHERE topic=? AND sns_id=?",
    )
    .bind(topic)
    .bind(sns_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if previous.as_deref() == Some("processed") {
        return Ok(());
    }
    // Malformed Message is durable/quarantined, not authority to mutate users.
    let Ok(event) = serde_json::from_str::<Value>(message) else {
        return record(transaction, topic, sns_id, "invalid_message").await;
    };
    if event
        .pointer("/mail/sendingAccountId")
        .and_then(Value::as_str)
        != Some("123456789012")
    {
        return record(transaction, topic, sns_id, "account_mismatch").await;
    }
    let ses_id = event
        .pointer("/mail/messageId")
        .and_then(Value::as_str)
        .unwrap_or("");
    if ses_id.is_empty() {
        return record(transaction, topic, sns_id, "missing_message_id").await;
    }
    let recipient: Option<String> =
        sqlx::query_scalar("SELECT recipient FROM ses_feedback_sends WHERE topic=? AND ses_id=?")
            .bind(topic)
            .bind(ses_id)
            .fetch_optional(&mut **transaction)
            .await?;
    let Some(recipient) = recipient else {
        return record(transaction, topic, sns_id, "awaiting_message").await;
    };
    if !event
        .pointer("/mail/destination")
        .and_then(Value::as_array)
        .is_some_and(|values| values.len() == 1 && values[0].as_str() == Some(recipient.as_str()))
    {
        return record(transaction, topic, sns_id, "recipient_mismatch").await;
    }
    let kind = match (
        event.get("notificationType").and_then(Value::as_str),
        event.get("eventType").and_then(Value::as_str),
    ) {
        (Some(a), Some(b)) if a != b => {
            return record(transaction, topic, sns_id, "ambiguous_type").await;
        }
        (Some(value), _) | (_, Some(value)) => value,
        _ => "",
    };
    let (reason, feedback_id) = match kind {
        "Complaint" => {
            let list = event
                .pointer("/complaint/complainedRecipients")
                .and_then(Value::as_array);
            let matching = list.is_some_and(|list| {
                list.len() == 1
                    && list[0].get("emailAddress").and_then(Value::as_str)
                        == Some(recipient.as_str())
            });
            let abuse = matches!(
                event
                    .pointer("/complaint/complaintFeedbackType")
                    .and_then(Value::as_str),
                Some("abuse" | "fraud")
            );
            (
                (matching && abuse).then_some("complaint"),
                event
                    .pointer("/complaint/feedbackId")
                    .and_then(Value::as_str),
            )
        }
        "Bounce" => {
            let list = event
                .pointer("/bounce/bouncedRecipients")
                .and_then(Value::as_array);
            let invalid = list.is_some_and(|list| {
                list.len() == 1
                    && list[0].get("emailAddress").and_then(Value::as_str)
                        == Some(recipient.as_str())
                    && list[0].get("action").and_then(Value::as_str) == Some("failed")
                    && list[0].get("status").and_then(Value::as_str) == Some("5.1.1")
            });
            let permanent = event.pointer("/bounce/bounceType").and_then(Value::as_str)
                == Some("Permanent")
                && event
                    .pointer("/bounce/bounceSubType")
                    .and_then(Value::as_str)
                    == Some("General");
            (
                (invalid && permanent).then_some("permanent_invalid"),
                event.pointer("/bounce/feedbackId").and_then(Value::as_str),
            )
        }
        _ => (None, None),
    };
    if let (Some(reason), Some(feedback_id)) = (
        reason,
        feedback_id.filter(|id| !id.is_empty() && id.len() <= 256),
    ) {
        sqlx::query("INSERT INTO ses_feedback_suppression(recipient,reason) VALUES(?,?) ON CONFLICT(recipient) DO UPDATE SET reason=CASE WHEN excluded.reason='complaint' THEN 'complaint' ELSE ses_feedback_suppression.reason END")
            .bind(recipient).bind(reason).execute(&mut **transaction).await?;
        // Feedback identity is distinct from SNS envelope identity; identical
        // feedback republished in another envelope doesn't duplicate audit.
        sqlx::query("INSERT INTO ses_feedback_audit(topic,feedback_id,ses_id,reason) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
            .bind(topic).bind(feedback_id).bind(ses_id).bind(reason).execute(&mut **transaction).await?;
        let stored: (String, String) = sqlx::query_as(
            "SELECT ses_id,reason FROM ses_feedback_audit WHERE topic=? AND feedback_id=?",
        )
        .bind(topic)
        .bind(feedback_id)
        .fetch_one(&mut **transaction)
        .await?;
        if stored != (ses_id.to_owned(), reason.to_owned()) {
            // Never acknowledge an identity collision or retain its effects.
            // Route returns a fixed 503; poison quarantine policy is pending.
            return Err(sqlx::Error::Protocol(
                "conflicting SES feedback identity".into(),
            ));
        }
    }
    record(transaction, topic, sns_id, "processed").await
}
async fn record(
    transaction: &mut Transaction<'_, Sqlite>,
    topic: &str,
    id: &str,
    disposition: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO ses_feedback_projection(topic,sns_id,disposition) VALUES(?,?,?) ON CONFLICT(topic,sns_id) DO UPDATE SET disposition=excluded.disposition")
        .bind(topic).bind(id).bind(disposition).execute(&mut **transaction).await?;
    Ok(())
}
