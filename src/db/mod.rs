use std::{
    fs::{DirBuilder, File, OpenOptions, TryLockError},
    path::Path,
    time::Duration,
};

use sqlx::{
    ConnectOptions, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use thiserror::Error;

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
        if let Err(error) =
            crate::jobs::JobStore::recover_abandoned_on_startup(&pool, chrono::Utc::now()).await
        {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self {
            pool,
            _directory_lock: directory_lock,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
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
