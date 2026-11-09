use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use super::{IssueStore, SqlitePool};
use crate::{
    access::{AccessCapability, AccessStore, ProjectRole, authorize},
    auth::time::format_timestamp,
};

const MAX_BODY_BYTES: usize = 65_536;

pub struct CommentEdit<'a> {
    pub actor_user_id: &'a str,
    pub project_id: &'a str,
    pub issue_number: i64,
    pub comment_id: &'a str,
    pub idempotency_key: &'a str,
    pub expected_version: i64,
    pub body: &'a str,
}

struct CommentWrite<'a> {
    actor_user_id: &'a str,
    project_id: &'a str,
    issue_number: i64,
    idempotency_key: &'a str,
    operation: &'static str,
    fingerprint: [u8; 32],
    now: chrono::DateTime<chrono::Utc>,
    write: Write,
}
const PAGE_LIMIT: i64 = 100;

#[derive(Debug, thiserror::Error)]
pub enum CommentError {
    #[error("the issue is not visible")]
    Unavailable,
    #[error("the actor cannot change this comment")]
    Forbidden,
    #[error("comment body is empty or longer than 64 KiB")]
    InvalidBody,
    #[error("the comment version is stale")]
    VersionConflict,
    #[error("the idempotency key was reused for a different request")]
    IdempotencyConflict,
    #[error("the idempotency key is not a canonical UUID")]
    InvalidIdempotencyKey,
    #[error("comment authorization state is invalid: {0}")]
    Access(#[from] crate::access::AccessStoreError),
    #[error("comment persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRecord {
    pub id: String,
    pub author_user_id: String,
    pub author_display_name: String,
    pub body: String,
    pub version: i64,
    pub can_edit: bool,
    pub can_delete: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentEventRecord {
    pub id: String,
    pub comment_id: String,
    pub actor_user_id: String,
    pub event_kind: String,
    pub occurred_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentPage {
    pub comments: Vec<CommentRecord>,
    pub events: Vec<CommentEventRecord>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentReceipt {
    pub comment_id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub result_version: Option<i64>,
    pub changed: bool,
    pub replayed: bool,
}

struct LoadedIssue {
    id: String,
    archived: bool,
    trashed: bool,
    role: Option<ProjectRole>,
    can_mutate: bool,
}

#[derive(Clone)]
pub struct CommentStore {
    pool: SqlitePool,
}

impl CommentStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list(
        &self,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
        cursor: Option<&str>,
    ) -> Result<CommentPage, CommentError> {
        if issue_number < 1 || !canonical_uuid(project_id) {
            return Err(CommentError::Unavailable);
        }
        if cursor.is_some_and(|value| !canonical_uuid(value)) {
            return Err(CommentError::InvalidBody);
        }
        let mut transaction = self.pool.begin().await?;
        let issue = load_issue(&mut transaction, actor_user_id, project_id, issue_number).await?;
        let cursor = cursor.unwrap_or("");
        let rows = sqlx::query(
            "SELECT issue_comments.id, issue_comments.author_user_id, users.display_name, \
                    issue_comments.body, issue_comments.version, issue_comments.created_at, \
                    issue_comments.updated_at \
             FROM issue_comments \
             JOIN users ON users.id = issue_comments.author_user_id \
             WHERE issue_comments.project_id = ? AND issue_comments.issue_id = ? \
               AND (? = '' OR issue_comments.id > ?) \
             ORDER BY issue_comments.id \
             LIMIT ?",
        )
        .bind(project_id)
        .bind(&issue.id)
        .bind(cursor)
        .bind(cursor)
        .bind(PAGE_LIMIT + 1)
        .fetch_all(&mut *transaction)
        .await?;
        let mut comments = Vec::new();
        for row in rows.iter().take(PAGE_LIMIT as usize) {
            let author: String = row.try_get("author_user_id")?;
            let (can_edit, can_delete) = permissions(actor_user_id, &author, &issue);
            comments.push(CommentRecord {
                id: row.try_get("id")?,
                author_user_id: author,
                author_display_name: row.try_get("display_name")?,
                body: row.try_get("body")?,
                version: row.try_get("version")?,
                can_edit,
                can_delete,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            });
        }
        let events_rows = sqlx::query(
            "SELECT id, comment_id, actor_user_id, event_kind, occurred_at \
             FROM issue_comment_events \
             WHERE project_id = ? AND issue_id = ? \
             ORDER BY id DESC \
             LIMIT ?",
        )
        .bind(project_id)
        .bind(&issue.id)
        .bind(PAGE_LIMIT)
        .fetch_all(&mut *transaction)
        .await?;
        let mut events = Vec::with_capacity(events_rows.len());
        for row in events_rows.iter().rev() {
            events.push(CommentEventRecord {
                id: row.try_get("id")?,
                comment_id: row.try_get("comment_id")?,
                actor_user_id: row.try_get("actor_user_id")?,
                event_kind: row.try_get("event_kind")?,
                occurred_at: row.try_get("occurred_at")?,
            });
        }
        transaction.commit().await?;
        let has_more = rows.len() > PAGE_LIMIT as usize;
        let next_cursor = if has_more {
            comments.last().map(|comment| comment.id.clone())
        } else {
            None
        };
        Ok(CommentPage {
            comments,
            events,
            has_more,
            next_cursor,
        })
    }

    pub async fn create(
        &self,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
        idempotency_key: &str,
        body: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<CommentReceipt, CommentError> {
        let body = validate_body(body)?;
        let fingerprint = fingerprint(&serde_json::json!([project_id, issue_number, body]))?;
        self.write(CommentWrite {
            actor_user_id,
            project_id,
            issue_number,
            idempotency_key,
            operation: "comment.create",
            fingerprint,
            now,
            write: Write::Create { body },
        })
        .await
    }

    pub async fn edit(
        &self,
        edit: CommentEdit<'_>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<CommentReceipt, CommentError> {
        if edit.expected_version < 1 || !canonical_uuid(edit.comment_id) {
            return Err(CommentError::Unavailable);
        }
        let body = validate_body(edit.body)?;
        let fingerprint = fingerprint(&serde_json::json!([
            edit.project_id,
            edit.issue_number,
            edit.comment_id,
            edit.expected_version,
            body
        ]))?;
        self.write(CommentWrite {
            actor_user_id: edit.actor_user_id,
            project_id: edit.project_id,
            issue_number: edit.issue_number,
            idempotency_key: edit.idempotency_key,
            operation: "comment.edit",
            fingerprint,
            now,
            write: Write::Edit {
                comment_id: edit.comment_id.to_owned(),
                expected_version: edit.expected_version,
                body,
            },
        })
        .await
    }

    pub async fn remove(
        &self,
        actor_user_id: &str,
        project_id: &str,
        issue_number: i64,
        comment_id: &str,
        idempotency_key: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<CommentReceipt, CommentError> {
        if !canonical_uuid(comment_id) {
            return Err(CommentError::Unavailable);
        }
        let fingerprint = fingerprint(&serde_json::json!([project_id, issue_number, comment_id]))?;
        self.write(CommentWrite {
            actor_user_id,
            project_id,
            issue_number,
            idempotency_key,
            operation: "comment.remove",
            fingerprint,
            now,
            write: Write::Remove {
                comment_id: comment_id.to_owned(),
            },
        })
        .await
    }

    async fn write(&self, input: CommentWrite<'_>) -> Result<CommentReceipt, CommentError> {
        let CommentWrite {
            actor_user_id,
            project_id,
            issue_number,
            idempotency_key,
            operation,
            fingerprint,
            now,
            write,
        } = input;
        let key = canonical_key(idempotency_key)?;
        if issue_number < 1 || !canonical_uuid(project_id) {
            return Err(CommentError::Unavailable);
        }
        let digest = key_digest(actor_user_id, operation, &key);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let issue = load_issue(&mut transaction, actor_user_id, project_id, issue_number).await?;
        if let Some(existing) = stored_receipt(
            &mut transaction,
            actor_user_id,
            operation,
            &digest,
            &fingerprint,
        )
        .await?
        {
            transaction.commit().await?;
            return Ok(CommentReceipt {
                comment_id: existing.comment_id,
                project_id: project_id.to_owned(),
                issue_number,
                result_version: existing.result_version,
                changed: existing.changed,
                replayed: true,
            });
        }
        let timestamp = format_timestamp(now);
        let outcome = match write {
            Write::Create { body } => {
                if !issue.can_mutate || issue.trashed {
                    return Err(CommentError::Forbidden);
                }
                let comment_id = Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO issue_comments \
                     (id, project_id, issue_id, author_user_id, body, version, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 1, ?, ?)",
                )
                .bind(&comment_id)
                .bind(project_id)
                .bind(&issue.id)
                .bind(actor_user_id)
                .bind(&body)
                .bind(&timestamp)
                .bind(&timestamp)
                .execute(&mut *transaction)
                .await?;
                insert_event(
                    &mut transaction,
                    project_id,
                    &issue.id,
                    &comment_id,
                    actor_user_id,
                    "comment.created",
                    &timestamp,
                )
                .await?;
                StoredReceipt {
                    comment_id,
                    result_version: Some(1),
                    changed: true,
                }
            }
            Write::Edit {
                comment_id,
                expected_version,
                body,
            } => {
                let current = load_comment(&mut transaction, project_id, &issue.id, &comment_id)
                    .await?
                    .ok_or(CommentError::Unavailable)?;
                if issue.archived || issue.trashed || current.author_user_id != actor_user_id {
                    return Err(CommentError::Forbidden);
                }
                if current.version != expected_version {
                    return Err(CommentError::VersionConflict);
                }
                let changed = current.body != body;
                let result_version = if changed {
                    expected_version + 1
                } else {
                    expected_version
                };
                if changed {
                    sqlx::query(
                        "UPDATE issue_comments SET body = ?, version = ?, updated_at = ? WHERE id = ?",
                    )
                    .bind(&body)
                    .bind(result_version)
                    .bind(&timestamp)
                    .bind(&comment_id)
                    .execute(&mut *transaction)
                    .await?;
                    insert_event(
                        &mut transaction,
                        project_id,
                        &issue.id,
                        &comment_id,
                        actor_user_id,
                        "comment.edited",
                        &timestamp,
                    )
                    .await?;
                }
                StoredReceipt {
                    comment_id,
                    result_version: Some(result_version),
                    changed,
                }
            }
            Write::Remove { comment_id } => {
                let current = load_comment(&mut transaction, project_id, &issue.id, &comment_id)
                    .await?
                    .ok_or(CommentError::Unavailable)?;
                let manager = issue.role == Some(ProjectRole::Manager);
                if issue.archived
                    || issue.trashed
                    || (current.author_user_id != actor_user_id && !manager)
                {
                    return Err(CommentError::Forbidden);
                }
                sqlx::query("DELETE FROM issue_comments WHERE id = ?")
                    .bind(&comment_id)
                    .execute(&mut *transaction)
                    .await?;
                insert_event(
                    &mut transaction,
                    project_id,
                    &issue.id,
                    &comment_id,
                    actor_user_id,
                    "comment.removed",
                    &timestamp,
                )
                .await?;
                StoredReceipt {
                    comment_id,
                    result_version: None,
                    changed: true,
                }
            }
        };
        sqlx::query(
            "INSERT INTO issue_comment_receipts \
             (actor_user_id, operation, key_digest, request_fingerprint, comment_id, \
              result_version, changed, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(actor_user_id)
        .bind(operation)
        .bind(digest.as_slice())
        .bind(fingerprint.as_slice())
        .bind(&outcome.comment_id)
        .bind(outcome.result_version)
        .bind(i64::from(outcome.changed))
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(CommentReceipt {
            comment_id: outcome.comment_id,
            project_id: project_id.to_owned(),
            issue_number,
            result_version: outcome.result_version,
            changed: outcome.changed,
            replayed: false,
        })
    }
}

enum Write {
    Create {
        body: String,
    },
    Edit {
        comment_id: String,
        expected_version: i64,
        body: String,
    },
    Remove {
        comment_id: String,
    },
}

struct CurrentComment {
    author_user_id: String,
    body: String,
    version: i64,
}

struct StoredReceipt {
    comment_id: String,
    result_version: Option<i64>,
    changed: bool,
}

async fn load_issue(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    project_id: &str,
    issue_number: i64,
) -> Result<LoadedIssue, CommentError> {
    let context = AccessStore::load_project_context_in_transaction(
        transaction,
        actor_user_id,
        project_id,
        false,
    )
    .await?
    .ok_or(CommentError::Unavailable)?;
    if authorize(&context, AccessCapability::ReadProjectContent).is_err() {
        return Err(CommentError::Unavailable);
    }
    let row = sqlx::query(
        "SELECT issues.id, issues.trashed_at, projects.archived_at \
         FROM issues \
         JOIN projects ON projects.id = issues.project_id \
         WHERE issues.project_id = ? AND issues.issue_number = ?",
    )
    .bind(project_id)
    .bind(issue_number)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(CommentError::Unavailable)?;
    let trashed: Option<String> = row.try_get("trashed_at")?;
    let archived: Option<String> = row.try_get("archived_at")?;
    Ok(LoadedIssue {
        id: row.try_get("id")?,
        archived: archived.is_some(),
        trashed: trashed.is_some(),
        role: context.project_role(),
        can_mutate: authorize(&context, AccessCapability::MutateIssues).is_ok(),
    })
}

async fn load_comment(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    issue_id: &str,
    comment_id: &str,
) -> Result<Option<CurrentComment>, CommentError> {
    let row = sqlx::query(
        "SELECT author_user_id, body, version FROM issue_comments \
         WHERE id = ? AND project_id = ? AND issue_id = ?",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(issue_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(CurrentComment {
        author_user_id: row.try_get("author_user_id")?,
        body: row.try_get("body")?,
        version: row.try_get("version")?,
    }))
}

async fn stored_receipt(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    operation: &str,
    digest: &[u8; 32],
    fingerprint: &[u8; 32],
) -> Result<Option<StoredReceipt>, CommentError> {
    let row = sqlx::query(
        "SELECT request_fingerprint, comment_id, result_version, changed \
         FROM issue_comment_receipts \
         WHERE actor_user_id = ? AND operation = ? AND key_digest = ?",
    )
    .bind(actor_user_id)
    .bind(operation)
    .bind(digest.as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored: Vec<u8> = row.try_get("request_fingerprint")?;
    if stored.as_slice() != fingerprint {
        return Err(CommentError::IdempotencyConflict);
    }
    let changed: i64 = row.try_get("changed")?;
    Ok(Some(StoredReceipt {
        comment_id: row.try_get("comment_id")?,
        result_version: row.try_get("result_version")?,
        changed: changed == 1,
    }))
}

async fn insert_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    issue_id: &str,
    comment_id: &str,
    actor_user_id: &str,
    event_kind: &str,
    timestamp: &str,
) -> Result<(), CommentError> {
    sqlx::query(
        "INSERT INTO issue_comment_events \
         (id, project_id, issue_id, comment_id, actor_user_id, event_kind, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(project_id)
    .bind(issue_id)
    .bind(comment_id)
    .bind(actor_user_id)
    .bind(event_kind)
    .bind(timestamp)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn permissions(actor: &str, author: &str, issue: &LoadedIssue) -> (bool, bool) {
    if issue.archived || issue.trashed {
        return (false, false);
    }
    let can_edit = actor == author;
    let can_delete = actor == author || issue.role == Some(ProjectRole::Manager);
    (can_edit, can_delete)
}

fn validate_body(body: &str) -> Result<String, CommentError> {
    let trimmed = body.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_BODY_BYTES
        || trimmed.chars().any(|character| {
            character.is_control() && character != '\n' && character != '\r' && character != '\t'
        })
    {
        return Err(CommentError::InvalidBody);
    }
    Ok(trimmed.to_owned())
}

fn canonical_key(value: &str) -> Result<String, CommentError> {
    let parsed = Uuid::parse_str(value).map_err(|_| CommentError::InvalidIdempotencyKey)?;
    let canonical = parsed.to_string();
    if canonical != value {
        return Err(CommentError::InvalidIdempotencyKey);
    }
    Ok(canonical)
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|id| id.to_string() == value)
        .unwrap_or(false)
}

fn key_digest(actor: &str, operation: &str, key: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"puxiri:issue-comment:v1:");
    hasher.update(actor.as_bytes());
    hasher.update([0]);
    hasher.update(operation.as_bytes());
    hasher.update([0]);
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

fn fingerprint(value: &serde_json::Value) -> Result<[u8; 32], CommentError> {
    let bytes = serde_json::to_vec(value).map_err(|_| CommentError::InvalidBody)?;
    Ok(Sha256::digest(bytes).into())
}

impl IssueStore {
    pub fn comments(&self) -> CommentStore {
        CommentStore::new(self.pool.clone())
    }
}
