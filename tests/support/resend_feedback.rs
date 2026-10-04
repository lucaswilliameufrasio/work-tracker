//! Experimental projection from authenticated Resend inbox. Not product auth,
//! outbox or a public callback. One recipient per send in this fixture.
use puxiri::db::Database;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

pub async fn schema(database: &Database) {
    sqlx::raw_sql("CREATE TABLE feedback_sends(id TEXT PRIMARY KEY,config TEXT NOT NULL,message_id TEXT,recipient TEXT NOT NULL,UNIQUE(config,message_id));
        CREATE TABLE feedback_projection(config TEXT NOT NULL,event_id TEXT NOT NULL,disposition TEXT NOT NULL,PRIMARY KEY(config,event_id));
        CREATE TABLE feedback_suppression(recipient TEXT PRIMARY KEY,reason TEXT NOT NULL CHECK(reason IN ('complaint','permanent_bounce')));
        CREATE TABLE feedback_audit(config TEXT NOT NULL,event_id TEXT NOT NULL,send_id TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(config,event_id));
        CREATE TABLE feedback_pending(id TEXT PRIMARY KEY,recipient TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('ready','dispatch_authorized','suppressed')));")
        .execute(database.pool()).await.unwrap();
}

pub async fn apply(
    transaction: &mut Transaction<'_, Sqlite>,
    config: &str,
    event_id: &str,
    body: &[u8],
) -> Result<(), sqlx::Error> {
    let disposition: Option<String> = sqlx::query_scalar(
        "SELECT disposition FROM feedback_projection WHERE config=? AND event_id=?",
    )
    .bind(config)
    .bind(event_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if disposition.as_deref() == Some("processed") {
        return Ok(());
    }
    let event: Value = serde_json::from_slice(body)
        .expect("only authenticated validated inbox bytes reach fixture projection");
    let message_id = event
        .pointer("/data/email_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let send: Option<(String, String)> =
        sqlx::query_as("SELECT id,recipient FROM feedback_sends WHERE config=? AND message_id=?")
            .bind(config)
            .bind(message_id)
            .fetch_optional(&mut **transaction)
            .await?;
    let Some((send_id, recipient)) = send else {
        return record(transaction, config, event_id, "awaiting_message").await;
    };
    let recipients = event.pointer("/data/to").and_then(Value::as_array);
    if !recipients.is_some_and(|recipients| {
        recipients.len() == 1 && recipients[0].as_str() == Some(recipient.as_str())
    }) {
        return record(transaction, config, event_id, "recipient_mismatch").await;
    }
    let reason = match event.get("type").and_then(Value::as_str) {
        Some("email.complained") => Some("complaint"),
        Some("email.bounced")
            if event.pointer("/data/bounce/type").and_then(Value::as_str) == Some("Permanent")
                && event
                    .pointer("/data/bounce/subType")
                    .and_then(Value::as_str)
                    == Some("General") =>
        {
            Some("permanent_bounce")
        }
        _ => None,
    };
    if let Some(reason) = reason {
        sqlx::query("INSERT INTO feedback_suppression(recipient,reason) VALUES(?,?) ON CONFLICT(recipient) DO UPDATE SET reason=CASE WHEN excluded.reason='complaint' THEN 'complaint' ELSE feedback_suppression.reason END")
            .bind(&recipient).bind(reason).execute(&mut **transaction).await?;
        // Installation-wide address block; changing provider does not bypass it.
        // Already-authorized/in-flight transfers cannot be undone by this SQL.
        sqlx::query(
            "UPDATE feedback_pending SET state='suppressed' WHERE recipient=? AND state='ready'",
        )
        .bind(&recipient)
        .execute(&mut **transaction)
        .await?;
        // Redacted audit: local ids and fixed reason only, no private content.
        sqlx::query("INSERT INTO feedback_audit(config,event_id,send_id,reason) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
            .bind(config).bind(event_id).bind(send_id).bind(reason).execute(&mut **transaction).await?;
    }
    record(transaction, config, event_id, "processed").await
}
async fn record(
    transaction: &mut Transaction<'_, Sqlite>,
    config: &str,
    event_id: &str,
    disposition: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO feedback_projection(config,event_id,disposition) VALUES(?,?,?) ON CONFLICT(config,event_id) DO UPDATE SET disposition=excluded.disposition")
        .bind(config).bind(event_id).bind(disposition).execute(&mut **transaction).await?;
    Ok(())
}

pub async fn reconcile(database: &Database) -> Result<(), sqlx::Error> {
    // Bounded local re-evaluation for callback before send receipt registration.
    // Fixture is one fixed config; a product worker needs pagination/fairness.
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await?;
    let rows:Vec<(String,String,Vec<u8>)>=sqlx::query_as("SELECT endpoint,id,payload FROM webhook_inbox WHERE NOT EXISTS(SELECT 1 FROM feedback_projection p WHERE p.config=webhook_inbox.endpoint AND p.event_id=webhook_inbox.id AND p.disposition='processed') LIMIT 100")
        .fetch_all(&mut *transaction).await?;
    for (config, id, body) in rows {
        apply(&mut transaction, &config, &id, &body).await?;
    }
    transaction.commit().await
}

pub async fn authorize_dispatch(database: &Database, id: &str) -> Result<bool, sqlx::Error> {
    let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT recipient,state FROM feedback_pending WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await?;
    let Some((recipient, state)) = row else {
        return Ok(false);
    };
    if state != "ready" {
        return Ok(false);
    }
    let blocked: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM feedback_suppression WHERE recipient=?)")
            .bind(recipient)
            .fetch_one(&mut *transaction)
            .await?;
    sqlx::query("UPDATE feedback_pending SET state=? WHERE id=?")
        .bind(if blocked {
            "suppressed"
        } else {
            "dispatch_authorized"
        })
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(!blocked)
}
