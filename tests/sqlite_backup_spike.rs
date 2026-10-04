//! Phase 0 experiment, not a product backup service or an authorized export path.
//! Both libraries must resolve the same native SQLite binding.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use puxiri::db::Database;
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use tempfile::TempDir;
use tokio::sync::oneshot;

async fn source_database() -> (TempDir, Arc<Database>) {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let mut transaction = database.pool().begin().await.unwrap();
    sqlx::query("CREATE TABLE balances (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO balances (id, value) VALUES (1, 0), (2, 0)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE padding (value TEXT NOT NULL)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    for _ in 0..128 {
        sqlx::query("INSERT INTO padding (value) VALUES (?)")
            .bind("x".repeat(4096))
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();
    (directory, database)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_backup_captures_a_consistent_read_snapshot_while_sqlx_commits() {
    let (directory, database) = source_database().await;
    let source_path = directory.path().join("puxiri.sqlite3");
    let destination_path = directory.path().join("snapshot.sqlite3");
    let result_path = destination_path.clone();
    let (snapshot_started, started) = oneshot::channel();
    let (writer_done, writes_done) = oneshot::channel();

    let snapshot = tokio::task::spawn_blocking(move || {
        let source =
            Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        source.busy_timeout(Duration::from_millis(100)).unwrap();
        source.execute_batch("BEGIN;").unwrap();
        // This read fixes the snapshot before later SQLx writes.
        let initial: i64 = source
            .query_row("SELECT value FROM balances WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(initial, 0);
        let mut destination = Connection::open(&destination_path).unwrap();
        let budget = Instant::now();
        {
            let backup = Backup::new(&source, &mut destination).unwrap();
            assert!(matches!(backup.step(1).unwrap(), StepResult::More));
            assert!(backup.progress().remaining > 0);
            snapshot_started.send(()).unwrap();
            // Hold the read snapshot between steps while real concurrent writes commit.
            // A broken non-WAL implementation fails the writer's busy timeout, rather
            // than silently running the two operations serially.
            writes_done.blocking_recv().unwrap();
            loop {
                assert!(
                    budget.elapsed() < Duration::from_secs(10),
                    "backup exceeded its test budget"
                );
                match backup.step(16).unwrap() {
                    StepResult::Done => break,
                    StepResult::More | StepResult::Busy | StepResult::Locked => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    other => panic!("unexpected backup step: {other:?}"),
                }
            }
        }
        source.execute_batch("COMMIT;").unwrap();
        destination.close().unwrap();
        let verification =
            Connection::open_with_flags(destination_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let integrity: String = verification
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let balances: (i64, i64) = verification
            .query_row("SELECT sum(value), max(value) FROM balances", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(balances, (0, 0));
        let copied: i64 = verification
            .query_row("SELECT count(*) FROM padding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copied, 128);
    });

    started.await.unwrap();
    for _ in 0..32 {
        let mut transaction = database.pool().begin().await.unwrap();
        sqlx::query("UPDATE balances SET value = value + 1 WHERE id = 1")
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query("UPDATE balances SET value = value - 1 WHERE id = 2")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }
    let live: i64 = sqlx::query_scalar("SELECT value FROM balances WHERE id = 1")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(live, 32);
    assert!(!snapshot.is_finished());
    writer_done.send(()).unwrap();
    snapshot.await.unwrap();
    assert!(result_path.metadata().unwrap().len() > 0);
    database.close().await;
}

#[tokio::test]
async fn interrupted_online_backup_is_not_a_complete_snapshot_even_if_integrity_is_ok() {
    let (directory, database) = source_database().await;
    let source_path = directory.path().join("puxiri.sqlite3");
    let destination_path = directory.path().join("interrupted.sqlite3");
    tokio::task::spawn_blocking(move || {
        let source =
            Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut destination = Connection::open(destination_path).unwrap();
        {
            let backup = Backup::new(&source, &mut destination).unwrap();
            assert!(matches!(backup.step(1).unwrap(), StepResult::More));
            // Drop before Done: SQLite rolls back the destination backup transaction.
        }
        let integrity: String = destination
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert!(
            destination
                .query_row("SELECT count(*) FROM padding", [], |row| row
                    .get::<_, i64>(0))
                .is_err()
        );
    })
    .await
    .unwrap();
    database.close().await;
}
