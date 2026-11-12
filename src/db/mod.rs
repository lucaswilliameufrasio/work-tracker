use std::{
    collections::HashMap,
    fs::{DirBuilder, File, OpenOptions, TryLockError},
    io,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

use sqlx::{
    ConnectOptions, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use thiserror::Error;
use tokio::sync::Notify;

#[derive(Debug, Error)]
pub enum OpenError {
    #[error("another Puxiri process already owns this data directory")]
    AlreadyRunning,
    #[error("could not prepare the data directory: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("could not open the database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("could not apply database migrations: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// The lock lives as long as the application state, including router clones.
pub struct Database {
    pool: SqlitePool,
    data_dir: PathBuf,
    database_file: PathBuf,
    replication_dir: PathBuf,
    remote_replica: Option<Arc<crate::replication::RemoteReplica>>,
    replication_policy: crate::replication::ReplicationPolicy,
    replication_lock: tokio::sync::Mutex<()>,
    admission: Arc<WriteAdmission>,
    _directory_lock: File,
}

impl Drop for Database {
    fn drop(&mut self) {
        // Explicit unlock also releases this open file description's lock
        // when a forked child's inherited descriptor has not reached exec yet.
        // Merely closing our descriptor waits for all duplicates to close.
        let _ = self._directory_lock.unlock();
    }
}

impl Database {
    pub async fn open(data_dir: &Path) -> Result<Self, OpenError> {
        Self::open_with_replication(data_dir, &data_dir.join("replication")).await
    }

    pub async fn open_with_replication(
        data_dir: &Path,
        replication_dir: &Path,
    ) -> Result<Self, OpenError> {
        let directory_lock = lock_directory(data_dir)?;
        let options = SqliteConnectOptions::new()
            .filename(data_dir.join("puxiri.sqlite3"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        if let Err(error) = sqlx::migrate!("./migrations").run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        let file: String =
            match sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
                .fetch_one(&pool)
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    pool.close().await;
                    return Err(error.into());
                }
            };
        let admission = Arc::new(WriteAdmission::new());
        register_admission(file.clone(), Arc::clone(&admission));
        if let Err(error) =
            crate::jobs::JobStore::recover_abandoned_on_startup(&pool, chrono::Utc::now()).await
        {
            unregister_admission(&file);
            pool.close().await;
            return Err(error.into());
        }
        if let Err(error) = create_private_directory(replication_dir) {
            unregister_admission(&file);
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self {
            pool,
            data_dir: data_dir.to_path_buf(),
            database_file: PathBuf::from(file),
            replication_dir: replication_dir.to_path_buf(),
            remote_replica: None,
            replication_policy: crate::replication::ReplicationPolicy::default(),
            replication_lock: tokio::sync::Mutex::new(()),
            admission,
            _directory_lock: directory_lock,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn database_path(&self) -> &Path {
        &self.database_file
    }

    pub fn replication_dir(&self) -> &Path {
        &self.replication_dir
    }

    /// Uploads each capture to this S3-compatible destination as well.
    pub fn with_remote_replica(mut self, remote: crate::replication::RemoteReplica) -> Self {
        self.remote_replica = Some(Arc::new(remote));
        self
    }

    pub fn remote_replica(&self) -> Option<Arc<crate::replication::RemoteReplica>> {
        self.remote_replica.clone()
    }

    /// Scheduled capture and generation retention for this installation.
    pub fn with_replication_policy(
        mut self,
        policy: crate::replication::ReplicationPolicy,
    ) -> Self {
        self.replication_policy = policy;
        self
    }

    pub fn replication_policy(&self) -> crate::replication::ReplicationPolicy {
        self.replication_policy
    }

    /// Held by one capture or restore at a time; the others report busy.
    pub(crate) fn replication_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.replication_lock
    }

    pub fn admission(&self) -> Arc<WriteAdmission> {
        Arc::clone(&self.admission)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut directory = DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(path)
}

fn lock_directory(data_dir: &Path) -> Result<File, OpenError> {
    let mut directory = DirBuilder::new();
    directory.recursive(true);
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        directory.mode(0o700);
        lock_options.mode(0o600);
    }
    directory.create(data_dir)?;
    let lock = lock_options.open(data_dir.join("puxiri.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(OpenError::AlreadyRunning),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn admission_registry() -> &'static Mutex<HashMap<String, Arc<WriteAdmission>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<WriteAdmission>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_admission(database_file: String, admission: Arc<WriteAdmission>) {
    let mut registry = admission_registry()
        .lock()
        .expect("write admission registry");
    registry.insert(database_file, admission);
}

fn lookup_admission(database_file: &str) -> Option<Arc<WriteAdmission>> {
    let registry = admission_registry()
        .lock()
        .expect("write admission registry");
    registry.get(database_file).map(Arc::clone)
}

fn unregister_admission(database_file: &str) {
    let mut registry = admission_registry()
        .lock()
        .expect("write admission registry");
    registry.remove(database_file);
}

struct GateState {
    closed: bool,
    inflight: usize,
}

/// Stops application writers before `sqlite-replication` captures the WAL.
pub struct WriteAdmission {
    state: Mutex<GateState>,
    drained: Condvar,
    resumed: Notify,
}

impl WriteAdmission {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                closed: false,
                inflight: 0,
            }),
            drained: Condvar::new(),
            resumed: Notify::new(),
        }
    }

    async fn enter_writer(self: &Arc<Self>) -> WriterPermit {
        loop {
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            {
                let mut state = self.state.lock().expect("write admission");
                if !state.closed {
                    state.inflight += 1;
                    return WriterPermit {
                        admission: Arc::clone(self),
                    };
                }
            }
            resumed.await;
        }
    }

    pub fn acquire_exclusive(self: &Arc<Self>) -> io::Result<ExclusivePermit> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("write admission lock is poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another capture already holds the write gate",
            ));
        }
        state.closed = true;
        while state.inflight > 0 {
            state = self
                .drained
                .wait(state)
                .map_err(|_| io::Error::other("write admission lock is poisoned"))?;
        }
        Ok(ExclusivePermit {
            admission: Arc::clone(self),
            open: true,
        })
    }

    fn resume(&self) {
        let mut state = self.state.lock().expect("write admission");
        state.closed = false;
        drop(state);
        self.resumed.notify_waiters();
    }
}

