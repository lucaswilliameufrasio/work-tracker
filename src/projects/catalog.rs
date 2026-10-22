use chrono::{DateTime, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    auth::{
        SessionToken,
        session::{SessionError, SessionPolicy, SessionStore},
    },
};

#[derive(Clone, Copy)]
pub enum CatalogKind {
    Statuses,
    IssueTypes,
}

#[derive(Clone, Copy)]
pub struct CatalogOptions<'a> {
    pub after_id: Option<&'a str>,
    pub page_size: u16,
    pub include_archived: bool,
}

pub struct CatalogRecord {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub system_key: Option<String>,
    pub archived_at: Option<DateTime<Utc>>,
    pub fields: CatalogFields,
}

pub enum CatalogFields {
    Status {
        category: String,
        position: i64,
        board_visible: bool,
    },
    IssueType {
        icon_key: Option<String>,
        color_key: Option<String>,
    },
}

pub struct CatalogPage {
    pub items: Vec<CatalogRecord>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum CatalogReadError {
    #[error("an active authenticated session is required")]
    Unauthenticated,
    #[error("project is unavailable")]
    Unavailable,
    #[error("catalog cursor must be a canonical UUID")]
    InvalidCursor,
    #[error("catalog record ID must be a canonical UUID")]
    InvalidRecordId,
    #[error("page size must be between 1 and 100")]
    InvalidPageSize,
    #[error("catalog storage is corrupt")]
    CorruptData,
    #[error("session authentication failed: {0}")]
    Session(#[from] SessionError),
    #[error("persisted access state is invalid: {0}")]
    Access(#[from] AccessStoreError),
    #[error("catalog query failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct SessionProjectCatalogReader {
    pool: SqlitePool,
    sessions: SessionStore,
}

impl SessionProjectCatalogReader {
    pub async fn get(
        &self,
        token: &SessionToken,
        project_id: &str,
        kind: CatalogKind,
        record_id: &str,
        now: DateTime<Utc>,
    ) -> Result<CatalogRecord, CatalogReadError> {
        if !canonical_uuid(record_id) {
            return Err(CatalogReadError::InvalidRecordId);
        }
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(CatalogReadError::Unauthenticated)?;
        authorize_read(&mut transaction, principal.user_id(), project_id).await?;
        let query = match kind {
            CatalogKind::Statuses => {
                "SELECT id, project_id, name, system_key, archived_at, category, position, board_visible FROM statuses WHERE project_id = ? AND id = ?"
            }
            CatalogKind::IssueTypes => {
                "SELECT id, project_id, name, system_key, archived_at, icon_key, color_key FROM issue_types WHERE project_id = ? AND id = ?"
            }
        };
        let row = sqlx::query(query)
            .bind(project_id)
            .bind(record_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(CatalogReadError::Unavailable)?;
        let result = record(&row, kind)?;
        transaction.commit().await?;
        Ok(result)
    }
    pub fn new(pool: SqlitePool, policy: SessionPolicy) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
        }
    }

    pub async fn list(
        &self,
        token: &SessionToken,
        project_id: &str,
        kind: CatalogKind,
        options: CatalogOptions<'_>,
        now: DateTime<Utc>,
    ) -> Result<CatalogPage, CatalogReadError> {
        validate_options(options)?;
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(CatalogReadError::Unauthenticated)?;
        let page = list_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            kind,
            options,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }
}

fn validate_options(options: CatalogOptions<'_>) -> Result<(), CatalogReadError> {
    if !(1..=100).contains(&options.page_size) {
        return Err(CatalogReadError::InvalidPageSize);
    }
    if options.after_id.is_some_and(|id| !canonical_uuid(id)) {
        return Err(CatalogReadError::InvalidCursor);
    }
    Ok(())
}

async fn list_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    actor_user_id: &str,
    project_id: &str,
    kind: CatalogKind,
    options: CatalogOptions<'_>,
) -> Result<CatalogPage, CatalogReadError> {
    validate_options(options)?;
    authorize_read(transaction, actor_user_id, project_id).await?;
    let query = match kind {
        CatalogKind::Statuses => {
            "SELECT id, project_id, name, system_key, archived_at, category, position, board_visible FROM statuses WHERE project_id = ? AND id > ? AND (? OR archived_at IS NULL) ORDER BY id ASC LIMIT ?"
        }
        CatalogKind::IssueTypes => {
            "SELECT id, project_id, name, system_key, archived_at, icon_key, color_key FROM issue_types WHERE project_id = ? AND id > ? AND (? OR archived_at IS NULL) ORDER BY id ASC LIMIT ?"
        }
    };
    let rows = sqlx::query(query)
        .bind(project_id)
        .bind(options.after_id.unwrap_or(""))
        .bind(options.include_archived)
        .bind(i64::from(options.page_size) + 1)
        .fetch_all(&mut **transaction)
        .await?;
    let has_more = rows.len() > usize::from(options.page_size);
    let mut items = Vec::with_capacity(rows.len().min(usize::from(options.page_size)));
    for row in rows.into_iter().take(usize::from(options.page_size)) {
        items.push(record(&row, kind)?);
    }
    let next_after_id = if has_more {
        items.last().map(|item| item.id.clone())
    } else {
        None
    };
    Ok(CatalogPage {
        items,
        next_after_id,
    })
}

async fn authorize_read(
    transaction: &mut Transaction<'_, Sqlite>,
    actor_user_id: &str,
    project_id: &str,
) -> Result<(), CatalogReadError> {
    let context = AccessStore::load_project_context_in_transaction(
        transaction,
        actor_user_id,
        project_id,
        false,
    )
    .await?
    .ok_or(CatalogReadError::Unavailable)?;
    if authorize(&context, AccessCapability::ReadProjectContent).is_err() {
        return Err(CatalogReadError::Unavailable);
    }
    Ok(())
}

fn record(row: &SqliteRow, kind: CatalogKind) -> Result<CatalogRecord, CatalogReadError> {
    let id: String = row.try_get("id")?;
    let project_id: String = row.try_get("project_id")?;
    let name: String = row.try_get("name")?;
    if !canonical_uuid(&id) || !canonical_uuid(&project_id) || name.trim().is_empty() {
        return Err(CatalogReadError::CorruptData);
    }
    let system_key = key(row, "system_key")?;
    let archive: Option<String> = row.try_get("archived_at")?;
    let archived_at = archive
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|date| date.with_timezone(&Utc))
                .map_err(|_| CatalogReadError::CorruptData)
        })
        .transpose()?;
    let fields = match kind {
        CatalogKind::IssueTypes => CatalogFields::IssueType {
            icon_key: key(row, "icon_key")?,
            color_key: key(row, "color_key")?,
        },
        CatalogKind::Statuses => {
            let category: String = row.try_get("category")?;
            let position: i64 = row.try_get("position")?;
            let visible: i64 = row.try_get("board_visible")?;
            if !matches!(
                category.as_str(),
                "backlog" | "active" | "done" | "canceled"
            ) || position < 0
                || !matches!(visible, 0 | 1)
            {
                return Err(CatalogReadError::CorruptData);
            }
            CatalogFields::Status {
                category,
                position,
                board_visible: visible == 1,
            }
        }
    };
    Ok(CatalogRecord {
        id,
        project_id,
        name,
        system_key,
        archived_at,
        fields,
    })
}

