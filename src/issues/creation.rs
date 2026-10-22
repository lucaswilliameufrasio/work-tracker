use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{IssueCreationError, IssueStore, NewIssue, validate_creation};

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
use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    auth::{
        SessionToken,
        master_key::MasterKey,
        session::{SessionError, SessionPolicy, SessionStore},
        time::format_timestamp,
    },
};

#[derive(Debug, Error)]
pub enum SessionIssueCreationError {
    #[error("an active authenticated session is required")]
    Unauthenticated,
    #[error("project or issue is unavailable for this operation")]
    Unavailable,
    #[error("idempotency key must be a canonical UUID")]
    InvalidIdempotencyKey,
    #[error("idempotency key was already used for a different request")]
    IdempotencyConflict,
    #[error("receipt fingerprint key is unavailable")]
    FingerprintKeyUnavailable,
    #[error("persisted creation receipt is invalid")]
    CorruptReceipt,
    #[error("session authentication failed: {0}")]
    Session(#[from] SessionError),
    #[error("persisted access state is invalid: {0}")]
    Access(#[from] AccessStoreError),
    #[error("issue creation failed: {0}")]
    Creation(#[from] IssueCreationError),
    #[error("creation transaction failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("creation request encoding failed")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCreationReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub created_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueCreationOutcome {
    Created(IssueCreationReceipt),
    Replayed(IssueCreationReceipt),
}

impl IssueCreationOutcome {
    pub fn receipt(&self) -> &IssueCreationReceipt {
        match self {
            Self::Created(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// No ID-based authentication, publication or in-memory deduplication cache.
#[derive(Clone)]
pub struct SessionIssueWriter {
    pool: SqlitePool,
    sessions: SessionStore,
    master_key: MasterKey,
}

impl SessionIssueWriter {
    pub fn new(pool: SqlitePool, policy: SessionPolicy, master_key: MasterKey) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
            master_key,
        }
    }

    pub async fn create(
        &self,
        token: &SessionToken,
        project_id: &str,
        idempotency_key: &str,
        input: NewIssue<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueCreationOutcome, SessionIssueCreationError> {
        validate_creation(input)?;
        let parsed_key = Uuid::parse_str(idempotency_key)
            .map_err(|_| SessionIssueCreationError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != idempotency_key {
            return Err(SessionIssueCreationError::InvalidIdempotencyKey);
        }
        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project_id,
                input.title,
                input.description,
                input.assignee_user_id,
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self
            .master_key
            .keyed_digest(b"issue.create request sha256-json v1", digest.as_slice());
        let key_id = self.master_key.key_id();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionIssueCreationError::Unauthenticated)?;
        let actor = principal.user_id();
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor,
            project_id,
            false,
        )
        .await?
        .ok_or(SessionIssueCreationError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(SessionIssueCreationError::Unavailable);
        }
        let mut hash = Sha256::new();
        hash.update(b"puxiri issue.create idempotency key v1\0");
        hash.update((actor.len() as u64).to_be_bytes());
        hash.update(actor.as_bytes());
        hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = hash.finalize().into();
        let previous = sqlx::query("SELECT request_fingerprint, master_key_id, project_id, issue_id, issue_number FROM issue_creation_receipts WHERE actor_user_id = ? AND operation = 'issue.create' AND key_digest = ?")
            .bind(actor).bind(key_digest.as_slice()).fetch_optional(&mut *transaction).await?;
        if let Some(previous) = previous {
            let stored_key_id: Vec<u8> = previous.try_get("master_key_id")?;
            if stored_key_id.as_slice() != key_id {
                return Err(SessionIssueCreationError::FingerprintKeyUnavailable);
            }
            let stored_fingerprint: Vec<u8> = previous.try_get("request_fingerprint")?;
            if stored_fingerprint.len() != 32 {
                return Err(SessionIssueCreationError::CorruptReceipt);
            }
            if !bool::from(stored_fingerprint.as_slice().ct_eq(&fingerprint)) {
                return Err(SessionIssueCreationError::IdempotencyConflict);
            }
            let stored_project: String = previous.try_get("project_id")?;
            let issue_id: String = previous.try_get("issue_id")?;
            let issue_number: i64 = previous.try_get("issue_number")?;
            if stored_project != project_id
                || issue_number <= 0
                || Uuid::parse_str(&issue_id)
                    .map(|id| id.to_string())
                    .ok()
                    .as_deref()
                    != Some(issue_id.as_str())
            {
                return Err(SessionIssueCreationError::CorruptReceipt);
            }
            let project_key: Option<String> = sqlx::query_scalar("SELECT projects.project_key FROM issues JOIN projects ON projects.id = issues.project_id WHERE issues.id = ? AND issues.project_id = ? AND issues.issue_number = ? AND issues.creator_user_id = ?")
                .bind(&issue_id).bind(project_id).bind(issue_number).bind(actor).fetch_optional(&mut *transaction).await?;
            let project_key = project_key.ok_or(SessionIssueCreationError::Unavailable)?;
            if !(2..=6).contains(&project_key.len())
                || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
            {
                return Err(SessionIssueCreationError::CorruptReceipt);
            }
            let receipt = IssueCreationReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                created_version: 1,
            };
            transaction.commit().await?;
            return Ok(IssueCreationOutcome::Replayed(receipt));
        }
        let issue =
            IssueStore::create_in_transaction(&mut transaction, actor, project_id, input, now)
                .await?;
        sqlx::query("INSERT INTO issue_creation_receipts (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, project_id, issue_id, issue_number, created_at) VALUES (?, 'issue.create', ?, ?, ?, ?, ?, ?, ?)")
            .bind(actor).bind(key_digest.as_slice()).bind(fingerprint.as_slice()).bind(key_id.as_slice()).bind(project_id).bind(&issue.id).bind(issue.issue_number).bind(format_timestamp(now)).execute(&mut *transaction).await?;
        let receipt = IssueCreationReceipt {
            issue_id: issue.id,
            project_id: issue.project_id,
            issue_number: issue.issue_number,
            issue_key: issue.issue_key,
            created_version: 1,
        };
        transaction.commit().await?;
        Ok(IssueCreationOutcome::Created(receipt))
    }
}
