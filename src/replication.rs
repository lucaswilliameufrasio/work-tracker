//! Disaster recovery through the sibling `sqlite-replication` library.
//!
//! Capture publishes a snapshot and WAL pages into the local repository and,
//! when an S3-compatible destination is configured, uploads that chain with
//! immutable keys. Restore writes a new database file and never replaces the
//! live file.

use std::{fmt, io, path::Path, sync::Arc};

use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use sqlite_replication_core::{RestoreTarget, WriteGate, WritePermit};
use sqlite_replication_storage_s3::S3Store;
use uuid::Uuid;

use crate::db::{Database, ExclusivePermit, WriteAdmission};

#[derive(Debug)]
pub enum ReplicationError {
    Busy,
    RemoteNotConfigured,
    Remote(io::Error),
    Unavailable(io::Error),
}

#[derive(Debug)]
pub struct CaptureReport {
    pub txid: String,
    pub transactions: u64,
    pub wal_frames: u64,
    pub snapshot_created: bool,
    /// Absent when no S3-compatible destination is configured.
    pub remote: Option<RemoteSync>,
}

/// The local capture is durable either way; a failed upload is retried by the next capture.
#[derive(Debug)]
pub enum RemoteSync {
    Synced {
        txid: String,
        objects_confirmed: u64,
        bytes_confirmed: u64,
    },
    Failed,
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
    /// Absent when no S3-compatible destination is configured.
    pub remote: Option<RemoteStatus>,
}

#[derive(Debug)]
pub struct RemoteStatus {
    pub location: String,
    /// The newest point this destination acknowledged after confirmed uploads.
    pub acknowledged_txid: Option<String>,
}

/// Operator-provided settings for an S3-compatible destination.
pub struct RemoteSettings {
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for RemoteSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSettings")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("path_style", &self.path_style)
            .finish_non_exhaustive()
    }
}

/// An S3-compatible destination for the local repository.
pub struct RemoteReplica {
    store: S3Store,
    bucket: String,
    prefix: String,
    remote_id: String,
}

impl fmt::Debug for RemoteReplica {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteReplica")
            .field("location", &self.location())
            .field("remote_id", &self.remote_id)
            .finish_non_exhaustive()
    }
}

impl RemoteReplica {
    pub fn new(settings: RemoteSettings) -> io::Result<Self> {
        let RemoteSettings {
            bucket,
            prefix,
            region,
            endpoint,
            path_style,
            access_key_id,
            secret_access_key,
        } = settings;
        if bucket.trim().is_empty() || bucket.chars().any(|c| c.is_control() || c == '/') {
            return Err(invalid_input("the S3 bucket name is invalid"));
        }
        validate_prefix(&prefix)?;
        if region.trim().is_empty() || region.chars().any(char::is_control) {
            return Err(invalid_input("the S3 region is invalid"));
        }
        if let Some(endpoint) = &endpoint {
            validate_endpoint(endpoint)?;
        }
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(invalid_input("S3 credentials are required"));
        }
        // Stable and non-secret: identifies the endpoint and bucket, never the credentials.
        let remote_id = match &endpoint {
            Some(endpoint) => format!("s3:{}:{bucket}", endpoint.trim_end_matches('/')),
            None => format!("s3:aws:{region}:{bucket}"),
        };
        let mut config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new(region))
            .force_path_style(path_style)
            .credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "puxiri-replication",
            ));
        if let Some(endpoint) = endpoint {
            config = config.endpoint_url(endpoint);
        }
        let store = S3Store::new(aws_sdk_s3::Client::from_conf(config.build()), &bucket)
            .map_err(|error| invalid_input(&error.to_string()))?;
        Ok(Self {
            store,
            bucket,
            prefix,
            remote_id,
        })
    }

    pub fn location(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
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
    let remote = database.remote_replica().map(|remote| {
        (
            remote.location(),
            remote.remote_id.clone(),
            remote.prefix.clone(),
        )
    });
    tokio::task::spawn_blocking(move || read_status(&source, &repository, remote))
        .await
        .unwrap_or(ReplicationStatus {
            snapshot: false,
            txid: None,
            same_filesystem: true,
            remote: None,
        })
}

