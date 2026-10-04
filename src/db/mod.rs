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