fn key(row: &SqliteRow, column: &str) -> Result<Option<String>, CatalogReadError> {
    let value: Option<String> = row.try_get(column)?;
    if value.as_deref().is_some_and(|key| {
        key.is_empty()
            || key.len() > 48
            || !key.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    }) {
        return Err(CatalogReadError::CorruptData);
    }
    Ok(value)
}

pub(crate) fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::parse_str(value)
            .map(|id| id.to_string() == value)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::Database, projects::ProjectStore};
    use chrono::TimeDelta;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[tokio::test]
    async fn catalog_reads_share_auth_snapshot_and_new_snapshots_reject_revoked_or_removed_access()
    {
        let directory = TempDir::new().unwrap();
        let db = Database::open(directory.path()).await.unwrap();
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Catalog snapshot', 'en', 'UTC', 'now', 'now')").execute(db.pool()).await.unwrap();
        let owner = Uuid::now_v7().to_string();
        let viewer = Uuid::now_v7().to_string();
        for (id, role) in [(&owner, "owner"), (&viewer, "guest")] {
            sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Catalog snapshot', 'active', 'now', 'now')").bind(id).execute(db.pool()).await.unwrap();
            sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')").bind(id).bind(role).execute(db.pool()).await.unwrap();
        }
        let now = Utc::now();
        let project = ProjectStore::new(db.pool().clone())
            .create(&owner, "CAT", "Catalog snapshot", None, now)
            .await
            .unwrap();
        sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')").bind(&project.id).bind(&viewer).execute(db.pool()).await.unwrap();
        let policy = SessionPolicy::initial(TimeDelta::minutes(5)).unwrap();
        let sessions = SessionStore::new(db.pool().clone(), policy);
        let issued = sessions.issue(&viewer, now).await.unwrap();
        let mut snapshot = db.pool().begin().await.unwrap();
        let principal = sessions
            .authenticate_in_transaction(&mut snapshot, &issued.token, now)
            .await
            .unwrap()
            .unwrap();
        let mut writer = db.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE statuses SET name = 'After' WHERE project_id = ?")
            .bind(&project.id)
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE issue_types SET name = 'After' WHERE project_id = ?")
            .bind(&project.id)
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(&project.id)
            .bind(&viewer)
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ?")
            .bind(now.to_rfc3339())
            .bind(&issued.id)
            .execute(&mut *writer)
            .await
            .unwrap();
        writer.commit().await.unwrap();
        let options = CatalogOptions {
            after_id: None,
            page_size: 50,
            include_archived: false,
        };
        for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
            let page = list_in_transaction(
                &mut snapshot,
                principal.user_id(),
                &project.id,
                kind,
                options,
            )
            .await
            .unwrap();
            assert!(page.items.iter().all(|record| record.name != "After"));
        }
        snapshot.commit().await.unwrap();
        let reader = SessionProjectCatalogReader::new(db.pool().clone(), policy);
        for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
            assert!(matches!(
                reader
                    .list(&issued.token, &project.id, kind, options, now)
                    .await,
                Err(CatalogReadError::Unauthenticated)
            ));
        }
        let fresh = sessions.issue(&viewer, now).await.unwrap();
        for kind in [CatalogKind::Statuses, CatalogKind::IssueTypes] {
            assert!(matches!(
                reader
                    .list(&fresh.token, &project.id, kind, options, now)
                    .await,
                Err(CatalogReadError::Unavailable)
            ));
        }
        let own = sessions.issue(&owner, now).await.unwrap();
        reader
            .list(
                &own.token,
                &project.id,
                CatalogKind::Statuses,
                options,
                now + TimeDelta::minutes(10),
            )
            .await
            .unwrap();
        let activity: String =
            sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
                .bind(&own.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(activity, crate::auth::time::format_timestamp(now));
        assert!(matches!(
            reader
                .list(
                    &own.token,
                    &project.id,
                    CatalogKind::Statuses,
                    options,
                    now + TimeDelta::days(8)
                )
                .await,
            Err(CatalogReadError::Unauthenticated)
        ));
        sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
            .bind(&owner)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(matches!(
            reader
                .list(&own.token, &project.id, CatalogKind::Statuses, options, now)
                .await,
            Err(CatalogReadError::Unauthenticated)
        ));
        db.pool().close().await;
        assert!(matches!(
            reader
                .list(&own.token, &project.id, CatalogKind::Statuses, options, now)
                .await,
            Err(CatalogReadError::Database(_))
        ));
    }
}
