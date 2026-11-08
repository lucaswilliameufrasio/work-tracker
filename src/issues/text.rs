use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{IssueCreationError, NewIssue, validate_creation};
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
pub enum IssueTextError {
    #[error("an active session is required")]
    Unauthenticated,
    #[error("project or issue is unavailable for mutation")]
    Unavailable,
    #[error("idempotency key must be a canonical UUID")]
    InvalidIdempotencyKey,
    #[error("expected version must be positive")]
    InvalidVersion,
    #[error("current issue version differs from expected version")]
    VersionConflict,
    #[error("idempotency key was used for a different request")]
    IdempotencyConflict,
    #[error("receipt fingerprint key is unavailable")]
    FingerprintKeyUnavailable,
    #[error("persisted issue or receipt data is invalid")]
    CorruptData,
    #[error("issue version is exhausted")]
    VersionExhausted,
    #[error("mutation timestamp precedes current issue update")]
    TimeRegression,
    #[error("invalid text: {0}")]
    Validation(#[from] IssueCreationError),
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
pub struct ReplaceIssueText<'a> {
    pub expected_version: i64,
    pub title: &'a str,
    pub description: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTextReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub result_version: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTextOutcome {
    pub receipt: IssueTextReceipt,
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
pub struct SessionIssueTextWriter {
    pool: SqlitePool,
    sessions: SessionStore,
    master_key: MasterKey,
}

#[derive(Clone, Copy)]
enum TextTarget<'a> {
    Id(&'a str),
    Number(i64),
}

impl SessionIssueTextWriter {
    pub fn new(pool: SqlitePool, policy: SessionPolicy, master_key: MasterKey) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
            master_key,
        }
    }

    pub async fn replace(
        &self,
        token: &SessionToken,
        project: &str,
        issue: &str,
        key: &str,
        input: ReplaceIssueText<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueTextOutcome, IssueTextError> {
        self.replace_target(token, project, TextTarget::Id(issue), key, input, now)
            .await
    }

    pub async fn replace_by_number(
        &self,
        token: &SessionToken,
        project: &str,
        number: i64,
        key: &str,
        input: ReplaceIssueText<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueTextOutcome, IssueTextError> {
        self.replace_target(token, project, TextTarget::Number(number), key, input, now)
            .await
    }

    async fn replace_target(
        &self,
        token: &SessionToken,
        project: &str,
        target: TextTarget<'_>,
        key: &str,
        input: ReplaceIssueText<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssueTextOutcome, IssueTextError> {
        validate_creation(NewIssue {
            title: input.title,
            description: input.description,
            assignee_user_id: None,
        })?;
        if input.expected_version < 1 {
            return Err(IssueTextError::InvalidVersion);
        }
        let supplied_id = match target {
            TextTarget::Id(id) => Some(id),
            TextTarget::Number(number) => {
                if number < 1 {
                    return Err(IssueTextError::Unavailable);
                }
                None
            }
        };
        for target in [Some(project), supplied_id].into_iter().flatten() {
            if Uuid::parse_str(target)
                .map(|id| id.to_string())
                .ok()
                .as_deref()
                != Some(target)
            {
                return Err(IssueTextError::Unavailable);
            }
        }
        let parsed_key = Uuid::parse_str(key).map_err(|_| IssueTextError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != key {
            return Err(IssueTextError::InvalidIdempotencyKey);
        }
        let key_id = self.master_key.key_id();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut tx, token, now)
            .await?
            .ok_or(IssueTextError::Unauthenticated)?;
        let actor = principal.user_id();
        let context =
            AccessStore::load_project_context_in_transaction(&mut tx, actor, project, false)
                .await?
                .ok_or(IssueTextError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(IssueTextError::Unavailable);
        }
        let resolved_id = match target {
            TextTarget::Id(id) => id.to_owned(),
            TextTarget::Number(number) => sqlx::query_scalar::<_, String>(
                "SELECT id FROM issues WHERE project_id = ? AND issue_number = ?",
            )
            .bind(project)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(IssueTextError::Unavailable)?,
        };
        let issue = resolved_id.as_str();
        if Uuid::parse_str(issue)
            .map(|id| id.to_string())
            .ok()
            .as_deref()
            != Some(issue)
        {
            return Err(IssueTextError::CorruptData);
        }
        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project,
                issue,
                input.expected_version,
                input.title,
                input.description,
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self.master_key.keyed_digest(
            b"issue.replace_text request sha256-json v1",
            digest.as_slice(),
        );
        let row = sqlx::query("SELECT issues.issue_number, issues.version, issues.title, issues.description, issues.trashed_at, issues.updated_at, projects.project_key FROM issues JOIN projects ON projects.id = issues.project_id WHERE issues.project_id = ? AND issues.id = ?")
            .bind(project).bind(issue).fetch_optional(&mut *tx).await?.ok_or(IssueTextError::Unavailable)?;
        if row.try_get::<Option<String>, _>("trashed_at")?.is_some() {
            return Err(IssueTextError::Unavailable);
        }
        let number: i64 = row.try_get("issue_number")?;
        let current_version: i64 = row.try_get("version")?;
        let project_key: String = row.try_get("project_key")?;
        if number < 1
            || current_version < 1
            || !(2..=6).contains(&project_key.len())
            || !project_key.bytes().all(|b| b.is_ascii_uppercase())
        {
            return Err(IssueTextError::CorruptData);
        }
        let mut hash = Sha256::new();
        hash.update(b"puxiri issue.replace_text idempotency key v1\0");
        hash.update((actor.len() as u64).to_be_bytes());
        hash.update(actor.as_bytes());
        hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = hash.finalize().into();
        let prior = sqlx::query("SELECT request_fingerprint, master_key_id, project_id, issue_id, issue_number, result_version, changed FROM issue_text_receipts WHERE actor_user_id = ? AND operation = 'issue.replace_text' AND key_digest = ?")
            .bind(actor).bind(key_digest.as_slice()).fetch_optional(&mut *tx).await?;
        if let Some(prior) = prior {
            let stored_key: Vec<u8> = prior.try_get("master_key_id")?;
            if stored_key.as_slice() != key_id {
                return Err(IssueTextError::FingerprintKeyUnavailable);
            }
            let stored: Vec<u8> = prior.try_get("request_fingerprint")?;
            if stored.len() != 32 {
                return Err(IssueTextError::CorruptData);
            }
            if !bool::from(stored.as_slice().ct_eq(&fingerprint)) {
                return Err(IssueTextError::IdempotencyConflict);
            }
            let result_version: i64 = prior.try_get("result_version")?;
            let changed: bool = prior.try_get("changed")?;
            let expected_result = if changed {
                input.expected_version.checked_add(1)
            } else {
                Some(input.expected_version)
            };
            if prior.try_get::<String, _>("project_id")? != project
                || prior.try_get::<String, _>("issue_id")? != issue
                || prior.try_get::<i64, _>("issue_number")? != number
                || expected_result != Some(result_version)
                || result_version > current_version
            {
                return Err(IssueTextError::CorruptData);
            }
            let receipt = IssueTextReceipt {
                issue_id: issue.to_owned(),
                project_id: project.to_owned(),
                issue_number: number,
                issue_key: format!("{project_key}-{number}"),
                result_version,
                changed,
            };
            tx.commit().await?;
            return Ok(IssueTextOutcome {
                receipt,
                replayed: true,
            });
        }
        if input.expected_version != current_version {
            return Err(IssueTextError::VersionConflict);
        }
        let old_title: String = row.try_get("title")?;
        let old_description: Option<String> = row.try_get("description")?;
        let title_changed = old_title != input.title.trim();
        let description_changed = old_description.as_deref() != input.description;
        let changed = title_changed || description_changed;
        let result_version = if changed {
            current_version
                .checked_add(1)
                .ok_or(IssueTextError::VersionExhausted)?
        } else {
            current_version
        };
        let timestamp = format_timestamp(now);
        if changed {
            let updated: String = row.try_get("updated_at")?;
            let parsed = DateTime::parse_from_rfc3339(&updated)
                .map_err(|_| IssueTextError::CorruptData)?
                .with_timezone(&Utc);
            if format_timestamp(parsed) != updated {
                return Err(IssueTextError::CorruptData);
            }
            if now < parsed {
                return Err(IssueTextError::TimeRegression);
            }
            let updated = sqlx::query("UPDATE issues SET title = ?, description = ?, version = ?, updated_at = ? WHERE project_id = ? AND id = ? AND version = ? AND trashed_at IS NULL")
                .bind(input.title.trim()).bind(input.description).bind(result_version).bind(&timestamp).bind(project).bind(issue).bind(current_version).execute(&mut *tx).await?;
            if updated.rows_affected() != 1 {
                return Err(IssueTextError::VersionConflict);
            }
            let kind = match (title_changed, description_changed) {
                (true, false) => "issue.title_updated",
                (false, true) => "issue.description_updated",
                _ => "issue.text_updated",
            };
            sqlx::query("INSERT INTO issue_events (id, project_id, issue_id, actor_user_id, event_kind, occurred_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(Uuid::now_v7().to_string()).bind(project).bind(issue).bind(actor).bind(kind).bind(&timestamp).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO outbox_events (id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at) VALUES (?, ?, 1, ?, ?, ?, ?, ?)")
                .bind(Uuid::now_v7().to_string()).bind(kind).bind(project).bind(issue).bind(actor).bind(result_version).bind(&timestamp).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO issue_text_receipts (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, project_id, issue_id, issue_number, result_version, changed, created_at) VALUES (?, 'issue.replace_text', ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(actor).bind(key_digest.as_slice()).bind(fingerprint.as_slice()).bind(key_id.as_slice()).bind(project).bind(issue).bind(number).bind(result_version).bind(changed).bind(&timestamp).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(IssueTextOutcome {
            receipt: IssueTextReceipt {
                issue_id: issue.to_owned(),
                project_id: project.to_owned(),
                issue_number: number,
                issue_key: format!("{project_key}-{number}"),
                result_version,
                changed,
            },
            replayed: false,
        })
    }
}
