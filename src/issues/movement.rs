use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool, Transaction};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    auth::{
        SessionToken,
        master_key::MasterKey,
        session::{SessionError, SessionPolicy, SessionStore},
        time::format_timestamp,
    },
};

const RANK_STRIDE: i64 = 1_024;

#[derive(Debug, Error)]
pub enum IssueMoveError {
    #[error("an active session is required")]
    Unauthenticated,
    #[error("project, issue, status or neighbor is unavailable for this operation")]
    Unavailable,
    #[error("idempotency key must be a canonical UUID")]
    InvalidIdempotencyKey,
    #[error("expected version must be positive")]
    InvalidVersion,
    #[error("the current issue version differs from the expected version")]
    VersionConflict,
    #[error("the requested neighbors are no longer adjacent")]
    OrderConflict,
    #[error("idempotency key was used for a different request")]
    IdempotencyConflict,
    #[error("receipt fingerprint key is unavailable")]
    FingerprintKeyUnavailable,
    #[error("persisted issue or receipt data is invalid")]
    CorruptData,
    #[error("issue version is exhausted")]
    VersionExhausted,
    #[error("rank space is exhausted")]
    RankSpaceExhausted,
    #[error("mutation timestamp precedes current issue update")]
    TimeRegression,
    #[error("session validation failed: {0}")]
    Session(#[from] SessionError),
    #[error("persisted access failed: {0}")]
    Access(#[from] AccessStoreError),
    #[error("transaction failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("request encoding failed")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Clone, Copy)]
pub struct MoveIssue<'a> {
    pub expected_version: i64,
    pub target_status_id: &'a str,
    pub previous_issue_id: Option<&'a str>,
    pub next_issue_id: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueMoveReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub status_id: String,
    pub result_version: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueMoveOutcome {
    pub receipt: IssueMoveReceipt,
    pub replayed: bool,
}

struct RequestDigest(Sha256);

impl std::io::Write for RequestDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SessionIssueMover {
    pool: SqlitePool,
    sessions: SessionStore,
    master_key: MasterKey,
}

#[derive(Clone)]
struct Neighbor {
    id: String,
    rank: i64,
}

struct ReceiptInsert<'a> {
    actor: &'a str,
    key_digest: &'a [u8],
    fingerprint: &'a [u8],
    key_id: &'a [u8],
    project_id: &'a str,
    issue_id: &'a str,
    issue_number: i64,
    status_id: &'a str,
    result_version: i64,
    changed: bool,
    timestamp: &'a str,
}

enum ActorSource<'a> {
    Session(&'a SessionToken),
    User(&'a str),
}

impl SessionIssueMover {
    pub fn new(pool: SqlitePool, policy: SessionPolicy, master_key: MasterKey) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
            master_key,
        }
    }

    pub async fn move_issue(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: MoveIssue<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueMoveOutcome, IssueMoveError> {
        self.move_with(
            ActorSource::Session(token),
            project_id,
            issue_number,
            idempotency_key,
            input,
            now,
        )
        .await
    }

    /// Same move as a browser session, for a person already authenticated by
    /// Muira and authorized by a Puxiri grant. The user id is not a credential.
    pub async fn move_issue_for_user(
        &self,
        user_id: &str,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: MoveIssue<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueMoveOutcome, IssueMoveError> {
        self.move_with(
            ActorSource::User(user_id),
            project_id,
            issue_number,
            idempotency_key,
            input,
            now,
        )
        .await
    }

    async fn move_with(
        &self,
        source: ActorSource<'_>,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: MoveIssue<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueMoveOutcome, IssueMoveError> {
        if input.expected_version < 1 {
            return Err(IssueMoveError::InvalidVersion);
        }
        if issue_number < 1 {
            return Err(IssueMoveError::Unavailable);
        }
        for value in [
            Some(project_id),
            Some(input.target_status_id),
            input.previous_issue_id,
            input.next_issue_id,
        ]
        .into_iter()
        .flatten()
        {
            if !canonical_uuid(value) {
                return Err(IssueMoveError::Unavailable);
            }
        }
        let parsed_key =
            Uuid::parse_str(idempotency_key).map_err(|_| IssueMoveError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != idempotency_key {
            return Err(IssueMoveError::InvalidIdempotencyKey);
        }

        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project_id,
                issue_number,
                input.expected_version,
                input.target_status_id,
                input.previous_issue_id,
                input.next_issue_id,
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self
            .master_key
            .keyed_digest(b"issue.move request sha256-json v1", digest.as_slice());
        let key_id = self.master_key.key_id();

        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let actor_id = match source {
            ActorSource::User(user_id) => user_id.to_owned(),
            ActorSource::Session(token) => self
                .sessions
                .authenticate_in_transaction(&mut transaction, token, now)
                .await?
                .ok_or(IssueMoveError::Unauthenticated)?
                .user_id()
                .to_owned(),
        };
        let actor = actor_id.as_str();
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor,
            project_id,
            false,
        )
        .await?
        .ok_or(IssueMoveError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(IssueMoveError::Unavailable);
        }

        let issue = sqlx::query("SELECT issues.id, issues.issue_number, issues.status_id, issues.rank, issues.version, issues.completed_at, issues.updated_at, statuses.category AS current_category, projects.project_key FROM issues JOIN statuses ON statuses.project_id = issues.project_id AND statuses.id = issues.status_id JOIN projects ON projects.id = issues.project_id WHERE issues.project_id = ? AND issues.issue_number = ? AND issues.trashed_at IS NULL")
            .bind(project_id)
            .bind(issue_number)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(IssueMoveError::Unavailable)?;
        let issue_id: String = issue.try_get("id")?;
        let current_status_id: String = issue.try_get("status_id")?;
        let current_rank: i64 = issue.try_get("rank")?;
        let current_version: i64 = issue.try_get("version")?;
        let current_category: String = issue.try_get("current_category")?;
        let project_key: String = issue.try_get("project_key")?;
        if !canonical_uuid(&issue_id)
            || !canonical_uuid(&current_status_id)
            || current_version < 1
            || issue.try_get::<i64, _>("issue_number")? != issue_number
            || !(2..=6).contains(&project_key.len())
            || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
            || !valid_category(&current_category)
        {
            return Err(IssueMoveError::CorruptData);
        }
        let old_completed_at: Option<String> = issue.try_get("completed_at")?;
        validate_optional_timestamp(old_completed_at.as_deref())?;

        let mut hash = Sha256::new();
        hash.update(b"puxiri issue.move idempotency key v1\0");
        hash.update((actor.len() as u64).to_be_bytes());
        hash.update(actor.as_bytes());
        hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = hash.finalize().into();

        let prior = sqlx::query("SELECT request_fingerprint, master_key_id, project_id, issue_id, issue_number, result_status_id, result_version, changed FROM issue_move_receipts WHERE actor_user_id = ? AND operation = 'issue.move' AND key_digest = ?")
            .bind(actor)
            .bind(key_digest.as_slice())
            .fetch_optional(&mut **transaction)
            .await?;
        if let Some(prior) = prior {
            let stored_key: Vec<u8> = prior.try_get("master_key_id")?;
            if stored_key.as_slice() != key_id {
                return Err(IssueMoveError::FingerprintKeyUnavailable);
            }
            let stored_fingerprint: Vec<u8> = prior.try_get("request_fingerprint")?;
            if stored_fingerprint.len() != 32 {
                return Err(IssueMoveError::CorruptData);
            }
            if !bool::from(stored_fingerprint.as_slice().ct_eq(&fingerprint)) {
                return Err(IssueMoveError::IdempotencyConflict);
            }
            let result_status_id: String = prior.try_get("result_status_id")?;
            let result_version: i64 = prior.try_get("result_version")?;
            let changed: bool = prior.try_get("changed")?;
            let expected_result = if changed {
                input.expected_version.checked_add(1)
            } else {
                Some(input.expected_version)
            };
            if prior.try_get::<String, _>("project_id")? != project_id
                || prior.try_get::<String, _>("issue_id")? != issue_id
                || prior.try_get::<i64, _>("issue_number")? != issue_number
                || !canonical_uuid(&result_status_id)
                || expected_result != Some(result_version)
                || result_version > current_version
            {
                return Err(IssueMoveError::CorruptData);
            }
            let receipt = IssueMoveReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                status_id: result_status_id,
                result_version,
                changed,
            };
            transaction.commit().await?;
            return Ok(IssueMoveOutcome {
                receipt,
                replayed: true,
            });
        }

        if input.expected_version != current_version {
            return Err(IssueMoveError::VersionConflict);
        }
        if input
            .previous_issue_id
            .zip(input.next_issue_id)
            .is_some_and(|(previous, next)| previous == next)
        {
            return Err(IssueMoveError::OrderConflict);
        }

        let target_category: Option<String> = sqlx::query_scalar(
            "SELECT category FROM statuses WHERE project_id = ? AND id = ? AND board_visible = 1 AND archived_at IS NULL",
        )
        .bind(project_id)
        .bind(input.target_status_id)
        .fetch_optional(&mut **transaction)
        .await?;
        let target_category = target_category.ok_or(IssueMoveError::Unavailable)?;
        if !valid_category(&target_category) {
            return Err(IssueMoveError::CorruptData);
        }

        let previous = load_neighbor(
            &mut transaction,
            project_id,
            input.target_status_id,
            issue_id.as_str(),
            input.previous_issue_id,
        )
        .await?;
        let next = load_neighbor(
            &mut transaction,
            project_id,
            input.target_status_id,
            issue_id.as_str(),
            input.next_issue_id,
        )
        .await?;
        validate_adjacency(
            &mut transaction,
            project_id,
            input.target_status_id,
            issue_id.as_str(),
            previous.as_ref(),
            next.as_ref(),
        )
        .await?;

        let old_neighbors = if current_status_id == input.target_status_id {
            Some(
                issue_neighbors(
                    &mut transaction,
                    project_id,
                    &current_status_id,
                    &issue_id,
                    current_rank,
                )
                .await?,
            )
        } else {
            None
        };
        let same_position = old_neighbors
            .as_ref()
            .is_some_and(|(old_previous, old_next)| {
                old_previous.as_deref() == input.previous_issue_id
                    && old_next.as_deref() == input.next_issue_id
            });
        if same_position {
            let timestamp = format_timestamp(now);
            insert_receipt(
                &mut transaction,
                ReceiptInsert {
                    actor,
                    key_digest: key_digest.as_slice(),
                    fingerprint: fingerprint.as_slice(),
                    key_id: key_id.as_slice(),
                    project_id,
                    issue_id: &issue_id,
                    issue_number,
                    status_id: input.target_status_id,
                    result_version: current_version,
                    changed: false,
                    timestamp: &timestamp,
                },
            )
            .await?;
            transaction.commit().await?;
            return Ok(IssueMoveOutcome {
                receipt: IssueMoveReceipt {
                    issue_id,
                    project_id: project_id.to_owned(),
                    issue_number,
                    issue_key: format!("{project_key}-{issue_number}"),
                    status_id: input.target_status_id.to_owned(),
                    result_version: current_version,
                    changed: false,
                },
                replayed: false,
            });
        }

        let timestamp = format_timestamp(now);
        let updated_at: String = issue.try_get("updated_at")?;
        let parsed_updated_at = DateTime::parse_from_rfc3339(&updated_at)
            .map_err(|_| IssueMoveError::CorruptData)?
            .with_timezone(&Utc);
        if format_timestamp(parsed_updated_at) != updated_at {
            return Err(IssueMoveError::CorruptData);
        }
        if now < parsed_updated_at {
            return Err(IssueMoveError::TimeRegression);
        }
        let result_version = current_version
            .checked_add(1)
            .ok_or(IssueMoveError::VersionExhausted)?;
        let rank = choose_rank(previous.as_ref(), next.as_ref());
        let result_rank = match rank {
            Some(rank) => rank,
            None => {
                rebalance_target_column(
                    &mut transaction,
                    project_id,
                    input.target_status_id,
                    issue_id.as_str(),
                    previous.as_ref(),
                    next.as_ref(),
                )
                .await?
            }
        };
        let completed_at = if target_category == "done" {
            if current_category == "done" {
                old_completed_at.clone()
            } else {
                Some(timestamp.clone())
            }
        } else {
            None
        };
        let updated = sqlx::query("UPDATE issues SET status_id = ?, rank = ?, version = ?, completed_at = ?, updated_at = ? WHERE project_id = ? AND id = ? AND version = ? AND trashed_at IS NULL")
            .bind(input.target_status_id)
            .bind(result_rank)
            .bind(result_version)
            .bind(completed_at)
            .bind(&timestamp)
            .bind(project_id)
            .bind(&issue_id)
            .bind(current_version)
            .execute(&mut **transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(IssueMoveError::VersionConflict);
        }

        sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, 'issue.moved', ?)")
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(&timestamp)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, 'issue.moved', 1, ?, ?, ?, ?, ?)")
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(result_version)
            .bind(&timestamp)
            .execute(&mut **transaction)
            .await?;
        insert_receipt(
            &mut transaction,
            ReceiptInsert {
                actor,
                key_digest: key_digest.as_slice(),
                fingerprint: fingerprint.as_slice(),
                key_id: key_id.as_slice(),
                project_id,
                issue_id: &issue_id,
                issue_number,
                status_id: input.target_status_id,
                result_version,
                changed: true,
                timestamp: &timestamp,
            },
        )
        .await?;
        transaction.commit().await?;

        Ok(IssueMoveOutcome {
            receipt: IssueMoveReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                status_id: input.target_status_id.to_owned(),
                result_version,
                changed: true,
            },
            replayed: false,
        })
    }
}

