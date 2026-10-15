use chrono::{DateTime, Utc};
use sqlx::Row;
use thiserror::Error;

use crate::access::{
    AccessCapability, AccessStore, AccessStoreError, AccountState, ProjectRole, authorize,
};

use super::{ProjectStore, validate_project_key};

#[derive(Debug, Error)]
pub enum ProjectReadError {
    #[error("project listing is unavailable for this account")]
    Unavailable,
    #[error("project-list cursor must be a valid project key")]
    InvalidCursor,
    #[error("project-list page size must be between 1 and 100")]
    InvalidPageSize,
    #[error("persisted project data is invalid")]
    CorruptData,
    #[error("persisted authorization state is invalid: {0}")]
    AccessState(#[from] AccessStoreError),
    #[error("project query failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct ProjectListOptions<'a> {
    pub after_project_key: Option<&'a str>,
    pub page_size: u16,
    pub include_archived: bool,
}

impl Default for ProjectListOptions<'_> {
    fn default() -> Self {
        Self {
            after_project_key: None,
            page_size: 50,
            include_archived: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSummary {
    pub id: String,
    pub project_key: String,
    pub name: String,
    pub role: ProjectRole,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectListPage {
    pub items: Vec<ProjectSummary>,
    pub next_after_project_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOperationalSummary {
    pub id: String,
    pub project_key: String,
    pub name: String,
    pub archived_at: Option<DateTime<Utc>>,
    pub membership_count: i64,
    pub issue_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOperationalPage {
    pub items: Vec<ProjectOperationalSummary>,
    pub next_after_project_key: Option<String>,
}

impl ProjectStore {
    /// Operational metadata only: owner/admin authority never implies issue
    /// content access. Counts include all retained memberships and issues.
    pub async fn list_operational_catalog(
        &self,
        actor_user_id: &str,
        options: ProjectListOptions<'_>,
    ) -> Result<ProjectOperationalPage, ProjectReadError> {
        validate_options(options)?;
        let mut transaction = self.pool.begin().await?;
        let actor = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?
        .ok_or(ProjectReadError::Unavailable)?;
        if authorize(&actor, AccessCapability::ReadProjectOperationalCatalog).is_err() {
            return Err(ProjectReadError::Unavailable);
        }
        let rows = sqlx::query(
            "SELECT projects.id, projects.project_key, projects.name, projects.archived_at, \
                    (SELECT COUNT(*) FROM project_members \
                     WHERE project_members.project_id = projects.id) AS membership_count, \
                    (SELECT COUNT(*) FROM issues \
                     WHERE issues.project_id = projects.id) AS issue_count \
             FROM projects WHERE projects.organization_id = 1 \
               AND (? OR projects.archived_at IS NULL) \
               AND projects.project_key > COALESCE(?, '') \
             ORDER BY projects.project_key ASC LIMIT ?",
        )
        .bind(options.include_archived)
        .bind(options.after_project_key)
        .bind(i64::from(options.page_size) + 1)
        .fetch_all(&mut *transaction)
        .await?;
        let has_more = rows.len() > usize::from(options.page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(options.page_size)));
        for row in rows.into_iter().take(usize::from(options.page_size)) {
            let project_key: String = row.try_get("project_key")?;
            let name: String = row.try_get("name")?;
            validate_project_fields(&project_key, &name)?;
            items.push(ProjectOperationalSummary {
                id: row.try_get("id")?,
                project_key,
                name,
                archived_at: parse_archive_timestamp(row.try_get("archived_at")?)?,
                membership_count: row.try_get("membership_count")?,
                issue_count: row.try_get("issue_count")?,
            });
        }
        let next_after_project_key = if has_more {
            items.last().map(|project| project.project_key.clone())
        } else {
            None
        };
        transaction.commit().await?;
        Ok(ProjectOperationalPage {
            items,
            next_after_project_key,
        })
    }

    /// Lists only projects with explicit current read access. The actor ID must
    /// originate from a validated session; organization/admin status alone never
    /// creates project-content access. Each call uses one authorization snapshot.
    pub async fn list_accessible(
        &self,
        actor_user_id: &str,
        options: ProjectListOptions<'_>,
    ) -> Result<ProjectListPage, ProjectReadError> {
        validate_options(options)?;

        let mut transaction = self.pool.begin().await?;
        let actor = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?
        .ok_or(ProjectReadError::Unavailable)?;
        if actor.account_state() != AccountState::Active || actor.organization_role().is_none() {
            return Err(ProjectReadError::Unavailable);
        }

        let rows = sqlx::query(
            "SELECT projects.id, projects.project_key, projects.name, projects.archived_at \
             FROM projects \
             JOIN project_members ON project_members.project_id = projects.id \
               AND project_members.organization_id = projects.organization_id \
             WHERE project_members.user_id = ? \
               AND projects.organization_id = 1 \
               AND (? OR projects.archived_at IS NULL) \
               AND projects.project_key > COALESCE(?, '') \
             ORDER BY projects.project_key ASC LIMIT ?",
        )
        .bind(actor_user_id)
        .bind(options.include_archived)
        .bind(options.after_project_key)
        .bind(i64::from(options.page_size) + 1)
        .fetch_all(&mut *transaction)
        .await?;

        let has_more = rows.len() > usize::from(options.page_size);
        let mut items = Vec::with_capacity(rows.len().min(usize::from(options.page_size)));
        for row in rows.into_iter().take(usize::from(options.page_size)) {
            let id: String = row.try_get("id")?;
            let context = AccessStore::load_project_context_in_transaction(
                &mut transaction,
                actor_user_id,
                &id,
                false,
            )
            .await?
            .ok_or(ProjectReadError::Unavailable)?;
            if authorize(&context, AccessCapability::ReadProjectContent).is_err() {
                return Err(ProjectReadError::Unavailable);
            }
            let role = context
                .project_role()
                .ok_or(ProjectReadError::Unavailable)?;
            let project_key: String = row.try_get("project_key")?;
            let name: String = row.try_get("name")?;
            validate_project_fields(&project_key, &name)?;
            let archived_at = parse_archive_timestamp(row.try_get("archived_at")?)?;
            items.push(ProjectSummary {
                id,
                project_key,
                name,
                role,
                archived_at,
            });
        }
        let next_after_project_key = if has_more {
            items.last().map(|project| project.project_key.clone())
        } else {
            None
        };
        transaction.commit().await?;
        Ok(ProjectListPage {
            items,
            next_after_project_key,
        })
    }
}

fn validate_options(options: ProjectListOptions<'_>) -> Result<(), ProjectReadError> {
    if !(1..=100).contains(&options.page_size) {
        return Err(ProjectReadError::InvalidPageSize);
    }
    if let Some(key) = options.after_project_key {
        validate_project_key(key).map_err(|_| ProjectReadError::InvalidCursor)?;
    }
    Ok(())
}

fn validate_project_fields(key: &str, name: &str) -> Result<(), ProjectReadError> {
    validate_project_key(key).map_err(|_| ProjectReadError::CorruptData)?;
    if name.trim().is_empty() {
        return Err(ProjectReadError::CorruptData);
    }
    Ok(())
}

fn parse_archive_timestamp(
    value: Option<String>,
) -> Result<Option<DateTime<Utc>>, ProjectReadError> {
    match value {
        Some(timestamp) => Ok(Some(
            DateTime::parse_from_rfc3339(&timestamp)
                .map_err(|_| ProjectReadError::CorruptData)?
                .with_timezone(&Utc),
        )),
        None => Ok(None),
    }
}
