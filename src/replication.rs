//! Disaster recovery through the sibling `sqlite-replication` library.
//!
//! Capture publishes a snapshot and WAL pages into the local repository and,
//! when an S3-compatible destination is configured, uploads that chain with
//! immutable keys. Restore writes a new database file and never replaces the
//! live file.
//!
//! The library keeps every level-zero delta of a repository, so one repository
//! only grows. Puxiri bounds it with generations: after the configured age,
//! the next capture starts a new repository from a fresh snapshot under
//! `generations/{id}`. Locally only the current generation is kept. Remotely
//! the newest generations up to the retention count are kept, and older ones
//! are deleted only after the current generation is acknowledged.

use std::{
    collections::BTreeSet,
    fmt, io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
    /// The capture began a new generation from a fresh snapshot.
    pub generation_started: bool,
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
    /// When the current generation began, in Unix milliseconds.
    pub generation_started_at_millis: Option<i64>,
    pub policy: ReplicationPolicy,
    /// Absent when no S3-compatible destination is configured.
    pub remote: Option<RemoteStatus>,
}

/// How often Puxiri captures by itself, and how much history it keeps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicationPolicy {
    /// `None` leaves capture to an administrator.
    pub capture_interval: Option<Duration>,
    /// Age after which the next capture starts a new generation.
    pub generation_age: Duration,
    /// Remote generations kept, the current one included.
    pub retained_generations: NonZeroUsize,
}

impl Default for ReplicationPolicy {
    fn default() -> Self {
        Self {
            capture_interval: Some(Duration::from_secs(300)),
            generation_age: Duration::from_secs(24 * 60 * 60),
            retained_generations: NonZeroUsize::new(7).expect("non-zero"),
        }
    }
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
    /// Cleared when a generation starts or a prune fails, so the next acknowledged upload prunes.
    pruned: AtomicBool,
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
            pruned: AtomicBool::new(false),
        })
    }

    fn generations_prefix(&self) -> String {
        format!("{}/{GENERATIONS}/", self.prefix)
    }

    fn generation_prefix(&self, generation: Uuid) -> String {
        format!("{}/{GENERATIONS}/{generation}", self.prefix)
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

const GENERATIONS: &str = "generations";
const CURRENT_GENERATION: &str = "current-generation";

pub async fn status(database: &Database) -> ReplicationStatus {
    let source = database.database_path().to_path_buf();
    let root = database.replication_dir().to_path_buf();
    let policy = database.replication_policy();
    let remote = database.remote_replica();
    let fallback_policy = policy;
    tokio::task::spawn_blocking(move || read_status(&source, &root, policy, remote.as_deref()))
        .await
        .unwrap_or(ReplicationStatus {
            snapshot: false,
            txid: None,
            same_filesystem: true,
            generation_started_at_millis: None,
            policy: fallback_policy,
            remote: None,
        })
}

/// Captures into the current generation, starting a new one when it is due,
/// then uploads and prunes when an S3-compatible destination is configured.
pub async fn capture(database: &Database) -> Result<CaptureReport, ReplicationError> {
    let _operation = database
        .replication_lock()
        .try_lock()
        .map_err(|_| ReplicationError::Busy)?;
    let source = database.database_path().to_path_buf();
    let root = database.replication_dir().to_path_buf();
    let admission = database.admission();
    let policy = database.replication_policy();
    let (mut report, generation) = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || capture_blocking(&source, &root, admission, policy))
            .await
            .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))??
    };
    if let Some(remote) = database.remote_replica() {
        if report.generation_started {
            remote.pruned.store(false, Ordering::SeqCst);
        }
        let repository = generation_dir(&root, generation);
        report.remote = Some(sync_and_prune(&remote, &repository, generation, policy).await);
    }
    Ok(report)
}