struct WriterPermit {
    admission: Arc<WriteAdmission>,
}

impl Drop for WriterPermit {
    fn drop(&mut self) {
        let mut state = self.admission.state.lock().expect("write admission");
        state.inflight = state.inflight.saturating_sub(1);
        self.admission.drained.notify_all();
    }
}

pub struct ExclusivePermit {
    admission: Arc<WriteAdmission>,
    open: bool,
}

impl ExclusivePermit {
    fn open_gate(&mut self) {
        if self.open {
            self.open = false;
            self.admission.resume();
        }
    }
}

impl Drop for ExclusivePermit {
    fn drop(&mut self) {
        self.open_gate();
    }
}

/// A write transaction that counts against the replication gate.
///
/// Drop rolls the SQLite transaction back before the permit resumes writers.
/// `commit` and `rollback` finish that SQLite call before releasing the gate.
pub struct WriteTransaction<'c> {
    inner: Option<Transaction<'c, Sqlite>>,
    permit: Option<WriterPermit>,
}

impl<'c> WriteTransaction<'c> {
    pub async fn commit(mut self) -> Result<(), sqlx::Error> {
        let inner = self.inner.take().expect("write transaction");
        let result = inner.commit().await;
        drop(self.permit.take());
        result
    }

    pub async fn rollback(mut self) -> Result<(), sqlx::Error> {
        let inner = self.inner.take().expect("write transaction");
        let result = inner.rollback().await;
        drop(self.permit.take());
        result
    }
}

impl<'c> Drop for WriteTransaction<'c> {
    fn drop(&mut self) {
        drop(self.inner.take());
        drop(self.permit.take());
    }
}

impl<'c> Deref for WriteTransaction<'c> {
    type Target = Transaction<'c, Sqlite>;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("write transaction")
    }
}

impl<'c> DerefMut for WriteTransaction<'c> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("write transaction")
    }
}

/// Attaches a write gate to a pool that [`Database::open`] did not open.
///
/// Partial-schema migration tests call product stores after applying only a
/// prefix of the migrations. Production capture keeps using the gate that
/// `Database` already registered for the same file.
pub async fn ensure_write_gate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let file: String =
        sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .fetch_one(pool)
            .await?;
    let mut registry = admission_registry()
        .lock()
        .expect("write admission registry");
    registry
        .entry(file)
        .or_insert_with(|| Arc::new(WriteAdmission::new()));
    Ok(())
}

/// Begins `BEGIN IMMEDIATE` only after this database's write gate admits it.
pub async fn admitted_write(pool: &SqlitePool) -> Result<WriteTransaction<'_>, sqlx::Error> {
    let file: String =
        sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .fetch_one(pool)
            .await?;
    let Some(admission) = lookup_admission(&file) else {
        return Err(sqlx::Error::Configuration(
            "this database has no write gate".into(),
        ));
    };
    let permit = admission.enter_writer().await;
    match pool.begin_with("BEGIN IMMEDIATE").await {
        Ok(inner) => Ok(WriteTransaction {
            inner: Some(inner),
            permit: Some(permit),
        }),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test]
    async fn dropping_database_releases_lock_even_when_a_duplicate_descriptor_survives() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        // A duplicate shares the open file description, as an inherited Unix
        // descriptor does between fork and exec. No unsafe fork is needed.
        let inherited = database._directory_lock.try_clone().unwrap();
        database.close().await;
        assert!(matches!(
            Database::open(directory.path()).await.err(),
            Some(OpenError::AlreadyRunning)
        ));
        drop(database);
        let reopened = Database::open(directory.path()).await.unwrap();
        assert!(matches!(
            Database::open(directory.path()).await.err(),
            Some(OpenError::AlreadyRunning)
        ));
        drop(inherited);
        // Closing the old duplicate must not unlock the new owner's lock.
        assert!(matches!(
            Database::open(directory.path()).await.err(),
            Some(OpenError::AlreadyRunning)
        ));
        reopened.close().await;
    }

    #[tokio::test]
    async fn committed_wal_data_survives_pool_close_and_database_reopen() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();

        let journal_mode = sqlx::query("PRAGMA journal_mode")
            .fetch_one(database.pool())
            .await
            .unwrap()
            .try_get::<String, _>(0)
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        sqlx::query("CREATE TABLE wal_lifecycle_probe (generation INTEGER NOT NULL)")
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO wal_lifecycle_probe (generation) VALUES (158)")
            .execute(database.pool())
            .await
            .unwrap();

        database.close().await;
        assert!(matches!(
            Database::open(directory.path()).await.err(),
            Some(OpenError::AlreadyRunning)
        ));
        drop(database);

        let reopened = Database::open(directory.path()).await.unwrap();
        let generation = sqlx::query("SELECT generation FROM wal_lifecycle_probe")
            .fetch_one(reopened.pool())
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap();
        assert_eq!(generation, 158);
        reopened.close().await;
    }
}