pub async fn capture(database: &Database) -> Result<CaptureReport, ReplicationError> {
    let source = database.database_path().to_path_buf();
    let repository = database.replication_dir().to_path_buf();
    let admission = database.admission();
    let mut report = {
        let repository = repository.clone();
        tokio::task::spawn_blocking(move || capture_blocking(&source, &repository, admission))
            .await
            .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))??
    };
    if let Some(remote) = database.remote_replica() {
        report.remote = Some(
            match sqlite_replication_core::sync_to_s3(
                &remote.store,
                &repository,
                &remote.prefix,
                &remote.remote_id,
            )
            .await
            {
                Ok(synced) => RemoteSync::Synced {
                    txid: synced.txid.to_string(),
                    objects_confirmed: u64::try_from(synced.objects_confirmed).unwrap_or(u64::MAX),
                    bytes_confirmed: synced.bytes_confirmed,
                },
                Err(error) => {
                    tracing::warn!(%error, location = %remote.location(), "replication upload failed");
                    RemoteSync::Failed
                }
            },
        );
    }
    Ok(report)
}

pub async fn restore_latest(database: &Database) -> Result<RestoreReport, ReplicationError> {
    let repository = database.replication_dir().to_path_buf();
    let restores = database.data_dir().join("restores");
    let live = database.database_path().to_path_buf();
    tokio::task::spawn_blocking(move || restore_blocking(&repository, &restores, &live))
        .await
        .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))?
}

/// Downloads the remote chain into temporary space, verifies it, and restores
/// the newest point into a new file. The live database is not replaced.
pub async fn restore_latest_from_remote(
    database: &Database,
) -> Result<RestoreReport, ReplicationError> {
    let remote = database
        .remote_replica()
        .ok_or(ReplicationError::RemoteNotConfigured)?;
    let restores = database.data_dir().join("restores");
    let (file_name, destination) =
        new_restore_destination(&restores, database.database_path()).map_err(map_io)?;
    sqlite_replication_core::restore_from_s3(
        &remote.store,
        &remote.prefix,
        &destination,
        latest_target(),
    )
    .await
    .map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            ReplicationError::Busy
        } else {
            ReplicationError::Remote(error)
        }
    })?;
    Ok(RestoreReport { file_name })
}

fn read_status(
    source: &Path,
    repository: &Path,
    remote: Option<(String, String, String)>,
) -> ReplicationStatus {
    let snapshot = sqlite_replication_core::initial_snapshot_path(repository).is_file();
    let txid = sqlite_replication_spool::read_watermark(repository)
        .ok()
        .map(|watermark| watermark.txid.to_string());
    let remote = remote.map(|(location, remote_id, prefix)| RemoteStatus {
        location,
        // An acknowledgement written for another destination says nothing about this one.
        acknowledged_txid: sqlite_replication_spool::read_remote_ack(repository)
            .ok()
            .filter(|ack| ack.remote_id == remote_id && ack.prefix == prefix)
            .map(|ack| ack.txid.to_string()),
    });
    ReplicationStatus {
        snapshot,
        txid,
        same_filesystem: same_filesystem(source, repository),
        remote,
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
        remote: None,
    })
}

fn restore_blocking(
    repository: &Path,
    restores: &Path,
    live: &Path,
) -> Result<RestoreReport, ReplicationError> {
    let (file_name, destination) = new_restore_destination(restores, live).map_err(map_io)?;
    sqlite_replication_core::restore_to_target(repository, &destination, latest_target())
        .map_err(map_io)?;
    Ok(RestoreReport { file_name })
}

fn new_restore_destination(
    restores: &Path,
    live: &Path,
) -> io::Result<(String, std::path::PathBuf)> {
    crate::db::create_private_directory(restores)?;
    let file_name = format!("restore-{}.sqlite3", Uuid::now_v7());
    if Path::new(&file_name).components().count() != 1 {
        return Err(invalid_input(
            "restore file name is not a single path component",
        ));
    }
    let destination = restores.join(&file_name);
    if destination == live || destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "restore destination is not a new file",
        ));
    }
    Ok((file_name, destination))
}