async fn load_neighbor(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    status_id: &str,
    moving_issue_id: &str,
    selected_id: Option<&str>,
) -> Result<Option<Neighbor>, IssueMoveError> {
    let Some(selected_id) = selected_id else {
        return Ok(None);
    };
    if selected_id == moving_issue_id {
        return Err(IssueMoveError::OrderConflict);
    }
    let row = sqlx::query("SELECT id, rank FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id = ?")
        .bind(project_id)
        .bind(status_id)
        .bind(selected_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(IssueMoveError::OrderConflict)?;
    let id: String = row.try_get("id")?;
    if !canonical_uuid(&id) {
        return Err(IssueMoveError::CorruptData);
    }
    Ok(Some(Neighbor {
        id,
        rank: row.try_get("rank")?,
    }))
}

async fn validate_adjacency(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    status_id: &str,
    moving_issue_id: &str,
    previous: Option<&Neighbor>,
    next: Option<&Neighbor>,
) -> Result<(), IssueMoveError> {
    let between: i64 = match (previous, next) {
        (Some(previous), Some(next)) => {
            if (previous.rank, previous.id.as_str()) >= (next.rank, next.id.as_str()) {
                return Err(IssueMoveError::OrderConflict);
            }
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id != ? AND (rank, id) > (?, ?) AND (rank, id) < (?, ?))")
                .bind(project_id)
                .bind(status_id)
                .bind(moving_issue_id)
                .bind(previous.rank)
                .bind(&previous.id)
                .bind(next.rank)
                .bind(&next.id)
                .fetch_one(&mut **transaction)
                .await?
        }
        (Some(previous), None) => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id != ? AND (rank, id) > (?, ?))")
                .bind(project_id)
                .bind(status_id)
                .bind(moving_issue_id)
                .bind(previous.rank)
                .bind(&previous.id)
                .fetch_one(&mut **transaction)
                .await?
        }
        (None, Some(next)) => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id != ? AND (rank, id) < (?, ?))")
                .bind(project_id)
                .bind(status_id)
                .bind(moving_issue_id)
                .bind(next.rank)
                .bind(&next.id)
                .fetch_one(&mut **transaction)
                .await?
        }
        (None, None) => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id != ?)")
                .bind(project_id)
                .bind(status_id)
                .bind(moving_issue_id)
                .fetch_one(&mut **transaction)
                .await?
        }
    };
    if between == 0 {
        Ok(())
    } else {
        Err(IssueMoveError::OrderConflict)
    }
}

