use std::{num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

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
    // The busy attempt removed its unfinished generation, so the retry starts one.
    assert!(report.snapshot_created);
    assert!(report.generation_started);
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

fn remote_settings(endpoint: &str, bucket: &str) -> replication::RemoteSettings {
    replication::RemoteSettings {
        bucket: bucket.to_owned(),
        prefix: "installations/test".to_owned(),
        region: "us-east-1".to_owned(),
        endpoint: Some(endpoint.to_owned()),
        path_style: true,
        access_key_id: std::env::var("PUXIRI_TEST_REPLICATION_S3_ACCESS_KEY_ID")
            .unwrap_or_else(|_| "test".to_owned()),
        secret_access_key: std::env::var("PUXIRI_TEST_REPLICATION_S3_SECRET_ACCESS_KEY")
            .unwrap_or_else(|_| "test".to_owned()),
    }
}

async fn insert_probe(database: &Database, id: i64, label: &str) {
    let mut write = puxiri::db::admitted_write(database.pool()).await.unwrap();
    if id == 1 {
        sqlx::query("CREATE TABLE replication_probe (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")
            .execute(&mut **write)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO replication_probe (id, label) VALUES (?, ?)")
        .bind(id)
        .bind(label)
        .execute(&mut **write)
        .await
        .unwrap();
    write.commit().await.unwrap();
}

#[tokio::test]
async fn remote_restore_requires_a_configured_destination() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    assert!(replication::status(&database).await.remote.is_none());
    let error = replication::restore_latest_from_remote(&database)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        replication::ReplicationError::RemoteNotConfigured
    ));
}

#[tokio::test]
async fn an_unreachable_destination_keeps_the_local_capture() {
    // Port 1 on loopback refuses the connection without leaving the machine.
    let remote =
        replication::RemoteReplica::new(remote_settings("http://127.0.0.1:1", "unreachable"))
            .unwrap();
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path())
        .await
        .unwrap()
        .with_remote_replica(remote);
    insert_probe(&database, 1, "local").await;

    let report = replication::capture(&database).await.unwrap();
    assert!(report.snapshot_created);
    assert!(matches!(
        report.remote,
        Some(replication::RemoteSync::Failed)
    ));
    let status = replication::status(&database).await;
    assert!(status.snapshot);
    let remote = status.remote.unwrap();
    assert_eq!(remote.location, "s3://unreachable/installations/test");
    assert!(remote.acknowledged_txid.is_none());

    let restored = replication::restore_latest(&database).await.unwrap();
    let path = directory.path().join("restores").join(restored.file_name);
    assert_eq!(labels(&path), vec!["local".to_owned()]);
    assert!(matches!(
        replication::restore_latest_from_remote(&database)
            .await
            .unwrap_err(),
        replication::ReplicationError::Remote(_)
    ));
}

/// Runs only against a disposable S3-compatible server, for example
/// `make test-replication-s3`, which starts a pinned local RustFS.
#[tokio::test]
async fn capture_uploads_to_s3_and_a_new_installation_restores_from_it() {
    let Ok(endpoint) = std::env::var("PUXIRI_TEST_REPLICATION_S3_ENDPOINT") else {
        eprintln!("skipped: PUXIRI_TEST_REPLICATION_S3_ENDPOINT is not set");
        return;
    };
    let bucket = format!("puxiri-replication-{}", uuid::Uuid::now_v7());
    let settings = remote_settings(&endpoint, &bucket);
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(&endpoint)
            .force_path_style(true)
            .credentials_provider(aws_credential_types::Credentials::new(
                settings.access_key_id.clone(),
                settings.secret_access_key.clone(),
                None,
                None,
                "puxiri-test",
            ))
            .build(),
    );
    client.create_bucket().bucket(&bucket).send().await.unwrap();

    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path())
        .await
        .unwrap()
        .with_remote_replica(replication::RemoteReplica::new(settings).unwrap());
    insert_probe(&database, 1, "first").await;
    let first = replication::capture(&database).await.unwrap();
    let Some(replication::RemoteSync::Synced { txid, .. }) = first.remote else {
        panic!("first upload was not confirmed: {:?}", first.remote);
    };
    assert_eq!(txid, first.txid);

    insert_probe(&database, 2, "second").await;
    let second = replication::capture(&database).await.unwrap();
    let Some(replication::RemoteSync::Synced { txid, .. }) = second.remote else {
        panic!("second upload was not confirmed: {:?}", second.remote);
    };
    assert_eq!(txid, second.txid);
    let status = replication::status(&database).await;
    assert_eq!(
        status.remote.unwrap().acknowledged_txid.as_deref(),
        Some(second.txid.as_str())
    );
    insert_probe(&database, 3, "not captured").await;

    // A different data directory stands in for a replacement server.
    let replacement = TempDir::new().unwrap();
    let fresh = Database::open(replacement.path())
        .await
        .unwrap()
        .with_remote_replica(
            replication::RemoteReplica::new(remote_settings(&endpoint, &bucket)).unwrap(),
        );
    let restored = replication::restore_latest_from_remote(&fresh)
        .await
        .unwrap();
    let path = replacement
        .path()
        .join("restores")
        .join(&restored.file_name);
    assert_ne!(path, fresh.database_path());
    assert_eq!(labels(&path), vec!["first".to_owned(), "second".to_owned()]);
}

fn rotating_policy(retained: usize) -> replication::ReplicationPolicy {
    replication::ReplicationPolicy {
        capture_interval: None,
        generation_age: Duration::ZERO,
        retained_generations: NonZeroUsize::new(retained).unwrap(),
    }
}

