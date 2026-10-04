//! Phase 0 protocol experiment: real SQLite and local files, not an S3 adapter,
//! encrypted bundle, retention policy, or product backup implementation.
//! The fixture schema is deliberately outside production migrations.

use std::{fs, path::PathBuf, sync::Arc, time::Duration};

use puxiri::db::Database;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, backup::Backup};
use tempfile::TempDir;
use tokio::{sync::oneshot, task::JoinHandle};

struct Fixture {
    directory: TempDir,
    database: Arc<Database>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        fs::create_dir(directory.path().join("objects")).unwrap();
        fs::create_dir(directory.path().join("staging")).unwrap();
        let database = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::raw_sql(
            "CREATE TABLE objects (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK (state IN ('available', 'deleting', 'deleted'))
             );
             CREATE TABLE object_references (
                owner TEXT PRIMARY KEY,
                object_id TEXT NOT NULL REFERENCES objects(id)
             );
             CREATE TABLE backup_runs (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK (state IN ('copying', 'complete', 'incomplete', 'cancelled'))
             );
             CREATE TABLE backup_pins (
                run_id TEXT NOT NULL REFERENCES backup_runs(id),
                object_id TEXT NOT NULL REFERENCES objects(id),
                PRIMARY KEY (run_id, object_id)
             );",
        )
        .execute(database.pool())
        .await
        .unwrap();
        for (id, owner, body) in [
            ("image", "issue", "original issue image"),
            ("avatar", "profile", "original avatar"),
            ("orphan", "", "unreferenced file"),
        ] {
            fs::write(directory.path().join("objects").join(id), body).unwrap();
            sqlx::query("INSERT INTO objects (id, state) VALUES (?, 'available')")
                .bind(id)
                .execute(database.pool())
                .await
                .unwrap();
            if !owner.is_empty() {
                assert!(Self::reference(&database, owner, id).await);
            }
        }
        Self {
            directory,
            database,
        }
    }

    async fn reference(database: &Database, owner: &str, id: &str) -> bool {
        let mut transaction = database.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let changed = sqlx::query(
            "INSERT INTO object_references (owner, object_id)
             SELECT ?, id FROM objects WHERE id = ? AND state = 'available'",
        )
        .bind(owner)
        .bind(id)
        .execute(&mut *transaction)
        .await
        .unwrap()
        .rows_affected();
        transaction.commit().await.unwrap();
        changed == 1
    }

    async fn remove_references(&self) {
        sqlx::query("DELETE FROM object_references")
            .execute(self.database.pool())
            .await
            .unwrap();
    }

    // Claim in a writer transaction before deleting bytes outside the database.
    // Object references and GC use the same serialization boundary.
    async fn claim_for_gc(&self, id: &str) -> bool {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap();
        let changed = sqlx::query(
            "UPDATE objects SET state = 'deleting'
             WHERE id = ? AND state = 'available'
             AND NOT EXISTS (SELECT 1 FROM object_references WHERE object_id = objects.id)
             AND NOT EXISTS (SELECT 1 FROM backup_pins WHERE object_id = objects.id)",
        )
        .bind(id)
        .execute(&mut *transaction)
        .await
        .unwrap()
        .rows_affected();
        transaction.commit().await.unwrap();
        changed == 1
    }

    async fn delete_claimed(&self, id: &str) {
        let state: String = sqlx::query_scalar("SELECT state FROM objects WHERE id = ?")
            .bind(id)
            .fetch_one(self.database.pool())
            .await
            .unwrap();
        assert_eq!(state, "deleting");
        fs::remove_file(self.directory.path().join("objects").join(id)).unwrap();
        sqlx::query("UPDATE objects SET state = 'deleted' WHERE id = ? AND state = 'deleting'")
            .bind(id)
            .execute(self.database.pool())
            .await
            .unwrap();
    }

    async fn terminal(&self, id: &str, state: &str) {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap();
        let changed =
            sqlx::query("UPDATE backup_runs SET state = ? WHERE id = ? AND state = 'copying'")
                .bind(state)
                .bind(id)
                .execute(&mut *transaction)
                .await
                .unwrap()
                .rows_affected();
        assert_eq!(changed, 1);
        sqlx::query("DELETE FROM backup_pins WHERE run_id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }

    async fn pin_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM backup_pins")
            .fetch_one(self.database.pool())
            .await
            .unwrap()
    }
}

#[derive(Debug, PartialEq)]
enum CopyResult {
    Copied,
    Missing(Vec<String>),
    Cancelled,
}

struct Frozen {
    inventory: Vec<String>,
    seal: oneshot::Sender<()>,
    sealed: oneshot::Receiver<()>,
    copy: oneshot::Sender<()>,
    worker: JoinHandle<CopyResult>,
}