async fn sync_and_prune(
    remote: &RemoteReplica,
    repository: &Path,
    generation: Uuid,
    policy: ReplicationPolicy,
) -> RemoteSync {
    let prefix = remote.generation_prefix(generation);
    let watermark = sqlite_replication_spool::read_watermark(repository)
        .ok()
        .map(|watermark| watermark.txid);
    let acknowledged = acknowledged_txid(repository, &remote.remote_id, &prefix);
    let synced = if watermark.is_some() && watermark == acknowledged {
        // Nothing new since the last confirmed upload; skip listing and re-checking every object.
        RemoteSync::Synced {
            txid: watermark.unwrap_or_default().to_string(),
            objects_confirmed: 0,
            bytes_confirmed: 0,
        }
    } else {
        match sqlite_replication_core::sync_to_s3(
            &remote.store,
            repository,
            &prefix,
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
                return RemoteSync::Failed;
            }
        }
    };
    if !remote.pruned.load(Ordering::SeqCst) {
        match prune_remote(remote, generation, policy.retained_generations).await {
            Ok(removed) => {
                remote.pruned.store(true, Ordering::SeqCst);
                if removed > 0 {
                    tracing::info!(removed, location = %remote.location(), "old replication generations removed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, location = %remote.location(), "replication retention failed");
            }
        }
    }
    synced
}

/// Deletes remote generations older than the newest `retained`, never the current one.
/// Only called after the current generation's upload is acknowledged.
async fn prune_remote(
    remote: &RemoteReplica,
    current: Uuid,
    retained: NonZeroUsize,
) -> io::Result<usize> {
    let objects = list_generation_objects(remote).await?;
    let generations: BTreeSet<Uuid> = objects.iter().map(|(generation, _)| *generation).collect();
    let keep: BTreeSet<Uuid> = generations
        .iter()
        .rev()
        .take(retained.get())
        .copied()
        .chain(std::iter::once(current))
        .collect();
    let doomed: Vec<String> = objects
        .into_iter()
        .filter(|(generation, _)| !keep.contains(generation))
        .map(|(_, key)| key)
        .collect();
    let removed = generations
        .iter()
        .filter(|generation| !keep.contains(generation))
        .count();
    for keys in doomed.chunks(1000) {
        remote
            .store
            .delete_batch(keys)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
    Ok(removed)
}

/// Every object under `{prefix}/generations/{uuid}/`. Keys that do not name a
/// generation are left out, so retention never deletes what Puxiri did not write.
async fn list_generation_objects(remote: &RemoteReplica) -> io::Result<Vec<(Uuid, String)>> {
    let prefix = remote.generations_prefix();
    let mut objects = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = remote
            .store
            .list_page(&prefix, cursor.as_deref(), 1000)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        for object in page.contents() {
            let Some(key) = object.key() else {
                continue;
            };
            let Some((segment, rest)) = key
                .strip_prefix(&prefix)
                .and_then(|relative| relative.split_once('/'))
            else {
                continue;
            };
            let Ok(generation) = Uuid::parse_str(segment) else {
                continue;
            };
            // Only the exact form Puxiri writes; another spelling of the same UUID is not ours.
            if generation.hyphenated().to_string() != segment || rest.is_empty() {
                continue;
            }
            objects.push((generation, key.to_owned()));
        }
        match page.next_continuation_token() {
            Some(next) if page.is_truncated() == Some(true) => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok(objects)
}

pub async fn restore_latest(database: &Database) -> Result<RestoreReport, ReplicationError> {
    let _operation = database
        .replication_lock()
        .try_lock()
        .map_err(|_| ReplicationError::Busy)?;
    let root = database.replication_dir().to_path_buf();
    let restores = database.data_dir().join("restores");
    let live = database.database_path().to_path_buf();
    tokio::task::spawn_blocking(move || restore_blocking(&root, &restores, &live))
        .await
        .map_err(|error| ReplicationError::Unavailable(io::Error::other(error)))?
}

/// Downloads the newest complete remote generation into temporary space,
/// verifies it, and restores its newest point into a new file. The live
/// database is not replaced.
pub async fn restore_latest_from_remote(
    database: &Database,
) -> Result<RestoreReport, ReplicationError> {
    let remote = database
        .remote_replica()
        .ok_or(ReplicationError::RemoteNotConfigured)?;
    let _operation = database
        .replication_lock()
        .try_lock()
        .map_err(|_| ReplicationError::Busy)?;
    let prefix = newest_remote_repository(&remote)
        .await
        .map_err(ReplicationError::Remote)?;
    let restores = database.data_dir().join("restores");
    let (file_name, destination) =
        new_restore_destination(&restores, database.database_path()).map_err(map_io)?;
    sqlite_replication_core::restore_from_s3(&remote.store, &prefix, &destination, latest_target())
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

/// The newest generation whose manifest was published, which `sync_to_s3` does
/// last. Falls back to a repository written directly under the prefix before
/// generations existed.
async fn newest_remote_repository(remote: &RemoteReplica) -> io::Result<String> {
    let objects = list_generation_objects(remote).await?;
    let complete = objects
        .iter()
        .filter(|(_, key)| key.ends_with("/_meta/repository.json"))
        .map(|(generation, _)| *generation)
        .max();
    if let Some(generation) = complete {
        return Ok(remote.generation_prefix(generation));
    }
    let legacy = format!("{}/_meta/repository.json", remote.prefix);
    match remote.store.head(&legacy).await {
        Ok(_) => Ok(remote.prefix.clone()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no replicated generation was found at the S3 destination",
        )),
    }
}

fn read_status(
    source: &Path,
    root: &Path,
    policy: ReplicationPolicy,
    remote: Option<&RemoteReplica>,
) -> ReplicationStatus {
    let generation = current_generation(root).ok().flatten();
    let repository = match generation {
        Some(generation) => generation_dir(root, generation),
        None => root.to_path_buf(),
    };
    let snapshot = sqlite_replication_core::initial_snapshot_path(&repository).is_file();
    let txid = sqlite_replication_spool::read_watermark(&repository)
        .ok()
        .map(|watermark| watermark.txid.to_string());
    let remote = remote.map(|remote| RemoteStatus {
        location: remote.location(),
        // An acknowledgement written for another destination or generation says nothing about this one.
        acknowledged_txid: generation
            .and_then(|generation| {
                acknowledged_txid(
                    &repository,
                    &remote.remote_id,
                    &remote.generation_prefix(generation),
                )
            })
            .map(|txid| txid.to_string()),
    });
    ReplicationStatus {
        snapshot,
        txid,
        same_filesystem: same_filesystem(source, root),
        generation_started_at_millis: generation.and_then(generation_started_millis),
        policy,
        remote,
    }
}

fn acknowledged_txid(repository: &Path, remote_id: &str, prefix: &str) -> Option<u64> {
    sqlite_replication_spool::read_remote_ack(repository)
        .ok()
        .filter(|ack| ack.remote_id == remote_id && ack.prefix == prefix)
        .map(|ack| ack.txid)
}

fn capture_blocking(
    source: &Path,
    root: &Path,
    admission: Arc<WriteAdmission>,
    policy: ReplicationPolicy,
) -> Result<(CaptureReport, Uuid), ReplicationError> {
    let gate = ProductGate { admission };
    let current = current_generation(root).map_err(map_io)?;
    let due = match current.and_then(generation_started_millis) {
        None => true,
        Some(started) => {
            let age = i64::try_from(policy.generation_age.as_millis()).unwrap_or(i64::MAX);
            chrono::Utc::now().timestamp_millis() >= started.saturating_add(age)
        }
    };
    if due {
        match start_generation(source, root, &gate) {
            Ok((generation, captured)) => {
                if let Some(previous) = current {
                    // The new snapshot holds everything the previous generation did.
                    if let Err(error) = std::fs::remove_dir_all(generation_dir(root, previous)) {
                        tracing::warn!(%error, %previous, "previous replication generation was not removed");
                    }
                }
                return Ok((report(captured, true, true)?, generation));
            }
            Err(ReplicationError::Unavailable(error)) if current.is_some() => {
                // Keep protecting the database in the generation that still works.
                tracing::warn!(%error, "new replication generation failed; capturing into the current one");
            }
            Err(error) => return Err(error),
        }
    }
    let generation = current.expect("a generation exists when none is due");
    let repository = generation_dir(root, generation);
    let captured =
        sqlite_replication_core::capture_once(source, &repository, &gate).map_err(map_io)?;
    Ok((report(captured, false, false)?, generation))
}

fn start_generation(
    source: &Path,
    root: &Path,
    gate: &ProductGate,
) -> Result<(Uuid, sqlite_replication_core::CaptureResult), ReplicationError> {
    let generation = Uuid::now_v7();
    let repository = generation_dir(root, generation);
    crate::db::create_private_directory(&root.join(GENERATIONS)).map_err(map_io)?;
    crate::db::create_private_directory(&repository).map_err(map_io)?;
    let started = sqlite_replication_core::snapshot(source, &repository)
        .and_then(|_| sqlite_replication_core::capture_once(source, &repository, gate))
        .and_then(|captured| write_current_generation(root, generation).map(|()| captured));
    match started {
        Ok(captured) => Ok((generation, captured)),
        Err(error) => {
            // An unfinished generation is never pointed to; remove it so it cannot be mistaken for one.
            let _ = std::fs::remove_dir_all(&repository);
            Err(map_io(error))
        }
    }
}

fn report(
    captured: sqlite_replication_core::CaptureResult,
    snapshot_created: bool,
    generation_started: bool,
) -> Result<CaptureReport, ReplicationError> {
    let transactions = u64::try_from(captured.transactions).map_err(|_| {
        ReplicationError::Unavailable(io::Error::other(
            "captured transaction count does not fit in u64",
        ))
    })?;
    Ok(CaptureReport {
        txid: captured.txid.to_string(),
        transactions,
        wal_frames: u64::from(captured.wal_frames),
        snapshot_created,
        generation_started,
        remote: None,
    })
}

fn generation_dir(root: &Path, generation: Uuid) -> PathBuf {
    root.join(GENERATIONS).join(generation.to_string())
}

fn generation_started_millis(generation: Uuid) -> Option<i64> {
    let (seconds, nanos) = generation.get_timestamp()?.to_unix();
    i64::try_from(seconds)
        .ok()?
        .checked_mul(1000)?
        .checked_add(i64::from(nanos / 1_000_000))
}

fn current_generation(root: &Path) -> io::Result<Option<Uuid>> {
    let text = match std::fs::read_to_string(root.join(CURRENT_GENERATION)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let generation = Uuid::parse_str(text.trim())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid current generation"))?;
    if !generation_dir(root, generation).is_dir() {
        return Ok(None);
    }
    Ok(Some(generation))
}

/// Replaces the pointer atomically so a crash leaves the old or the new generation, never neither.
fn write_current_generation(root: &Path, generation: Uuid) -> io::Result<()> {
    use std::io::Write;
    let staged = root.join(format!(".{CURRENT_GENERATION}-{}", Uuid::now_v7()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        file.write_all(generation.to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, root.join(CURRENT_GENERATION))?;
        std::fs::File::open(root)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

fn restore_blocking(
    root: &Path,
    restores: &Path,
    live: &Path,
) -> Result<RestoreReport, ReplicationError> {
    // Before generations existed, the repository lived directly in the root.
    let repository = match current_generation(root).map_err(map_io)? {
        Some(generation) => generation_dir(root, generation),
        None => root.to_path_buf(),
    };
    let (file_name, destination) = new_restore_destination(restores, live).map_err(map_io)?;
    sqlite_replication_core::restore_to_target(&repository, &destination, latest_target())
        .map_err(map_io)?;
    Ok(RestoreReport { file_name })
}

/// Captures on the policy's interval until `shutdown` changes. A capture in
/// progress finishes before the task ends. Returns `None` when the policy has no interval.
pub fn spawn_scheduler(
    database: Arc<Database>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let interval = database.replication_policy().capture_interval?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            match capture(&database).await {
                Ok(report) => tracing::debug!(
                    txid = %report.txid,
                    generation_started = report.generation_started,
                    "scheduled replication capture"
                ),
                Err(ReplicationError::Busy) => {
                    tracing::debug!(
                        "scheduled replication capture skipped: another operation is running"
                    );
                }
                Err(error) => tracing::warn!(?error, "scheduled replication capture failed"),
            }
        }
    }))
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
