use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::IssuePriority;
use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    auth::{
        SessionToken,
        master_key::MasterKey,
        session::{SessionError, SessionPolicy, SessionStore},
        time::format_timestamp,
    },
    organization_timezone::OrganizationTimezone,
};

#[derive(Debug, Error)]
pub enum IssueFieldError {
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
    #[error("organization timezone is unavailable")]
    OrganizationTimezoneUnavailable,
    #[error("selected user is not currently eligible for issue assignment")]
    AssigneeUnavailable,
    #[error("session validation failed: {0}")]
    Session(#[from] SessionError),
    #[error("persisted access failed: {0}")]
    Access(#[from] AccessStoreError),
    #[error("transaction failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("request encoding failed")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct SetIssuePriority {
    pub expected_version: i64,
    pub priority: IssuePriority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePriorityReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub priority: IssuePriority,
    pub result_version: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePriorityOutcome {
    pub receipt: IssuePriorityReceipt,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SetIssueDueDate {
    pub expected_version: i64,
    pub due_date: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDueDateReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub due_date: Option<NaiveDate>,
    pub result_version: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDueDateOutcome {
    pub receipt: IssueDueDateReceipt,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
pub struct SetIssueAssignee {
    pub expected_version: i64,
    pub assignee_user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueAssigneeReceipt {
    pub issue_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_key: String,
    pub assignee_user_id: Option<String>,
    pub result_version: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueAssigneeOutcome {
    pub receipt: IssueAssigneeReceipt,
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
pub struct SessionIssueFieldWriter {
    pool: SqlitePool,
    sessions: SessionStore,
    master_key: MasterKey,
}

impl SessionIssueFieldWriter {
    pub fn new(pool: SqlitePool, policy: SessionPolicy, master_key: MasterKey) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
            master_key,
        }
    }

    pub async fn set_priority(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: SetIssuePriority,
        now: DateTime<Utc>,
    ) -> Result<IssuePriorityOutcome, IssueFieldError> {
        if input.expected_version < 1 {
            return Err(IssueFieldError::InvalidVersion);
        }
        if issue_number < 1 || !canonical_uuid(project_id) {
            return Err(IssueFieldError::Unavailable);
        }
        let parsed_key =
            Uuid::parse_str(idempotency_key).map_err(|_| IssueFieldError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != idempotency_key {
            return Err(IssueFieldError::InvalidIdempotencyKey);
        }

        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project_id,
                issue_number,
                input.expected_version,
                priority_name(input.priority),
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self.master_key.keyed_digest(
            b"issue.set_priority request sha256-json v1",
            digest.as_slice(),
        );
        let key_id = self.master_key.key_id();

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(IssueFieldError::Unauthenticated)?;
        let actor = principal.user_id();
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor,
            project_id,
            false,
        )
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(IssueFieldError::Unavailable);
        }

        let row = sqlx::query(
            "SELECT issues.id, issues.issue_number, issues.priority, issues.version, \
                    issues.trashed_at, issues.updated_at, projects.project_key \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             WHERE issues.project_id = ? AND issues.issue_number = ?",
        )
        .bind(project_id)
        .bind(issue_number)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if row.try_get::<Option<String>, _>("trashed_at")?.is_some() {
            return Err(IssueFieldError::Unavailable);
        }
        let issue_id: String = row.try_get("id")?;
        let current_priority = parse_priority(&row.try_get::<String, _>("priority")?)?;
        let current_version: i64 = row.try_get("version")?;
        let project_key: String = row.try_get("project_key")?;
        if !canonical_uuid(&issue_id)
            || row.try_get::<i64, _>("issue_number")? != issue_number
            || current_version < 1
            || !(2..=6).contains(&project_key.len())
            || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(IssueFieldError::CorruptData);
        }

        let mut key_hash = Sha256::new();
        key_hash.update(b"puxiri issue.set_priority idempotency key v1\0");
        key_hash.update((actor.len() as u64).to_be_bytes());
        key_hash.update(actor.as_bytes());
        key_hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = key_hash.finalize().into();

        let prior = sqlx::query(
            "SELECT request_fingerprint, master_key_id, project_id, issue_id, \
                    issue_number, result_priority, result_version, changed \
             FROM issue_priority_receipts \
             WHERE actor_user_id = ? AND operation = 'issue.set_priority' \
               AND key_digest = ?",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(prior) = prior {
            let stored_key: Vec<u8> = prior.try_get("master_key_id")?;
            if stored_key.as_slice() != key_id {
                return Err(IssueFieldError::FingerprintKeyUnavailable);
            }
            let stored_fingerprint: Vec<u8> = prior.try_get("request_fingerprint")?;
            if stored_fingerprint.len() != 32 {
                return Err(IssueFieldError::CorruptData);
            }
            if !bool::from(stored_fingerprint.as_slice().ct_eq(&fingerprint)) {
                return Err(IssueFieldError::IdempotencyConflict);
            }
            let result_priority = parse_priority(&prior.try_get::<String, _>("result_priority")?)?;
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
                || result_priority != input.priority
                || expected_result != Some(result_version)
                || result_version > current_version
            {
                return Err(IssueFieldError::CorruptData);
            }
            let receipt = IssuePriorityReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                priority: result_priority,
                result_version,
                changed,
            };
            transaction.commit().await?;
            return Ok(IssuePriorityOutcome {
                receipt,
                replayed: true,
            });
        }

        if input.expected_version != current_version {
            return Err(IssueFieldError::VersionConflict);
        }

        let changed = current_priority != input.priority;
        let result_version = if changed {
            current_version
                .checked_add(1)
                .ok_or(IssueFieldError::VersionExhausted)?
        } else {
            current_version
        };
        let timestamp = format_timestamp(now);
        if changed {
            let updated_at: String = row.try_get("updated_at")?;
            let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|_| IssueFieldError::CorruptData)?
                .with_timezone(&Utc);
            if format_timestamp(updated_at) != row.try_get::<String, _>("updated_at")? {
                return Err(IssueFieldError::CorruptData);
            }
            if now < updated_at {
                return Err(IssueFieldError::TimeRegression);
            }

            let updated = sqlx::query(
                "UPDATE issues SET priority = ?, version = ?, updated_at = ? \
                 WHERE project_id = ? AND id = ? AND version = ? AND trashed_at IS NULL",
            )
            .bind(priority_name(input.priority))
            .bind(result_version)
            .bind(&timestamp)
            .bind(project_id)
            .bind(&issue_id)
            .bind(current_version)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(IssueFieldError::VersionConflict);
            }

            sqlx::query(
                "INSERT INTO issue_events \
                 (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
                  field_key, before_value_json, after_value_json) \
                 VALUES (?, ?, ?, ?, 'issue.field_updated', ?, 'priority', ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(&timestamp)
            .bind(serde_json::to_string(&priority_name(current_priority))?)
            .bind(serde_json::to_string(&priority_name(input.priority))?)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO outbox_events \
                 (id, event_kind, payload_version, project_id, issue_id, actor_user_id, \
                  issue_version, occurred_at) \
                 VALUES (?, 'issue.field_updated', 1, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(result_version)
            .bind(&timestamp)
            .execute(&mut *transaction)
            .await?;
        }

        sqlx::query(
            "INSERT INTO issue_priority_receipts \
             (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, \
              project_id, issue_id, issue_number, result_priority, result_version, \
              changed, created_at) \
             VALUES (?, 'issue.set_priority', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .bind(fingerprint.as_slice())
        .bind(key_id.as_slice())
        .bind(project_id)
        .bind(&issue_id)
        .bind(issue_number)
        .bind(priority_name(input.priority))
        .bind(result_version)
        .bind(changed)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(IssuePriorityOutcome {
            receipt: IssuePriorityReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                priority: input.priority,
                result_version,
                changed,
            },
            replayed: false,
        })
    }

    pub async fn set_due_date(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: SetIssueDueDate,
        now: DateTime<Utc>,
    ) -> Result<IssueDueDateOutcome, IssueFieldError> {
        if input.expected_version < 1 {
            return Err(IssueFieldError::InvalidVersion);
        }
        if issue_number < 1 || !canonical_uuid(project_id) {
            return Err(IssueFieldError::Unavailable);
        }
        let parsed_key =
            Uuid::parse_str(idempotency_key).map_err(|_| IssueFieldError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != idempotency_key {
            return Err(IssueFieldError::InvalidIdempotencyKey);
        }

        let input_date = input
            .due_date
            .map(|date| date.format("%Y-%m-%d").to_string());
        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project_id,
                issue_number,
                input.expected_version,
                input_date.as_deref(),
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self.master_key.keyed_digest(
            b"issue.set_due_date request sha256-json v1",
            digest.as_slice(),
        );
        let key_id = self.master_key.key_id();

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(IssueFieldError::Unauthenticated)?;
        let actor = principal.user_id();
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor,
            project_id,
            false,
        )
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(IssueFieldError::Unavailable);
        }

        let timezone: Option<String> =
            sqlx::query_scalar("SELECT timezone FROM organizations WHERE id = 1")
                .fetch_optional(&mut *transaction)
                .await?;
        let timezone = timezone.ok_or(IssueFieldError::OrganizationTimezoneUnavailable)?;
        let _timezone = OrganizationTimezone::parse(&timezone)
            .map_err(|_| IssueFieldError::OrganizationTimezoneUnavailable)?;

        let row = sqlx::query(
            "SELECT issues.id, issues.issue_number, issues.due_date, issues.version, \
                    issues.trashed_at, issues.updated_at, projects.project_key \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             WHERE issues.project_id = ? AND issues.issue_number = ?",
        )
        .bind(project_id)
        .bind(issue_number)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if row.try_get::<Option<String>, _>("trashed_at")?.is_some() {
            return Err(IssueFieldError::Unavailable);
        }
        let issue_id: String = row.try_get("id")?;
        let current_due_date = parse_optional_due_date(row.try_get("due_date")?)?;
        let current_version: i64 = row.try_get("version")?;
        let project_key: String = row.try_get("project_key")?;
        if !canonical_uuid(&issue_id)
            || row.try_get::<i64, _>("issue_number")? != issue_number
            || current_version < 1
            || !(2..=6).contains(&project_key.len())
            || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(IssueFieldError::CorruptData);
        }

        let mut key_hash = Sha256::new();
        key_hash.update(b"puxiri issue.set_due_date idempotency key v1\0");
        key_hash.update((actor.len() as u64).to_be_bytes());
        key_hash.update(actor.as_bytes());
        key_hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = key_hash.finalize().into();

        let prior = sqlx::query(
            "SELECT request_fingerprint, master_key_id, project_id, issue_id, \
                    issue_number, result_due_date, result_version, changed \
             FROM issue_due_date_receipts \
             WHERE actor_user_id = ? AND operation = 'issue.set_due_date' \
               AND key_digest = ?",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(prior) = prior {
            let stored_key: Vec<u8> = prior.try_get("master_key_id")?;
            if stored_key.as_slice() != key_id {
                return Err(IssueFieldError::FingerprintKeyUnavailable);
            }
            let stored_fingerprint: Vec<u8> = prior.try_get("request_fingerprint")?;
            if stored_fingerprint.len() != 32 {
                return Err(IssueFieldError::CorruptData);
            }
            if !bool::from(stored_fingerprint.as_slice().ct_eq(&fingerprint)) {
                return Err(IssueFieldError::IdempotencyConflict);
            }
            let result_due_date = parse_optional_due_date(prior.try_get("result_due_date")?)?;
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
                || result_due_date != input.due_date
                || expected_result != Some(result_version)
                || result_version > current_version
            {
                return Err(IssueFieldError::CorruptData);
            }
            let receipt = IssueDueDateReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                due_date: result_due_date,
                result_version,
                changed,
            };
            transaction.commit().await?;
            return Ok(IssueDueDateOutcome {
                receipt,
                replayed: true,
            });
        }

        if input.expected_version != current_version {
            return Err(IssueFieldError::VersionConflict);
        }

        let changed = current_due_date != input.due_date;
        let result_version = if changed {
            current_version
                .checked_add(1)
                .ok_or(IssueFieldError::VersionExhausted)?
        } else {
            current_version
        };
        let timestamp = format_timestamp(now);
        if changed {
            let updated_at: String = row.try_get("updated_at")?;
            let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|_| IssueFieldError::CorruptData)?
                .with_timezone(&Utc);
            if format_timestamp(updated_at) != row.try_get::<String, _>("updated_at")? {
                return Err(IssueFieldError::CorruptData);
            }
            if now < updated_at {
                return Err(IssueFieldError::TimeRegression);
            }

            let updated = sqlx::query(
                "UPDATE issues SET due_date = ?, version = ?, updated_at = ? \
                 WHERE project_id = ? AND id = ? AND version = ? AND trashed_at IS NULL",
            )
            .bind(&input_date)
            .bind(result_version)
            .bind(&timestamp)
            .bind(project_id)
            .bind(&issue_id)
            .bind(current_version)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(IssueFieldError::VersionConflict);
            }

            sqlx::query(
                "INSERT INTO issue_events \
                 (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
                  field_key, before_value_json, after_value_json) \
                 VALUES (?, ?, ?, ?, 'issue.field_updated', ?, 'due_date', ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(&timestamp)
            .bind(serde_json::to_string(
                &current_due_date.map(|date| date.format("%Y-%m-%d").to_string()),
            )?)
            .bind(serde_json::to_string(
                &input
                    .due_date
                    .map(|date| date.format("%Y-%m-%d").to_string()),
            )?)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO outbox_events \
                 (id, event_kind, payload_version, project_id, issue_id, actor_user_id, \
                  issue_version, occurred_at) \
                 VALUES (?, 'issue.field_updated', 1, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(result_version)
            .bind(&timestamp)
            .execute(&mut *transaction)
            .await?;
        }

        sqlx::query(
            "INSERT INTO issue_due_date_receipts \
             (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, \
              project_id, issue_id, issue_number, result_due_date, result_version, \
              changed, created_at) \
             VALUES (?, 'issue.set_due_date', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .bind(fingerprint.as_slice())
        .bind(key_id.as_slice())
        .bind(project_id)
        .bind(&issue_id)
        .bind(issue_number)
        .bind(&input_date)
        .bind(result_version)
        .bind(changed)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(IssueDueDateOutcome {
            receipt: IssueDueDateReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                due_date: input.due_date,
                result_version,
                changed,
            },
            replayed: false,
        })
    }

    pub async fn set_assignee(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        input: SetIssueAssignee,
        now: DateTime<Utc>,
    ) -> Result<IssueAssigneeOutcome, IssueFieldError> {
        if input.expected_version < 1 {
            return Err(IssueFieldError::InvalidVersion);
        }
        if issue_number < 1 || !canonical_uuid(project_id) {
            return Err(IssueFieldError::Unavailable);
        }
        if input
            .assignee_user_id
            .as_deref()
            .is_some_and(|user_id| !canonical_uuid(user_id))
        {
            return Err(IssueFieldError::AssigneeUnavailable);
        }
        let parsed_key =
            Uuid::parse_str(idempotency_key).map_err(|_| IssueFieldError::InvalidIdempotencyKey)?;
        if parsed_key.to_string() != idempotency_key {
            return Err(IssueFieldError::InvalidIdempotencyKey);
        }

        let mut encoded = RequestDigest(Sha256::new());
        serde_json::to_writer(
            &mut encoded,
            &(
                project_id,
                issue_number,
                input.expected_version,
                input.assignee_user_id.as_deref(),
            ),
        )?;
        let digest: [u8; 32] = encoded.0.finalize().into();
        let digest = Zeroizing::new(digest);
        let fingerprint = self.master_key.keyed_digest(
            b"issue.set_assignee request sha256-json v1",
            digest.as_slice(),
        );
        let key_id = self.master_key.key_id();

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(IssueFieldError::Unauthenticated)?;
        let actor = principal.user_id();
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor,
            project_id,
            false,
        )
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if authorize(&context, AccessCapability::MutateIssues).is_err() {
            return Err(IssueFieldError::Unavailable);
        }

        let row = sqlx::query(
            "SELECT issues.id, issues.issue_number, issues.assignee_user_id, \
                    issues.version, issues.trashed_at, issues.updated_at, projects.project_key \
             FROM issues \
             JOIN projects ON projects.id = issues.project_id \
             WHERE issues.project_id = ? AND issues.issue_number = ?",
        )
        .bind(project_id)
        .bind(issue_number)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IssueFieldError::Unavailable)?;
        if row.try_get::<Option<String>, _>("trashed_at")?.is_some() {
            return Err(IssueFieldError::Unavailable);
        }
        let issue_id: String = row.try_get("id")?;
        let current_assignee: Option<String> = row.try_get("assignee_user_id")?;
        let current_version: i64 = row.try_get("version")?;
        let project_key: String = row.try_get("project_key")?;
        if !canonical_uuid(&issue_id)
            || current_assignee
                .as_deref()
                .is_some_and(|user_id| !canonical_uuid(user_id))
            || row.try_get::<i64, _>("issue_number")? != issue_number
            || current_version < 1
            || !(2..=6).contains(&project_key.len())
            || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(IssueFieldError::CorruptData);
        }

        let mut key_hash = Sha256::new();
        key_hash.update(b"puxiri issue.set_assignee idempotency key v1\0");
        key_hash.update((actor.len() as u64).to_be_bytes());
        key_hash.update(actor.as_bytes());
        key_hash.update(parsed_key.as_bytes());
        let key_digest: [u8; 32] = key_hash.finalize().into();

        let prior = sqlx::query(
            "SELECT request_fingerprint, master_key_id, project_id, issue_id, \
                    issue_number, result_assignee_user_id, result_version, changed \
             FROM issue_assignee_receipts \
             WHERE actor_user_id = ? AND operation = 'issue.set_assignee' \
               AND key_digest = ?",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(prior) = prior {
            let stored_key: Vec<u8> = prior.try_get("master_key_id")?;
            if stored_key.as_slice() != key_id {
                return Err(IssueFieldError::FingerprintKeyUnavailable);
            }
            let stored_fingerprint: Vec<u8> = prior.try_get("request_fingerprint")?;
            if stored_fingerprint.len() != 32 {
                return Err(IssueFieldError::CorruptData);
            }
            if !bool::from(stored_fingerprint.as_slice().ct_eq(&fingerprint)) {
                return Err(IssueFieldError::IdempotencyConflict);
            }
            let result_assignee: Option<String> = prior.try_get("result_assignee_user_id")?;
            if result_assignee
                .as_deref()
                .is_some_and(|user_id| !canonical_uuid(user_id))
            {
                return Err(IssueFieldError::CorruptData);
            }
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
                || result_assignee != input.assignee_user_id
                || expected_result != Some(result_version)
                || result_version > current_version
            {
                return Err(IssueFieldError::CorruptData);
            }
            let receipt = IssueAssigneeReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                assignee_user_id: result_assignee,
                result_version,
                changed,
            };
            transaction.commit().await?;
            return Ok(IssueAssigneeOutcome {
                receipt,
                replayed: true,
            });
        }

        if input.expected_version != current_version {
            return Err(IssueFieldError::VersionConflict);
        }

        let changed = current_assignee != input.assignee_user_id;
        if changed && let Some(assignee_user_id) = input.assignee_user_id.as_deref() {
            let target_context = AccessStore::load_project_context_in_transaction(
                &mut transaction,
                assignee_user_id,
                project_id,
                false,
            )
            .await?
            .ok_or(IssueFieldError::AssigneeUnavailable)?;
            if authorize(&target_context, AccessCapability::MutateIssues).is_err() {
                return Err(IssueFieldError::AssigneeUnavailable);
            }
        }

        let result_version = if changed {
            current_version
                .checked_add(1)
                .ok_or(IssueFieldError::VersionExhausted)?
        } else {
            current_version
        };
        let timestamp = format_timestamp(now);
        if changed {
            let updated_at: String = row.try_get("updated_at")?;
            let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|_| IssueFieldError::CorruptData)?
                .with_timezone(&Utc);
            if format_timestamp(updated_at) != row.try_get::<String, _>("updated_at")? {
                return Err(IssueFieldError::CorruptData);
            }
            if now < updated_at {
                return Err(IssueFieldError::TimeRegression);
            }

            let updated = sqlx::query(
                "UPDATE issues SET assignee_user_id = ?, version = ?, updated_at = ? \
                 WHERE project_id = ? AND id = ? AND version = ? AND trashed_at IS NULL",
            )
            .bind(&input.assignee_user_id)
            .bind(result_version)
            .bind(&timestamp)
            .bind(project_id)
            .bind(&issue_id)
            .bind(current_version)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(IssueFieldError::VersionConflict);
            }