fn latest_target() -> RestoreTarget {
    let now_ms = chrono::Utc::now().timestamp_millis().saturating_add(60_000);
    RestoreTarget::AtOrBeforeTimestampMillis(now_ms)
}

/// Mirrors the library's prefix rule so a bad setting fails at startup, not at the first upload.
fn validate_prefix(prefix: &str) -> io::Result<()> {
    if prefix.starts_with('/')
        || prefix.ends_with('/')
        || prefix.chars().any(char::is_control)
        || prefix
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid_input(
            "the S3 prefix must be non-empty relative segments without a leading or trailing slash",
        ));
    }
    Ok(())
}

/// Plain HTTP is accepted only on loopback, for a local S3-compatible test server.
fn validate_endpoint(endpoint: &str) -> io::Result<()> {
    let host = if let Some(rest) = endpoint.strip_prefix("https://") {
        rest
    } else if let Some(rest) = endpoint.strip_prefix("http://") {
        let loopback = ["127.0.0.1", "localhost", "[::1]"].iter().any(|host| {
            rest == *host
                || rest.starts_with(&format!("{host}:"))
                || rest.starts_with(&format!("{host}/"))
        });
        if !loopback {
            return Err(invalid_input(
                "the S3 endpoint must use HTTPS unless it is on loopback",
            ));
        }
        rest
    } else {
        return Err(invalid_input("the S3 endpoint must be an http(s) URL"));
    };
    if host.is_empty() || host.contains('@') || host.chars().any(|c| c.is_control() || c == ' ') {
        return Err(invalid_input("the S3 endpoint is invalid"));
    }
    Ok(())
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> RemoteSettings {
        RemoteSettings {
            bucket: "puxiri-replica".into(),
            prefix: "installations/main".into(),
            region: "auto".into(),
            endpoint: Some("https://account.r2.example.test".into()),
            path_style: false,
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "not-a-real-secret".into(),
        }
    }

    #[test]
    fn location_and_remote_id_name_the_destination_without_credentials() {
        let remote = RemoteReplica::new(settings()).unwrap();
        assert_eq!(remote.location(), "s3://puxiri-replica/installations/main");
        assert_eq!(
            remote.remote_id,
            "s3:https://account.r2.example.test:puxiri-replica"
        );
        let shown = format!("{remote:?} {:?}", settings());
        assert!(!shown.contains("not-a-real-secret"));
        assert!(!shown.contains("AKIDEXAMPLE"));
    }

    #[test]
    fn aws_default_endpoint_is_identified_by_region() {
        let mut aws = settings();
        aws.endpoint = None;
        aws.region = "sa-east-1".into();
        let remote = RemoteReplica::new(aws).unwrap();
        assert_eq!(remote.remote_id, "s3:aws:sa-east-1:puxiri-replica");
    }

    #[test]
    fn invalid_prefixes_are_rejected() {
        for prefix in ["", "/a", "a/", "a//b", "a/../b", "./a"] {
            let mut bad = settings();
            bad.prefix = prefix.into();
            assert!(RemoteReplica::new(bad).is_err(), "{prefix:?} was accepted");
        }
    }

    #[test]
    fn plain_http_is_only_accepted_on_loopback() {
        for endpoint in [
            "http://127.0.0.1:9000",
            "http://localhost:9000",
            "http://[::1]:9000",
        ] {
            let mut local = settings();
            local.endpoint = Some(endpoint.into());
            assert!(RemoteReplica::new(local).is_ok(), "{endpoint} was rejected");
        }
        for endpoint in [
            "http://s3.example.test",
            "http://localhost.example.test",
            "ftp://s3.example.test",
            "https://user:pass@s3.example.test",
            "https://",
        ] {
            let mut bad = settings();
            bad.endpoint = Some(endpoint.into());
            assert!(RemoteReplica::new(bad).is_err(), "{endpoint} was accepted");
        }
    }

    #[test]
    fn credentials_and_bucket_are_required() {
        let mut missing = settings();
        missing.secret_access_key.clear();
        assert!(RemoteReplica::new(missing).is_err());
        let mut bucket = settings();
        bucket.bucket = " ".into();
        assert!(RemoteReplica::new(bucket).is_err());
    }
}