async fn issue_neighbors(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    status_id: &str,
    issue_id: &str,
    rank: i64,
) -> Result<(Option<String>, Option<String>), IssueMoveError> {
    let previous = sqlx::query_scalar("SELECT id FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND (rank, id) < (?, ?) ORDER BY rank DESC, id DESC LIMIT 1")
        .bind(project_id)
        .bind(status_id)
        .bind(rank)
        .bind(issue_id)
        .fetch_optional(&mut **transaction)
        .await?;
    let next = sqlx::query_scalar("SELECT id FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND (rank, id) > (?, ?) ORDER BY rank ASC, id ASC LIMIT 1")
        .bind(project_id)
        .bind(status_id)
        .bind(rank)
        .bind(issue_id)
        .fetch_optional(&mut **transaction)
        .await?;
    Ok((previous, next))
}

fn choose_rank(previous: Option<&Neighbor>, next: Option<&Neighbor>) -> Option<i64> {
    match (previous, next) {
        (Some(previous), Some(next)) => {
            let gap = i128::from(next.rank) - i128::from(previous.rank);
            (gap > 1).then(|| (i128::from(previous.rank) + gap / 2) as i64)
        }
        (Some(previous), None) => previous.rank.checked_add(1),
        (None, Some(next)) => next.rank.checked_sub(1),
        (None, None) => Some(RANK_STRIDE),
    }
}