            sqlx::query(
                "INSERT INTO issue_events \
                 (id, project_id, issue_id, actor_user_id, event_kind, occurred_at, \
                  field_key, before_value_json, after_value_json) \
                 VALUES (?, ?, ?, ?, 'issue.field_updated', ?, 'assignee_user_id', ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(&timestamp)
            .bind(serde_json::to_string(&current_assignee)?)
            .bind(serde_json::to_string(&input.assignee_user_id)?)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO outbox_events \
                 (id, event_kind, payload_version, project_id, issue_id, actor_user_id, \
                  issue_version, occurred_at) \
                 VALUES (?, 'issue.field_updated', 1, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(project_id)
            .bind(&issue_id)
            .bind(actor)
            .bind(result_version)
            .bind(&timestamp)
            .execute(&mut *transaction)
            .await?;
        }

        sqlx::query(
            "INSERT INTO issue_assignee_receipts \
             (actor_user_id, operation, key_digest, request_fingerprint, master_key_id, \
              project_id, issue_id, issue_number, result_assignee_user_id, result_version, \
              changed, created_at) \
             VALUES (?, 'issue.set_assignee', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(actor)
        .bind(key_digest.as_slice())
        .bind(fingerprint.as_slice())
        .bind(key_id.as_slice())
        .bind(project_id)
        .bind(&issue_id)
        .bind(issue_number)
        .bind(&input.assignee_user_id)
        .bind(result_version)
        .bind(changed)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(IssueAssigneeOutcome {
            receipt: IssueAssigneeReceipt {
                issue_id,
                project_id: project_id.to_owned(),
                issue_number,
                issue_key: format!("{project_key}-{issue_number}"),
                assignee_user_id: input.assignee_user_id,
                result_version,
                changed,
            },
            replayed: false,
        })
    }
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && Uuid::parse_str(value)
            .map(|id| id.to_string() == value)
            .unwrap_or(false)
}

fn priority_name(priority: IssuePriority) -> &'static str {
    match priority {
        IssuePriority::None => "none",
        IssuePriority::Low => "low",
        IssuePriority::Medium => "medium",
        IssuePriority::High => "high",
        IssuePriority::Urgent => "urgent",
    }
}

fn parse_priority(value: &str) -> Result<IssuePriority, IssueFieldError> {
    match value {
        "none" => Ok(IssuePriority::None),
        "low" => Ok(IssuePriority::Low),
        "medium" => Ok(IssuePriority::Medium),
        "high" => Ok(IssuePriority::High),
        "urgent" => Ok(IssuePriority::Urgent),
        _ => Err(IssueFieldError::CorruptData),
    }
}

fn parse_optional_due_date(value: Option<String>) -> Result<Option<NaiveDate>, IssueFieldError> {
    value
        .as_deref()
        .map(|date| super::deadlines::parse_date(date).ok_or(IssueFieldError::CorruptData))
        .transpose()
}
