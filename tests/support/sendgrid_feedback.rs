//! Dev-only effects after ECDSA authentication. Correlation uses an opaque local
//! attempt token, never inferred X-Message-ID/sg_message_id prefix equivalence.
use puxiri::db::Database;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
pub async fn schema(database: &Database) {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS sg_feedback_sends(config TEXT NOT NULL,attempt TEXT NOT NULL,recipient TEXT NOT NULL,PRIMARY KEY(config,attempt));
        CREATE TABLE IF NOT EXISTS sg_feedback_projection(config TEXT NOT NULL,event_id TEXT NOT NULL,disposition TEXT NOT NULL,PRIMARY KEY(config,event_id));
        CREATE TABLE IF NOT EXISTS sg_feedback_suppression(recipient TEXT PRIMARY KEY,reason TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS sg_feedback_audit(config TEXT NOT NULL,event_id TEXT NOT NULL,attempt TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(config,event_id));")
        .execute(database.pool()).await.unwrap();
}
pub async fn apply(
    transaction: &mut Transaction<'_, Sqlite>,
    config: &str,
    id: &str,
    json: &[u8],
) -> Result<(), sqlx::Error> {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT disposition FROM sg_feedback_projection WHERE config=? AND event_id=?",
    )
    .bind(config)
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await?;
    if previous.as_deref() == Some("processed") {
        return Ok(());
    }
    let event: Value = serde_json::from_slice(json).expect("validated authenticated event JSON");
    // SendGrid flattens v3 custom_args onto the event, per official examples.
    let token = event
        .get("puxiri_attempt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let recipient: Option<String> =
        sqlx::query_scalar("SELECT recipient FROM sg_feedback_sends WHERE config=? AND attempt=?")
            .bind(config)
            .bind(token)
            .fetch_optional(&mut **transaction)
            .await?;
    let disposition = if let Some(recipient) = recipient {
        if event.get("email").and_then(Value::as_str) != Some(recipient.as_str()) {
            "recipient_mismatch"
        } else {
            let reason = match event.get("event").and_then(Value::as_str) {
                Some("spamreport") => Some("complaint"),
                Some("bounce")
                    if event.get("type").and_then(Value::as_str) == Some("bounce")
                        && event.get("bounce_classification").and_then(Value::as_str)
                            == Some("Invalid Address") =>
                {
                    Some("permanent_invalid")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                sqlx::query("INSERT INTO sg_feedback_suppression(recipient,reason) VALUES(?,?) ON CONFLICT(recipient) DO UPDATE SET reason=CASE WHEN excluded.reason='complaint' THEN 'complaint' ELSE sg_feedback_suppression.reason END")
                    .bind(recipient).bind(reason).execute(&mut **transaction).await?;
                sqlx::query("INSERT INTO sg_feedback_audit(config,event_id,attempt,reason) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
                    .bind(config).bind(id).bind(token).bind(reason).execute(&mut **transaction).await?;
            }
            "processed"
        }
    } else {
        "awaiting_attempt"
    };
    sqlx::query("INSERT INTO sg_feedback_projection(config,event_id,disposition) VALUES(?,?,?) ON CONFLICT(config,event_id) DO UPDATE SET disposition=excluded.disposition")
        .bind(config).bind(id).bind(disposition).execute(&mut **transaction).await?;
    Ok(())
}
