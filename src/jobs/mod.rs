use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::Value;
use sqlx::{FromRow, Row, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_PAYLOAD_BYTES: usize = 65_536;

const JOB_SELECT_BY_ID: &str = "SELECT id, job_kind, payload_version, payload_json, dedupe_key, \
    source_outbox_event_id, state, available_at, attempts, max_attempts, claim_epoch, \
    lease_expires_at, last_error_code, created_at, updated_at, finished_at FROM jobs WHERE id = ?";
const JOB_SELECT_BY_DEDUPE_KEY: &str = "SELECT id, job_kind, payload_version, payload_json, dedupe_key, \
    source_outbox_event_id, state, available_at, attempts, max_attempts, claim_epoch, \
    lease_expires_at, last_error_code, created_at, updated_at, finished_at FROM jobs WHERE dedupe_key = ?";

#[derive(Debug, Error)]
pub enum JobStoreError {
    #[error("job store database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("job specification is invalid")]
    InvalidSpecification,
    #[error("job queue capacity has been reached")]
    Capacity,
    #[error("deduplication key was reused for different work")]
    DeduplicationConflict,
    #[error("job claim is no longer current")]
    StaleClaim,
    #[error("job is not in a state that permits this operation")]
    InvalidTransition,
    #[error("persisted job record is invalid")]
    CorruptRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl JobState {
    fn parse(value: &str) -> Result<Self, JobStoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            _ => Err(JobStoreError::CorruptRecord),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

pub struct JobSpec {
    pub job_kind: String,
    pub payload_version: u16,
    pub payload: Value,
    pub dedupe_key: String,
    pub source_outbox_event_id: Option<String>,
    pub available_at: DateTime<Utc>,
    pub max_attempts: u16,
}

pub struct JobRecord {
    pub id: String,
    pub job_kind: String,
    pub payload_version: u16,
    pub payload: Value,
    pub dedupe_key: String,
    pub source_outbox_event_id: Option<String>,
    pub state: JobState,
    pub available_at: DateTime<Utc>,
    pub attempts: u16,
    pub max_attempts: u16,
    pub claim_epoch: i64,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub struct EnqueueOutcome {
    pub job: JobRecord,
    pub inserted: bool,
}

pub struct JobClaim {
    pub job: JobRecord,
    token: String,
}

pub enum JobFailure {
    Permanent {
        code: String,
    },
    Transient {
        code: String,
        retry_at: DateTime<Utc>,
    },
}

pub struct JobStore {
    pool: SqlitePool,
}

#[derive(FromRow)]
struct JobRow {
    id: String,
    job_kind: String,
    payload_version: i64,
    payload_json: String,
    dedupe_key: String,
    source_outbox_event_id: Option<String>,
    state: String,
    available_at: String,
    attempts: i64,
    max_attempts: i64,
    claim_epoch: i64,
    lease_expires_at: Option<String>,
    last_error_code: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

impl JobStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Atomically accepts bounded work under the producer's job-kind limit.
    pub async fn enqueue(
        &self,
        spec: JobSpec,
        max_outstanding: u32,
        now: DateTime<Utc>,
    ) -> Result<EnqueueOutcome, JobStoreError> {
        let payload_json = validate_and_encode_spec(&spec)?;
        if max_outstanding == 0 {
            return Err(JobStoreError::InvalidSpecification);
        }
        let available_at = encode_time(spec.available_at);
        let created_at = encode_time(now);
        let source_event_id = spec
            .source_outbox_event_id
            .as_deref()
            .map(canonical_uuid)
            .transpose()?;

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) = load_by_dedupe_key(&mut transaction, &spec.dedupe_key).await? {
            let inserted = false;
            if !same_specification(
                &existing,
                &spec,
                &payload_json,
                &available_at,
                source_event_id.as_deref(),
            ) {
                return Err(JobStoreError::DeduplicationConflict);
            }
            transaction.commit().await?;
            return Ok(EnqueueOutcome {
                job: existing,
                inserted,
            });
        }

        let outstanding: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs \
             WHERE job_kind = ? AND state IN ('pending', 'running')",
        )
        .bind(&spec.job_kind)
        .fetch_one(&mut *transaction)
        .await?;
        if outstanding >= i64::from(max_outstanding) {
            return Err(JobStoreError::Capacity);
        }

        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO jobs (
                id, job_kind, payload_version, payload_json, dedupe_key,
                source_outbox_event_id, state, available_at, attempts, max_attempts,
                claim_epoch, claim_token, lease_expires_at, last_error_code,
                created_at, updated_at, finished_at
             ) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, 0, ?, 0, NULL, NULL, NULL, ?, ?, NULL)",
        )
        .bind(&id)
        .bind(&spec.job_kind)
        .bind(i64::from(spec.payload_version))
        .bind(payload_json)
        .bind(&spec.dedupe_key)
        .bind(source_event_id)
        .bind(&available_at)
        .bind(i64::from(spec.max_attempts))
        .bind(&created_at)
        .bind(&created_at)
        .execute(&mut *transaction)
        .await?;

        let job = load_by_id(&mut transaction, &id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(EnqueueOutcome {
            job,
            inserted: true,
        })
    }

    /// Claims one due pending job. Expired running jobs are never stolen here.
    pub async fn claim_next(
        &self,
        now: DateTime<Utc>,
        lease_for: TimeDelta,
    ) -> Result<Option<JobClaim>, JobStoreError> {
        if lease_for <= TimeDelta::zero() {
            return Err(JobStoreError::InvalidSpecification);
        }
        let lease_expires_at = now
            .checked_add_signed(lease_for)
            .ok_or(JobStoreError::InvalidSpecification)?;
        let now_text = encode_time(now);
        let lease_text = encode_time(lease_expires_at);
        let token = Uuid::now_v7().to_string();

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let selected = sqlx::query(
            "SELECT id, claim_epoch FROM jobs \
             WHERE state = 'pending' AND available_at <= ? AND attempts < max_attempts \
             ORDER BY available_at, id LIMIT 1",
        )
        .bind(&now_text)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(selected) = selected else {
            transaction.commit().await?;
            return Ok(None);
        };
        let id: String = selected.try_get("id")?;
        let old_epoch: i64 = selected.try_get("claim_epoch")?;
        let new_epoch = old_epoch
            .checked_add(1)
            .ok_or(JobStoreError::InvalidTransition)?;

        let changed = sqlx::query(
            "UPDATE jobs SET state = 'running', attempts = attempts + 1, \
             claim_epoch = ?, claim_token = ?, lease_expires_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'pending' AND claim_epoch = ? \
             AND attempts < max_attempts",
        )
        .bind(new_epoch)
        .bind(&token)
        .bind(&lease_text)
        .bind(&now_text)
        .bind(&id)
        .bind(old_epoch)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(JobStoreError::InvalidTransition);
        }

        let job = load_by_id(&mut transaction, &id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(Some(JobClaim { job, token }))
    }

    /// Extends a live claim without permitting an expired lease to be revived.
    pub async fn heartbeat(
        &self,
        claim: &mut JobClaim,
        now: DateTime<Utc>,
        lease_for: TimeDelta,
    ) -> Result<(), JobStoreError> {
        if lease_for <= TimeDelta::zero() {
            return Err(JobStoreError::InvalidSpecification);
        }
        let proposed_expiry = now
            .checked_add_signed(lease_for)
            .ok_or(JobStoreError::InvalidSpecification)?;
        let now_text = encode_time(now);
        let expiry_text = encode_time(proposed_expiry);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE jobs SET \
             lease_expires_at = CASE WHEN lease_expires_at < ? THEN ? ELSE lease_expires_at END, \
             updated_at = CASE WHEN updated_at < ? THEN ? ELSE updated_at END \
             WHERE id = ? AND state = 'running' AND claim_epoch = ? AND claim_token = ? \
             AND lease_expires_at > ?",
        )
        .bind(&expiry_text)
        .bind(&expiry_text)
        .bind(&now_text)
        .bind(&now_text)
        .bind(&claim.job.id)
        .bind(claim.job.claim_epoch)
        .bind(&claim.token)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(JobStoreError::StaleClaim);
        }
        claim.job = load_by_id(&mut transaction, &claim.job.id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn succeed(
        &self,
        claim: &JobClaim,
        now: DateTime<Utc>,
    ) -> Result<JobRecord, JobStoreError> {
        let now_text = encode_time(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE jobs SET state = 'succeeded', claim_token = NULL, \
             lease_expires_at = NULL, finished_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'running' AND claim_epoch = ? AND claim_token = ? \
             AND lease_expires_at > ?",
        )
        .bind(&now_text)
        .bind(&now_text)
        .bind(&claim.job.id)
        .bind(claim.job.claim_epoch)
        .bind(&claim.token)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(JobStoreError::StaleClaim);
        }
        let completed = load_by_id(&mut transaction, &claim.job.id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(completed)
    }

    pub async fn fail(
        &self,
        claim: &JobClaim,
        failure: JobFailure,
        now: DateTime<Utc>,
    ) -> Result<JobRecord, JobStoreError> {
        let (code, retry_at) = match failure {
            JobFailure::Permanent { code } => (code, None),
            JobFailure::Transient { code, retry_at } => (code, Some(retry_at)),
        };
        if !valid_error_code(&code) {
            return Err(JobStoreError::InvalidSpecification);
        }
        if retry_at.is_some_and(|retry_at| retry_at <= now) {
            return Err(JobStoreError::InvalidSpecification);
        }

        let schedule_retry = retry_at.is_some() && claim.job.attempts < claim.job.max_attempts;
        let state = if schedule_retry {
            JobState::Pending
        } else {
            JobState::Failed
        };
        let now_text = encode_time(now);
        let finished_at = if state == JobState::Failed {
            Some(now_text.clone())
        } else {
            None
        };
        let available_at = if schedule_retry {
            encode_time(retry_at.ok_or(JobStoreError::InvalidSpecification)?)
        } else {
            encode_time(claim.job.available_at)
        };
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE jobs SET state = ?, available_at = ?, claim_token = NULL, \
             lease_expires_at = NULL, last_error_code = ?, finished_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'running' AND claim_epoch = ? AND claim_token = ? \
             AND lease_expires_at > ?",
        )
        .bind(state.as_str())
        .bind(available_at)
        .bind(code)
        .bind(finished_at)
        .bind(&now_text)
        .bind(&claim.job.id)
        .bind(claim.job.claim_epoch)
        .bind(&claim.token)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(JobStoreError::StaleClaim);
        }
        let completed = load_by_id(&mut transaction, &claim.job.id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(completed)
    }

    pub async fn cancel_pending(
        &self,
        job_id: &str,
        now: DateTime<Utc>,
    ) -> Result<JobRecord, JobStoreError> {
        let now_text = encode_time(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE jobs SET state = 'canceled', finished_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(&now_text)
        .bind(&now_text)
        .bind(job_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(JobStoreError::InvalidTransition);
        }
        let canceled = load_by_id(&mut transaction, job_id)
            .await?
            .ok_or(JobStoreError::CorruptRecord)?;
        transaction.commit().await?;
        Ok(canceled)
    }

    pub async fn get(&self, job_id: &str) -> Result<Option<JobRecord>, JobStoreError> {
        let row = sqlx::query_as::<_, JobRow>(JOB_SELECT_BY_ID)
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(map_row).transpose()
    }

    /// Called only by `Database::open` while the exclusive installation lock is held.
    pub(crate) async fn recover_abandoned_on_startup(
        pool: &SqlitePool,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let now_text = encode_time(now);
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query(
            "UPDATE jobs SET \
             state = CASE WHEN attempts < max_attempts THEN 'pending' ELSE 'failed' END, \
             available_at = CASE WHEN attempts < max_attempts THEN ? ELSE available_at END, \
             claim_epoch = claim_epoch + 1, claim_token = NULL, lease_expires_at = NULL, \
             last_error_code = 'worker_interrupted', \
             finished_at = CASE WHEN attempts < max_attempts THEN NULL ELSE ? END, \
             updated_at = ? \
             WHERE state = 'running'",
        )
        .bind(&now_text)
        .bind(&now_text)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected())
    }
}

