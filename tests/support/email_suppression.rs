//! Shared dev-only address policy; caller must authenticate AND correlate first.
//! No production migrations, external send, unblock or address normalization.
use puxiri::db::Database;
use sqlx::{Sqlite, Transaction};

pub enum Reason {
    Complaint,
    PermanentInvalid,
}

pub async fn schema(database: &Database) {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS email_suppression(recipient TEXT PRIMARY KEY,reason TEXT NOT NULL CHECK(reason IN ('complaint','permanent_invalid')));
        CREATE TABLE IF NOT EXISTS feedback_pending(id TEXT PRIMARY KEY,recipient TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('ready','dispatch_authorized','suppressed')));")
        .execute(database.pool()).await.unwrap();
}

pub async fn block(
    transaction: &mut Transaction<'_, Sqlite>,
    recipient: &str,
    reason: Reason,
) -> Result<(), sqlx::Error> {
    let reason = match reason {
        Reason::Complaint => "complaint",
        Reason::PermanentInvalid => "permanent_invalid",
    };
    sqlx::query("INSERT INTO email_suppression(recipient,reason) VALUES(?,?) ON CONFLICT(recipient) DO UPDATE SET reason=CASE WHEN excluded.reason='complaint' THEN 'complaint' ELSE email_suppression.reason END")
        .bind(recipient).bind(reason).execute(&mut **transaction).await?;
    // Already-authorized transfers may be in flight and cannot be undone here.
    sqlx::query(
        "UPDATE feedback_pending SET state='suppressed' WHERE recipient=? AND state='ready'",
    )
    .bind(recipient)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