fn local_generations(root: &Path) -> usize {
    std::fs::read_dir(root.join("replication").join("generations"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .count()
}

#[tokio::test]
async fn a_due_generation_starts_from_a_fresh_snapshot_and_drops_the_previous_one() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path())
        .await
        .unwrap()
        .with_replication_policy(rotating_policy(1));
    for (id, label) in [(1, "first"), (2, "second"), (3, "third")] {
        insert_probe(&database, id, label).await;
        let report = replication::capture(&database).await.unwrap();
        assert!(report.generation_started);
        assert!(report.snapshot_created);
        assert_eq!(local_generations(directory.path()), 1);
    }
    let status = replication::status(&database).await;
    assert!(status.snapshot);
    assert!(status.generation_started_at_millis.is_some());
    let restored = replication::restore_latest(&database).await.unwrap();
    let path = directory.path().join("restores").join(restored.file_name);
    assert_eq!(
        labels(&path),
        vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
    );
}

#[tokio::test]
async fn a_generation_that_is_not_due_keeps_capturing_incrementally() {
    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path()).await.unwrap();
    insert_probe(&database, 1, "first").await;
    assert!(
        replication::capture(&database)
            .await
            .unwrap()
            .generation_started
    );
    insert_probe(&database, 2, "second").await;
    let second = replication::capture(&database).await.unwrap();
    assert!(!second.generation_started);
    assert!(!second.snapshot_created);
    assert_eq!(local_generations(directory.path()), 1);
}

#[tokio::test]
async fn the_scheduler_captures_on_its_interval_and_stops_on_shutdown() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(
        Database::open(directory.path())
            .await
            .unwrap()
            .with_replication_policy(replication::ReplicationPolicy {
                capture_interval: Some(Duration::from_millis(100)),
                ..replication::ReplicationPolicy::default()
            }),
    );
    insert_probe(&database, 1, "scheduled").await;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let scheduler = replication::spawn_scheduler(Arc::clone(&database), shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while replication::status(&database).await.txid.is_none() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the scheduler did not capture");
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), scheduler)
        .await
        .expect("the scheduler did not stop")
        .unwrap();
}

#[tokio::test]
async fn without_an_interval_no_scheduler_runs() {
    let directory = TempDir::new().unwrap();
    let database = Arc::new(
        Database::open(directory.path())
            .await
            .unwrap()
            .with_replication_policy(rotating_policy(1)),
    );
    let (_stop, shutdown) = tokio::sync::watch::channel(false);
    assert!(replication::spawn_scheduler(database, shutdown).is_none());
}

/// Runs only against a disposable S3-compatible server; see `make test-replication-s3`.
#[tokio::test]
async fn remote_retention_keeps_the_newest_generations_and_only_deletes_its_own() {
    let Ok(endpoint) = std::env::var("PUXIRI_TEST_REPLICATION_S3_ENDPOINT") else {
        eprintln!("skipped: PUXIRI_TEST_REPLICATION_S3_ENDPOINT is not set");
        return;
    };
    let bucket = format!("puxiri-retention-{}", uuid::Uuid::now_v7());
    let settings = remote_settings(&endpoint, &bucket);
    let client = test_client(&endpoint, &settings);
    client.create_bucket().bucket(&bucket).send().await.unwrap();
    // Objects Puxiri did not write must survive retention.
    let foreign = [
        "installations/test/generations/not-a-generation/keep.txt",
        "installations/test/notes.txt",
    ];
    for key in foreign {
        client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"keep"))
            .send()
            .await
            .unwrap();
    }

    let directory = TempDir::new().unwrap();
    let database = Database::open(directory.path())
        .await
        .unwrap()
        .with_replication_policy(rotating_policy(2))
        .with_remote_replica(replication::RemoteReplica::new(settings).unwrap());
    for (id, label) in [(1, "first"), (2, "second"), (3, "third")] {
        insert_probe(&database, id, label).await;
        let report = replication::capture(&database).await.unwrap();
        assert!(report.generation_started);
        assert!(matches!(
            report.remote,
            Some(replication::RemoteSync::Synced { .. })
        ));
    }
    let mut generations = std::collections::BTreeSet::new();
    let listed = client
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("installations/test/")
        .send()
        .await
        .unwrap();
    let keys: Vec<String> = listed
        .contents()
        .iter()
        .filter_map(|object| object.key().map(ToOwned::to_owned))
        .collect();
    for key in &keys {
        if let Some(rest) = key.strip_prefix("installations/test/generations/")
            && let Some((generation, _)) = rest.split_once('/')
            && uuid::Uuid::parse_str(generation).is_ok()
        {
            generations.insert(generation.to_owned());
        }
    }
    assert_eq!(generations.len(), 2, "{keys:?}");
    for key in foreign {
        assert!(keys.iter().any(|listed| listed == key), "{key} was deleted");
    }

    let replacement = TempDir::new().unwrap();
    let fresh = Database::open(replacement.path())
        .await
        .unwrap()
        .with_remote_replica(
            replication::RemoteReplica::new(remote_settings(&endpoint, &bucket)).unwrap(),
        );
    let restored = replication::restore_latest_from_remote(&fresh)
        .await
        .unwrap();
    let path = replacement.path().join("restores").join(restored.file_name);
    assert_eq!(
        labels(&path),
        vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
    );
}

fn test_client(endpoint: &str, settings: &replication::RemoteSettings) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .credentials_provider(aws_credential_types::Credentials::new(
                settings.access_key_id.clone(),
                settings.secret_access_key.clone(),
                None,
                None,
                "puxiri-test",
            ))
            .build(),
    )
}