fn validate_and_encode_spec(spec: &JobSpec) -> Result<String, JobStoreError> {
    if !valid_job_kind(&spec.job_kind)
        || spec.payload_version == 0
        || spec.max_attempts == 0
        || spec.dedupe_key.is_empty()
        || spec.dedupe_key.len() > 255
        || !spec
            .dedupe_key
            .bytes()
            .all(|byte| (b'!'..=b'~').contains(&byte))
        || !spec.payload.is_object()
    {
        return Err(JobStoreError::InvalidSpecification);
    }
    let payload_json =
        serde_json::to_string(&spec.payload).map_err(|_| JobStoreError::InvalidSpecification)?;
    if payload_json.len() > MAX_PAYLOAD_BYTES {
        return Err(JobStoreError::InvalidSpecification);
    }
    Ok(payload_json)
}

fn valid_job_kind(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
    {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

fn valid_error_code(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
    {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

fn canonical_uuid(value: &str) -> Result<String, JobStoreError> {
    let parsed = Uuid::parse_str(value).map_err(|_| JobStoreError::InvalidSpecification)?;
    if parsed.to_string() != value {
        return Err(JobStoreError::InvalidSpecification);
    }
    Ok(value.to_owned())
}

fn encode_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, JobStoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| JobStoreError::CorruptRecord)
}

async fn load_by_id(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<Option<JobRecord>, JobStoreError> {
    let row = sqlx::query_as::<_, JobRow>(JOB_SELECT_BY_ID)
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await?;
    row.map(map_row).transpose()
}

async fn load_by_dedupe_key(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    dedupe_key: &str,
) -> Result<Option<JobRecord>, JobStoreError> {
    let row = sqlx::query_as::<_, JobRow>(JOB_SELECT_BY_DEDUPE_KEY)
        .bind(dedupe_key)
        .fetch_optional(&mut **transaction)
        .await?;
    row.map(map_row).transpose()
}

fn same_specification(
    existing: &JobRecord,
    spec: &JobSpec,
    payload_json: &str,
    available_at: &str,
    source_event_id: Option<&str>,
) -> bool {
    existing.job_kind == spec.job_kind
        && existing.payload_version == spec.payload_version
        && serde_json::to_string(&existing.payload).is_ok_and(|value| value == payload_json)
        && existing.dedupe_key == spec.dedupe_key
        && existing.source_outbox_event_id.as_deref() == source_event_id
        && encode_time(existing.available_at) == available_at
        && existing.max_attempts == spec.max_attempts
}

fn map_row(row: JobRow) -> Result<JobRecord, JobStoreError> {
    let payload: Value =
        serde_json::from_str(&row.payload_json).map_err(|_| JobStoreError::CorruptRecord)?;
    if !payload.is_object() {
        return Err(JobStoreError::CorruptRecord);
    }
    let payload_version =
        u16::try_from(row.payload_version).map_err(|_| JobStoreError::CorruptRecord)?;
    let attempts = u16::try_from(row.attempts).map_err(|_| JobStoreError::CorruptRecord)?;
    let max_attempts = u16::try_from(row.max_attempts).map_err(|_| JobStoreError::CorruptRecord)?;
    if payload_version == 0 || max_attempts == 0 || attempts > max_attempts || row.claim_epoch < 0 {
        return Err(JobStoreError::CorruptRecord);
    }

    Ok(JobRecord {
        id: row.id,
        job_kind: row.job_kind,
        payload_version,
        payload,
        dedupe_key: row.dedupe_key,
        source_outbox_event_id: row.source_outbox_event_id,
        state: JobState::parse(&row.state)?,
        available_at: parse_time(&row.available_at)?,
        attempts,
        max_attempts,
        claim_epoch: row.claim_epoch,
        lease_expires_at: row
            .lease_expires_at
            .as_deref()
            .map(parse_time)
            .transpose()?,
        last_error_code: row.last_error_code,
        created_at: parse_time(&row.created_at)?,
        updated_at: parse_time(&row.updated_at)?,
        finished_at: row.finished_at.as_deref().map(parse_time).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use tempfile::TempDir;

    fn spec(dedupe_key: &str, max_attempts: u16, available_at: DateTime<Utc>) -> JobSpec {
        JobSpec {
            job_kind: "issue.notify".to_owned(),
            payload_version: 1,
            payload: serde_json::json!({"issue_id":"01890f3e-7b1c-7cc4-98a7-123456789abc"}),
            dedupe_key: dedupe_key.to_owned(),
            source_outbox_event_id: None,
            available_at,
            max_attempts,
        }
    }

    async fn database() -> (TempDir, Database) {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        (directory, database)
    }

    #[tokio::test]
    async fn expired_claim_is_not_stolen_and_cannot_commit() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let accepted = store
            .enqueue(spec("issue/one", 3, now), 10, now)
            .await
            .unwrap();
        let claim = store
            .claim_next(now, TimeDelta::seconds(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.job.id, accepted.job.id);
        assert_eq!(claim.job.attempts, 1);
        assert_eq!(claim.job.claim_epoch, 1);

        let expired = now + TimeDelta::seconds(3);
        assert!(
            store
                .claim_next(expired, TimeDelta::seconds(2))
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.succeed(&claim, expired).await,
            Err(JobStoreError::StaleClaim)
        ));

        let still_running = store.get(&claim.job.id).await.unwrap().unwrap();
        assert_eq!(still_running.state, JobState::Running);
        assert_eq!(still_running.claim_epoch, 1);
    }

    #[tokio::test]
    async fn process_startup_recovers_abandoned_claims_only_after_database_lock_is_acquired() {
        let directory = TempDir::new().unwrap();
        let first_database = Database::open(directory.path()).await.unwrap();
        let first_store = JobStore::new(first_database.pool().clone());
        let now = Utc::now();
        let enqueued = first_store
            .enqueue(spec("issue/restart", 2, now), 10, now)
            .await
            .unwrap();
        let claim = first_store
            .claim_next(now, TimeDelta::seconds(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.job.id, enqueued.job.id);
        assert_eq!(
            first_store.get(&claim.job.id).await.unwrap().unwrap().state,
            JobState::Running
        );
        assert!(matches!(
            Database::open(directory.path()).await.err(),
            Some(crate::db::OpenError::AlreadyRunning)
        ));
        assert_eq!(
            first_store
                .get(&claim.job.id)
                .await
                .unwrap()
                .unwrap()
                .claim_epoch,
            1
        );

        first_database.close().await;
        drop(first_store);
        drop(first_database);

        let reopened = Database::open(directory.path()).await.unwrap();
        let recovered_store = JobStore::new(reopened.pool().clone());
        assert!(matches!(
            recovered_store
                .succeed(&claim, now + TimeDelta::seconds(2))
                .await,
            Err(JobStoreError::StaleClaim)
        ));
        let recovered = recovered_store
            .get(&enqueued.job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, JobState::Pending);
        assert_eq!(recovered.claim_epoch, 2);
        assert_eq!(recovered.attempts, 1);
        assert_eq!(
            recovered.last_error_code.as_deref(),
            Some("worker_interrupted")
        );
    }

    #[tokio::test]
    async fn startup_recovery_exhausts_attempt_budget_without_requeueing() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let accepted = store
            .enqueue(spec("issue/exhaust", 1, now), 10, now)
            .await
            .unwrap();
        let _claim = store
            .claim_next(now, TimeDelta::seconds(2))
            .await
            .unwrap()
            .unwrap();
        database.close().await;
        drop(store);
        drop(database);

        let reopened = Database::open(directory.path()).await.unwrap();
        let recovered = JobStore::new(reopened.pool().clone())
            .get(&accepted.job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, JobState::Failed);
        assert_eq!(recovered.finished_at, Some(recovered.updated_at));
        assert_eq!(
            recovered.last_error_code.as_deref(),
            Some("worker_interrupted")
        );
    }

    #[tokio::test]
    async fn recovery_rolls_back_the_entire_batch_if_one_epoch_cannot_be_fenced() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let one = store
            .enqueue(spec("issue/one-recovery", 2, now), 10, now)
            .await
            .unwrap();
        let two = store
            .enqueue(spec("issue/two-recovery", 2, now), 10, now)
            .await
            .unwrap();
        let _one_claim = store
            .claim_next(now, TimeDelta::seconds(2))
            .await
            .unwrap()
            .unwrap();
        let _two_claim = store
            .claim_next(now, TimeDelta::seconds(2))
            .await
            .unwrap()
            .unwrap();
        let trigger = format!(
            "CREATE TRIGGER reject_job_recovery BEFORE UPDATE OF claim_epoch ON jobs \
             WHEN OLD.id = '{}' BEGIN SELECT RAISE(FAIL, 'fixture'); END",
            two.job.id
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(trigger))
            .execute(database.pool())
            .await
            .unwrap();

        assert!(
            JobStore::recover_abandoned_on_startup(database.pool(), now)
                .await
                .is_err()
        );
        for job_id in [&one.job.id, &two.job.id] {
            let job = store.get(job_id).await.unwrap().unwrap();
            assert_eq!(job.state, JobState::Running);
            assert_eq!(job.claim_epoch, 1);
        }
    }

    #[tokio::test]
    async fn concurrent_claims_have_one_winner_and_payload_and_attempt_budget_are_immutable() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let accepted = store
            .enqueue(spec("issue/concurrent", 2, now), 10, now)
            .await
            .unwrap();
        let other = JobStore::new(database.pool().clone());
        let (first, second) = tokio::join!(
            store.claim_next(now, TimeDelta::seconds(10)),
            other.claim_next(now, TimeDelta::seconds(10)),
        );
        let claims = [first.unwrap(), second.unwrap()];
        assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
        let claim = claims.into_iter().flatten().next().unwrap();
        assert_eq!(claim.job.id, accepted.job.id);
        assert_eq!(claim.job.attempts, 1);
        assert!(
            sqlx::query("UPDATE jobs SET payload_json = '{}' WHERE id = ?")
                .bind(&claim.job.id)
                .execute(database.pool())
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE jobs SET max_attempts = 20 WHERE id = ?")
                .bind(&claim.job.id)
                .execute(database.pool())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn claims_only_due_jobs_in_availability_order() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let future = store
            .enqueue(
                spec("issue/future", 2, now + TimeDelta::seconds(10)),
                10,
                now,
            )
            .await
            .unwrap();
        let soon = store
            .enqueue(spec("issue/soon", 2, now + TimeDelta::seconds(3)), 10, now)
            .await
            .unwrap();
        let ready = store
            .enqueue(spec("issue/ready", 2, now - TimeDelta::seconds(1)), 10, now)
            .await
            .unwrap();

        let first = store
            .claim_next(now, TimeDelta::seconds(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.job.id, ready.job.id);
        let second = store
            .claim_next(now + TimeDelta::seconds(3), TimeDelta::seconds(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.job.id, soon.job.id);
        assert!(
            store
                .claim_next(now + TimeDelta::seconds(9), TimeDelta::seconds(30))
                .await
                .unwrap()
                .is_none()
        );
        let third = store
            .claim_next(now + TimeDelta::seconds(10), TimeDelta::seconds(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third.job.id, future.job.id);
    }

    #[tokio::test]
    async fn enqueue_deduplicates_exact_work_and_enforces_capacity_across_concurrent_producers() {
        let (_directory, database) = database().await;
        let first_store = JobStore::new(database.pool().clone());
        let second_store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let (first, second) = tokio::join!(
            first_store.enqueue(spec("issue/capacity-a", 2, now), 1, now),
            second_store.enqueue(spec("issue/capacity-b", 2, now), 1, now),
        );
        let outcomes = [first, second];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(JobStoreError::Capacity)))
                .count(),
            1
        );
        let accepted = outcomes.into_iter().find_map(Result::ok).unwrap();
        assert!(accepted.inserted);

        let replay = first_store
            .enqueue(
                spec(&accepted.job.dedupe_key, 2, accepted.job.available_at),
                1,
                now,
            )
            .await
            .unwrap();
        assert!(!replay.inserted);
        assert_eq!(replay.job.id, accepted.job.id);

        let mut changed = spec(&accepted.job.dedupe_key, 2, accepted.job.available_at);
        changed.payload = serde_json::json!({"issue_id":"different"});
        assert!(matches!(
            first_store.enqueue(changed, 1, now).await,
            Err(JobStoreError::DeduplicationConflict)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs")
                .fetch_one(database.pool())
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn transient_retry_is_explicit_and_attempt_exhaustion_is_terminal() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let accepted = store
            .enqueue(spec("issue/retry", 2, now), 10, now)
            .await
            .unwrap();
        let first = store
            .claim_next(now, TimeDelta::seconds(30))
            .await
            .unwrap()
            .unwrap();
        let retry_at = now + TimeDelta::seconds(5);
        let scheduled = store
            .fail(
                &first,
                JobFailure::Transient {
                    code: "provider_busy".to_owned(),
                    retry_at,
                },
                now + TimeDelta::seconds(1),
            )
            .await
            .unwrap();
        assert_eq!(scheduled.state, JobState::Pending);
        assert_eq!(scheduled.available_at, retry_at);
        assert_eq!(scheduled.last_error_code.as_deref(), Some("provider_busy"));
        assert!(
            store
                .claim_next(retry_at - TimeDelta::seconds(1), TimeDelta::seconds(30))
                .await
                .unwrap()
                .is_none()
        );

        let second = store
            .claim_next(retry_at, TimeDelta::seconds(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.job.id, accepted.job.id);
        assert_eq!(second.job.attempts, 2);
        let failed_at = retry_at + TimeDelta::seconds(1);
        let exhausted = store
            .fail(
                &second,
                JobFailure::Transient {
                    code: "provider_busy".to_owned(),
                    retry_at: retry_at + TimeDelta::seconds(10),
                },
                failed_at,
            )
            .await
            .unwrap();
        assert_eq!(exhausted.state, JobState::Failed);
        assert_eq!(exhausted.finished_at, Some(failed_at));
        assert!(
            store
                .claim_next(retry_at + TimeDelta::seconds(20), TimeDelta::seconds(30))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn heartbeat_extends_a_live_lease_and_never_revives_an_expired_one() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let accepted = store
            .enqueue(spec("issue/heartbeat", 2, now), 10, now)
            .await
            .unwrap();
        let mut claim = store
            .claim_next(now, TimeDelta::seconds(2))
            .await
            .unwrap()
            .unwrap();
        store
            .heartbeat(
                &mut claim,
                now + TimeDelta::seconds(1),
                TimeDelta::seconds(5),
            )
            .await
            .unwrap();
        assert_eq!(
            claim.job.lease_expires_at,
            Some(now + TimeDelta::seconds(6))
        );
        let completed = store
            .succeed(&claim, now + TimeDelta::seconds(5))
            .await
            .unwrap();
        assert_eq!(completed.id, accepted.job.id);
        assert_eq!(completed.state, JobState::Succeeded);

        let later = now + TimeDelta::seconds(10);
        let other = store
            .enqueue(spec("issue/heartbeat-expired", 2, later), 10, later)
            .await
            .unwrap();
        let mut expired = store
            .claim_next(later, TimeDelta::seconds(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(expired.job.id, other.job.id);
        assert!(matches!(
            store
                .heartbeat(
                    &mut expired,
                    later + TimeDelta::seconds(1),
                    TimeDelta::seconds(10)
                )
                .await,
            Err(JobStoreError::StaleClaim)
        ));
    }

    #[tokio::test]
    async fn cancel_only_changes_pending_jobs_and_failed_completion_rolls_back() {
        let (_directory, database) = database().await;
        let store = JobStore::new(database.pool().clone());
        let now = Utc::now();
        let pending = store
            .enqueue(spec("issue/cancel", 2, now), 10, now)
            .await
            .unwrap();
        let canceled = store.cancel_pending(&pending.job.id, now).await.unwrap();
        assert_eq!(canceled.state, JobState::Canceled);
        assert!(matches!(
            store.cancel_pending(&pending.job.id, now).await,
            Err(JobStoreError::InvalidTransition)
        ));

        let running = store
            .enqueue(spec("issue/terminal-rollback", 2, now), 10, now)
            .await
            .unwrap();
        let claim = store
            .claim_next(now, TimeDelta::seconds(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.job.id, running.job.id);
        assert!(matches!(
            store.cancel_pending(&running.job.id, now).await,
            Err(JobStoreError::InvalidTransition)
        ));
        sqlx::query("CREATE TRIGGER reject_job_success BEFORE UPDATE OF state ON jobs WHEN NEW.state = 'succeeded' BEGIN SELECT RAISE(FAIL, 'fixture'); END")
            .execute(database.pool())
            .await
            .unwrap();
        assert!(
            store
                .succeed(&claim, now + TimeDelta::seconds(1))
                .await
                .is_err()
        );
        let after_rollback = store.get(&claim.job.id).await.unwrap().unwrap();
        assert_eq!(after_rollback.state, JobState::Running);
        assert_eq!(after_rollback.claim_epoch, claim.job.claim_epoch);
    }
}