async fn rebalance_target_column(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    status_id: &str,
    moving_issue_id: &str,
    previous: Option<&Neighbor>,
    next: Option<&Neighbor>,
) -> Result<i64, IssueMoveError> {
    let mut issue_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM issues WHERE project_id = ? AND status_id = ? AND trashed_at IS NULL AND id != ? ORDER BY rank ASC, id ASC")
        .bind(project_id)
        .bind(status_id)
        .bind(moving_issue_id)
        .fetch_all(&mut **transaction)
        .await?;
    let insertion_index = match (previous, next) {
        (Some(previous), _) => issue_ids
            .iter()
            .position(|id| id == &previous.id)
            .map(|index| index + 1)
            .ok_or(IssueMoveError::OrderConflict)?,
        (None, Some(next)) => issue_ids
            .iter()
            .position(|id| id == &next.id)
            .ok_or(IssueMoveError::OrderConflict)?,
        (None, None) if issue_ids.is_empty() => 0,
        (None, None) => return Err(IssueMoveError::OrderConflict),
    };
    if let Some(next) = next
        && issue_ids.get(insertion_index) != Some(&next.id)
    {
        return Err(IssueMoveError::OrderConflict);
    }
    issue_ids.insert(insertion_index, moving_issue_id.to_owned());

    let mut moving_rank = None;
    for (index, id) in issue_ids.iter().enumerate() {
        let position = i64::try_from(index + 1).map_err(|_| IssueMoveError::RankSpaceExhausted)?;
        let rank = position
            .checked_mul(RANK_STRIDE)
            .ok_or(IssueMoveError::RankSpaceExhausted)?;
        if id == moving_issue_id {
            moving_rank = Some(rank);
            continue;
        }
        let updated = sqlx::query("UPDATE issues SET rank = ? WHERE project_id = ? AND status_id = ? AND id = ? AND trashed_at IS NULL")
            .bind(rank)
            .bind(project_id)
            .bind(status_id)
            .bind(id)
            .execute(&mut **transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(IssueMoveError::OrderConflict);
        }
    }
    moving_rank.ok_or(IssueMoveError::CorruptData)
}

async fn insert_receipt(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    receipt: ReceiptInsert<'_>,
) -> Result<(), IssueMoveError> {
    sqlx::query("INSERT INTO issue_move_receipts (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, project_id, issue_id, issue_number, result_status_id, result_version, changed, created_at) VALUES (?, 'issue.move', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(receipt.actor)
        .bind(receipt.key_digest)
        .bind(receipt.fingerprint)
        .bind(receipt.key_id)
        .bind(receipt.project_id)
        .bind(receipt.issue_id)
        .bind(receipt.issue_number)
        .bind(receipt.status_id)
        .bind(receipt.result_version)
        .bind(receipt.changed)
        .bind(receipt.timestamp)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn valid_category(value: &str) -> bool {
    matches!(value, "backlog" | "active" | "done" | "canceled")
}

fn validate_optional_timestamp(value: Option<&str>) -> Result<(), IssueMoveError> {
    if let Some(value) = value {
        let parsed = DateTime::parse_from_rfc3339(value)
            .map_err(|_| IssueMoveError::CorruptData)?
            .with_timezone(&Utc);
        if format_timestamp(parsed) != value {
            return Err(IssueMoveError::CorruptData);
        }
    }
    Ok(())
}
