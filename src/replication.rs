//! Local disaster recovery through the sibling `sqlite-replication` library.
//!
//! Capture publishes a snapshot and WAL pages into the configured repository.
//! Restore writes a new database file and never replaces the live file.

use std::{io, path::Path, sync::Arc};

use sqlite_replication_core::{RestoreTarget, WriteGate, WritePermit};
use uuid::Uuid;

use crate::db::{Database, ExclusivePermit, WriteAdmission};

#[derive(Debug)]
pub enum ReplicationError {
    Busy,
    Unavailable(io::Error),
}

#[derive(Debug)]
pub struct CaptureReport {
    pub txid: String,
    pub transactions: u64,
    pub wal_frames: u64,
    pub snapshot_created: bool,
}

#[derive(Debug)]
pub struct RestoreReport {
    pub file_name: String,
}

#[derive(Debug)]
pub struct ReplicationStatus {
    pub snapshot: bool,
    pub txid: Option<String>,
    pub same_filesystem: bool,
}

struct ProductGate {
    admission: Arc<WriteAdmission>,
}

struct ProductPermit {
    _inner: ExclusivePermit,
}

impl WriteGate for ProductGate {
    type Permit = ProductPermit;

    fn acquire(&self) -> io::Result<Self::Permit> {
        Ok(ProductPermit {
            _inner: self.admission.acquire_exclusive()?,
        })
    }
}

impl WritePermit for ProductPermit {
    fn release(self) -> io::Result<()> {
        drop(self);
        Ok(())
    }
}

pub async fn status(database: &Database) -> ReplicationStatus {
    let source = database.database_path().to_path_buf();
    let repository = database.replication_dir().to_path_buf();
    tokio::task::spawn_blocking(move || read_status(&source, &repository))
        .await
        .unwrap_or(ReplicationStatus {
            snapshot: false,
            txid: None,
            same_filesystem: true,
        })
}

pub async fn capture(database: &Database) -> Result<CaptureReport, ReplicationError> {
    let source = database.database_path().to_path_buf();
    let repository = database.replication_dir().to_path_buf();
    let admission = database.admission();
    tokio::task::spawn_blocking(move || capture_blocking(&source, &repository, admission))
        .await
        .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))?
}

pub async fn restore_latest(database: &Database) -> Result<RestoreReport, ReplicationError> {
    let repository = database.replication_dir().to_path_buf();
    let restores = database.data_dir().join("restores");
    let live = database.database_path().to_path_buf();
    tokio::task::spawn_blocking(move || restore_blocking(&repository, &restores, &live))
        .await
        .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))?
}

fn read_status(source: &Path, repository: &Path) -> ReplicationStatus {
    let snapshot = sqlite_replication_core::initial_snapshot_path(repository).is_file();
    let txid = sqlite_replication_spool::read_watermark(repository)
        .ok()
        .map(|watermark| watermark.txid.to_string());
    ReplicationStatus {
        snapshot,
        txid,
        same_filesystem: same_filesystem(source, repository),
    }
}

fn capture_blocking(
    source: &Path,
    repository: &Path,
    admission: Arc<WriteAdmission>,
) -> Result<CaptureReport, ReplicationError> {
    let snapshot_existed = sqlite_replication_core::initial_snapshot_path(repository).is_file();
    if !snapshot_existed {
        sqlite_replication_core::snapshot(source, repository).map_err(map_io)?;
    }
    let gate = ProductGate { admission };
    let captured =
        sqlite_replication_core::capture_once(source, repository, &gate).map_err(map_io)?;
    let transactions = u64::try_from(captured.transactions).map_err(|_| {
        ReplicationError::Unavailable(io::Error::other(
            "captured transaction count does not fit in u64",
        ))
    })?;
    Ok(CaptureReport {
        txid: captured.txid.to_string(),
        transactions,
        wal_frames: u64::from(captured.wal_frames),
        snapshot_created: !snapshot_existed,
    })
}

fn restore_blocking(
    repository: &Path,
    restores: &Path,
    live: &Path,
) -> Result<RestoreReport, ReplicationError> {
    crate::db::create_private_directory(restores).map_err(map_io)?;
    let file_name = format!("restore-{}.sqlite3", Uuid::now_v7());
    if Path::new(&file_name).components().count() != 1 {
        return Err(ReplicationError::Unavailable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "restore file name is not a single path component",
        )));
    }
    let destination = restores.join(&file_name);
    if destination == live || destination.exists() {
        return Err(ReplicationError::Unavailable(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "restore destination is not a new file",
        )));
    }
    let now_ms = chrono::Utc::now().timestamp_millis().saturating_add(60_000);
    sqlite_replication_core::restore_to_target(
        repository,
        &destination,
        RestoreTarget::AtOrBeforeTimestampMillis(now_ms),
    )
    .map_err(map_io)?;
    Ok(RestoreReport { file_name })
}

fn map_io(error: io::Error) -> ReplicationError {
    if error.kind() == io::ErrorKind::WouldBlock {
        return ReplicationError::Busy;
    }
    ReplicationError::Unavailable(error)
}

fn same_filesystem(left: &Path, right: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(left_metadata) = std::fs::metadata(left) else {
            return true;
        };
        let Ok(right_metadata) = std::fs::metadata(right) else {
            return true;
        };
        left_metadata.dev() == right_metadata.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        true
    }
}
