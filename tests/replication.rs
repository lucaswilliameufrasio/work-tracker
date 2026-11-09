use std::{path::Path, sync::Arc, time::Duration};

use puxiri::{db::Database, replication};
use rusqlite::OpenFlags;
use tempfile::TempDir;

#[tokio::test]
async fn capture_waits_for_an_open_write_and_restore_leaves_the_live_database() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let mut setup = puxiri::db::admitted_write(database.pool()).await.unwrap();
    sqlx::query("CREATE TABLE replication_probe (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")
        .execute(&mut **setup)
        .await
        .unwrap();
    sqlx::query("INSERT INTO replication_probe (id, label) VALUES (1, 'before')")
        .execute(&mut **setup)
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let first = replication::capture(&database).await.unwrap();
    assert!(first.snapshot_created);
    assert!(!first.txid.is_empty());

    let mut held = puxiri::db::admitted_write(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO replication_probe (id, label) VALUES (2, 'during')")
        .execute(&mut **held)
        .await
        .unwrap();
    let pending = {
        let database = Arc::clone(&database);
        tokio::spawn(async move { replication::capture(&database).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !pending.is_finished(),
        "capture resumed while a write was still open"
    );
    held.commit().await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(30), pending)
        .await
        .expect("capture timed out after the write committed")
        .unwrap()
        .unwrap();
    assert!(!second.snapshot_created);

    let restored = replication::restore_latest(&database).await.unwrap();
    assert!(restored.file_name.starts_with("restore-"));
    assert!(restored.file_name.ends_with(".sqlite3"));
    assert!(!restored.file_name.contains('/'));
    assert!(!restored.file_name.contains(".."));
    let restored_path = directory.path().join("restores").join(&restored.file_name);
    assert_ne!(restored_path, database.database_path());

    let mut after = puxiri::db::admitted_write(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO replication_probe (id, label) VALUES (3, 'after')")
        .execute(&mut **after)
        .await
        .unwrap();
    after.commit().await.unwrap();

    let live = labels(database.database_path());
    let recovered = labels(&restored_path);
    assert_eq!(
        live,
        vec!["before".to_owned(), "during".to_owned(), "after".to_owned()]
    );
    assert_eq!(recovered, vec!["before".to_owned(), "during".to_owned()]);
}

#[tokio::test]
async fn a_second_capture_is_busy_until_the_first_gate_is_released() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let permit = database.admission().acquire_exclusive().unwrap();
    let error = replication::capture(&database).await.unwrap_err();
    assert!(matches!(error, replication::ReplicationError::Busy));
    drop(permit);
    let report = replication::capture(&database).await.unwrap();
    // The busy attempt already published the online snapshot. The write
    // gate only stops the incremental capture, so the retry reuses it.
    assert!(!report.snapshot_created);
    assert!(!report.txid.is_empty());
}

#[tokio::test]
async fn dropping_an_uncommitted_write_lets_the_next_capture_run() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let abandoned = puxiri::db::admitted_write(database.pool()).await.unwrap();
    drop(abandoned);
    let report = replication::capture(&database).await.unwrap();
    assert!(report.snapshot_created);
}

#[tokio::test]
async fn status_reports_whether_the_replica_shares_the_database_device() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    let status = replication::status(&database).await;
    assert!(!status.snapshot);
    assert!(status.txid.is_none());
    assert!(status.same_filesystem);

    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let Ok(replica) = TempDir::new_in(shm) else {
        return;
    };
    let fresh = TempDir::new().unwrap();
    let separated = Database::open_with_replication(fresh.path(), replica.path())
        .await
        .unwrap();
    let status = replication::status(&separated).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let database_device = std::fs::metadata(separated.database_path()).unwrap();
        let replica_device = std::fs::metadata(replica.path()).unwrap();
        assert_eq!(
            status.same_filesystem,
            database_device.dev() == replica_device.dev()
        );
    }
}

fn labels(path: &Path) -> Vec<String> {
    let connection =
        rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut statement = connection
        .prepare("SELECT label FROM replication_probe ORDER BY id")
        .unwrap();
    let rows = statement.query_map([], |row| row.get(0)).unwrap();
    let mut labels = Vec::new();
    for row in rows {
        labels.push(row.unwrap());
    }
    labels
}