async fn freeze(fixture: &Fixture, id: &str) -> Frozen {
    let root = fixture.directory.path().to_path_buf();
    let id = id.to_owned();
    let (inventory_tx, inventory_rx) = oneshot::channel();
    let (seal, seal_rx) = oneshot::channel();
    let (sealed_tx, sealed) = oneshot::channel();
    let (copy, copy_rx) = oneshot::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let path = root.join("puxiri.sqlite3");
        let mut guard = Connection::open(&path).unwrap();
        guard.busy_timeout(Duration::from_secs(5)).unwrap();
        guard.pragma_update(None, "foreign_keys", true).unwrap();
        let transaction = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let source = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        source.execute_batch("BEGIN;").unwrap();
        let inventory: Vec<String> = {
            let mut statement = source
                .prepare(
                    "SELECT DISTINCT objects.id FROM objects
                 JOIN object_references ON object_references.object_id = objects.id
                 WHERE objects.state = 'available' ORDER BY objects.id",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        // The first read fixes the snapshot while BEGIN IMMEDIATE excludes GC.
        // Keep that *same* read transaction alive until Online Backup is Done.
        inventory_tx.send(inventory.clone()).unwrap();
        if seal_rx.blocking_recv().is_err() {
            return CopyResult::Cancelled;
        }
        transaction
            .execute(
                "INSERT INTO backup_runs (id, state) VALUES (?, 'copying')",
                [&id],
            )
            .unwrap();
        for object in &inventory {
            transaction
                .execute(
                    "INSERT INTO backup_pins (run_id, object_id) VALUES (?, ?)",
                    [&id, object],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        sealed_tx.send(()).unwrap();
        if copy_rx.blocking_recv().is_err() {
            // Durable pins intentionally survive an interrupted worker. Recovery
            // must make the run terminal before releasing them, not guess by TTL.
            return CopyResult::Cancelled;
        }
        let staging = root.join("staging").join(id);
        fs::create_dir(&staging).unwrap();
        let mut destination = Connection::open(staging.join("database.sqlite3")).unwrap();
        {
            let backup = Backup::new(&source, &mut destination).unwrap();
            backup
                .run_to_completion(32, Duration::from_millis(1), None)
                .unwrap();
        }
        destination.close().unwrap();
        source.execute_batch("COMMIT;").unwrap();
        let mut missing = Vec::new();
        for object in &inventory {
            let path = root.join("objects").join(object);
            match fs::File::open(path) {
                Ok(mut input) => {
                    let mut output = fs::File::create(staging.join(object)).unwrap();
                    std::io::copy(&mut input, &mut output).unwrap();
                    output.sync_all().unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(object.clone())
                }
                Err(error) => panic!("unexpected fixture IO failure: {error}"),
            }
        }
        if missing.is_empty() {
            CopyResult::Copied
        } else {
            CopyResult::Missing(missing)
        }
    });
    Frozen {
        inventory: inventory_rx.await.unwrap(),
        seal,
        sealed,
        copy,
        worker,
    }
}

async fn seal(frozen: Frozen) -> (oneshot::Sender<()>, JoinHandle<CopyResult>) {
    frozen.seal.send(()).unwrap();
    frozen.sealed.await.unwrap();
    (frozen.copy, frozen.worker)
}

async fn inspect_snapshot(path: PathBuf) -> Vec<String> {
    tokio::task::spawn_blocking(move || {
        let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let integrity: String = database
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let mut statement = database
            .prepare("SELECT object_id FROM object_references ORDER BY object_id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_serializes_pins_with_gc_and_uses_the_same_snapshot_after_refs_change() {
    let fixture = Fixture::new().await;
    let frozen = freeze(&fixture, "run").await;
    assert_eq!(frozen.inventory, ["avatar", "image"]);

    // Force the dangerous interval: read snapshot exists but pins not committed.
    // A separate real SQLx connection must not delete refs or claim objects here.
    let mut contender = fixture.database.pool().acquire().await.unwrap();
    sqlx::query("PRAGMA busy_timeout = 50")
        .execute(&mut *contender)
        .await
        .unwrap();
    let error = sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *contender)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("5")
    );
    sqlx::query("PRAGMA busy_timeout = 5000")
        .execute(&mut *contender)
        .await
        .unwrap();
    drop(contender);

    let (copy, worker) = seal(frozen).await;
    fixture.remove_references().await;
    assert_eq!(fixture.pin_count().await, 2);
    assert!(!fixture.claim_for_gc("image").await);
    assert!(!fixture.claim_for_gc("avatar").await);
    assert!(fixture.claim_for_gc("orphan").await);
    fixture.delete_claimed("orphan").await;
    copy.send(()).unwrap();
    assert_eq!(worker.await.unwrap(), CopyResult::Copied);
    let staging = fixture.directory.path().join("staging/run");
    assert_eq!(
        inspect_snapshot(staging.join("database.sqlite3")).await,
        ["avatar", "image"]
    );
    assert_eq!(
        fs::read_to_string(staging.join("image")).unwrap(),
        "original issue image"
    );
    assert_eq!(
        fs::read_to_string(staging.join("avatar")).unwrap(),
        "original avatar"
    );
    assert!(!staging.join("orphan").exists());
    fixture.terminal("run", "complete").await;
    assert_eq!(fixture.pin_count().await, 0);
    assert!(fixture.claim_for_gc("image").await);
    fixture.delete_claimed("image").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_snapshots_keep_objects_pinned_until_both_runs_are_terminal() {
    let fixture = Fixture::new().await;
    let (copy_a, worker_a) = seal(freeze(&fixture, "a").await).await;
    let (copy_b, worker_b) = seal(freeze(&fixture, "b").await).await;
    fixture.remove_references().await;
    assert_eq!(fixture.pin_count().await, 4);
    drop(copy_a);
    assert_eq!(worker_a.await.unwrap(), CopyResult::Cancelled);
    fixture.terminal("a", "cancelled").await;
    assert_eq!(fixture.pin_count().await, 2);
    assert!(!fixture.claim_for_gc("image").await);
    copy_b.send(()).unwrap();
    assert_eq!(worker_b.await.unwrap(), CopyResult::Copied);
    fixture.terminal("b", "complete").await;
    assert!(fixture.claim_for_gc("image").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_worker_leaves_durable_pins_until_explicit_recovery_after_reopen() {
    let fixture = Fixture::new().await;
    let (copy, worker) = seal(freeze(&fixture, "interrupted").await).await;
    fixture.remove_references().await;
    drop(copy);
    assert_eq!(worker.await.unwrap(), CopyResult::Cancelled);
    fixture.database.close().await;
    let Fixture {
        directory,
        database,
    } = fixture;
    drop(database);
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let fixture = Fixture {
        directory,
        database,
    };
    assert_eq!(fixture.pin_count().await, 2);
    assert!(!fixture.claim_for_gc("avatar").await);
    fixture.terminal("interrupted", "incomplete").await;
    assert_eq!(fixture.pin_count().await, 0);
    assert!(fixture.claim_for_gc("avatar").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_pinned_bytes_leave_an_incomplete_staging_snapshot_not_success() {
    let fixture = Fixture::new().await;
    let (copy, worker) = seal(freeze(&fixture, "missing").await).await;
    // External deletion is outside the application's GC safety boundary.
    fs::remove_file(fixture.directory.path().join("objects/image")).unwrap();
    copy.send(()).unwrap();
    assert_eq!(
        worker.await.unwrap(),
        CopyResult::Missing(vec!["image".into()])
    );
    let staging = fixture.directory.path().join("staging/missing");
    assert_eq!(
        inspect_snapshot(staging.join("database.sqlite3")).await,
        ["avatar", "image"]
    );
    assert!(!staging.join("image").exists());
    assert!(staging.join("avatar").exists());
    fixture.terminal("missing", "incomplete").await;
    let state: String = sqlx::query_scalar("SELECT state FROM backup_runs WHERE id = 'missing'")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(state, "incomplete");
    assert_eq!(fixture.pin_count().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_claim_rejects_new_references_and_is_not_in_the_next_inventory() {
    let fixture = Fixture::new().await;
    fixture.remove_references().await;
    assert!(fixture.claim_for_gc("image").await);
    assert!(!Fixture::reference(&fixture.database, "new-issue", "image").await);
    let frozen = freeze(&fixture, "after-gc").await;
    assert!(frozen.inventory.is_empty());
    let (copy, worker) = seal(frozen).await;
    fixture.delete_claimed("image").await;
    copy.send(()).unwrap();
    assert_eq!(worker.await.unwrap(), CopyResult::Copied);
    assert!(
        inspect_snapshot(
            fixture
                .directory
                .path()
                .join("staging/after-gc/database.sqlite3")
        )
        .await
        .is_empty()
    );
    fixture.terminal("after-gc", "complete").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_sealing_rolls_back_without_leaving_pins_or_writer_lock() {
    let fixture = Fixture::new().await;
    let frozen = freeze(&fixture, "unsealed").await;
    drop(frozen.seal);
    drop(frozen.copy);
    assert_eq!(frozen.worker.await.unwrap(), CopyResult::Cancelled);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_runs")
        .fetch_one(fixture.database.pool())
        .await
        .unwrap();
    assert_eq!(runs, 0);
    assert_eq!(fixture.pin_count().await, 0);
    fixture.remove_references().await;
    assert!(fixture.claim_for_gc("image").await);
    assert!(!fixture.directory.path().join("staging/unsealed").exists());
}
