use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

pub mod membership;

use crate::{
    access::{AccessCapability, AccessStore, AccessStoreError, authorize},
    audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget},
    auth::time::format_timestamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectDefaultView {
    Board,
    List,
}

impl ProjectDefaultView {
    fn as_str(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::List => "list",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: String,
    pub project_key: String,
    pub name: String,
    pub description: Option<String>,
    pub default_view: ProjectDefaultView,
}

#[derive(Debug, Error)]
pub enum ProjectCreationError {
    #[error("project key must be 2 to 6 uppercase ASCII letters")]
    InvalidProjectKey,
    #[error("project name must not be empty")]
    InvalidProjectName,
    #[error("the authenticated actor does not exist")]
    UnknownActor,
    #[error("the authenticated actor is not allowed to create a project")]
    Forbidden,
    #[error("persisted organization locale is invalid")]
    CorruptOrganizationLocale,
    #[error("project key is already in use")]
    ProjectKeyUnavailable,
    #[error("current access state is invalid")]
    AccessState(#[from] AccessStoreError),
    #[error("project persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("project audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectArchiveChange {
    Changed,
    Unchanged,
}

#[derive(Debug, Error)]
pub enum ProjectArchiveError {
    #[error("project is missing or not available for this operation")]
    Unavailable,
    #[error("current access state is invalid")]
    AccessState(#[from] AccessStoreError),
    #[error("project archive persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("project archive audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

#[derive(Clone)]
pub struct ProjectStore {
    pool: SqlitePool,
}

impl ProjectStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Sets the desired archive state for an already-authenticated actor.
    /// Authorization, state and audit are evaluated in the same transaction.
    /// This does not grant content access or resume future automation rules.
    pub async fn set_archived(
        &self,
        actor_user_id: &str,
        project_id: &str,
        archived: bool,
        now: DateTime<Utc>,
    ) -> Result<ProjectArchiveChange, ProjectArchiveError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            actor_user_id,
            project_id,
            false,
        )
        .await?;
        let Some(context) = context else {
            transaction.rollback().await?;
            return Err(ProjectArchiveError::Unavailable);
        };
        if authorize(&context, AccessCapability::ChangeProjectArchive).is_err() {
            transaction.rollback().await?;
            return Err(ProjectArchiveError::Unavailable);
        }

        let timestamp = format_timestamp(now);
        let archived_at = if archived { Some(&timestamp) } else { None };
        let update = sqlx::query(
            "UPDATE projects SET archived_at = ?, updated_at = ? \
             WHERE id = ? AND (archived_at IS NOT NULL) != ?",
        )
        .bind(archived_at)
        .bind(&timestamp)
        .bind(project_id)
        .bind(archived)
        .execute(&mut *transaction)
        .await?;
        if update.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(ProjectArchiveChange::Unchanged);
        }

        let event_kind = if archived {
            AuditEventKind::ProjectArchived
        } else {
            AuditEventKind::ProjectReopened
        };
        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            event_kind,
            AuditEventResult::Success,
            &AuditEventTarget::project(project_id),
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(ProjectArchiveChange::Changed)
    }

    /// Creates a project and grants its creator the initial manager role.
    /// The caller must provide the user ID from an already-authenticated
    /// request; current account and organization roles are reloaded here.
    pub async fn create(
        &self,
        actor_user_id: &str,
        project_key: &str,
        name: &str,
        description: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<ProjectRecord, ProjectCreationError> {
        validate_project_key(project_key)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(ProjectCreationError::InvalidProjectName);
        }

        let project_id = Uuid::now_v7().to_string();
        let timestamp = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let access_context = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?;
        let Some(access_context) = access_context else {
            transaction.rollback().await?;
            return Err(ProjectCreationError::UnknownActor);
        };
        if authorize(&access_context, AccessCapability::CreateProject).is_err() {
            transaction.rollback().await?;
            return Err(ProjectCreationError::Forbidden);
        }

        let organization_locale: String =
            sqlx::query_scalar("SELECT locale FROM organizations WHERE id = 1")
                .fetch_one(&mut *transaction)
                .await?;
        let is_portuguese = match organization_locale.as_str() {
            "pt-BR" => true,
            "en" => false,
            _ => return Err(ProjectCreationError::CorruptOrganizationLocale),
        };

        let inserted = sqlx::query(
            "INSERT INTO projects \
             (id, organization_id, project_key, name, description, default_view, \
              archived_at, next_issue_number, created_at, updated_at) \
             VALUES (?, 1, ?, ?, ?, ?, NULL, 1, ?, ?) \
             ON CONFLICT (organization_id, project_key) DO NOTHING",
        )
        .bind(&project_id)
        .bind(project_key)
        .bind(name)
        .bind(description)
        .bind(ProjectDefaultView::Board.as_str())
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;

        if inserted.rows_affected() == 0 {
            transaction.rollback().await?;
            return Err(ProjectCreationError::ProjectKeyUnavailable);
        }

        sqlx::query(
            "INSERT INTO project_members \
             (project_id, organization_id, user_id, role, created_at) \
             VALUES (?, 1, ?, 'manager', ?)",
        )
        .bind(&project_id)
        .bind(actor_user_id)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;

        persist_initial_issue_schema(&mut transaction, &project_id, is_portuguese, &timestamp)
            .await?;

        audit::append_in_transaction(
            &mut transaction,
            Some(actor_user_id),
            AuditEventKind::ProjectCreated,
            AuditEventResult::Success,
            &AuditEventTarget::project(&project_id),
            now,
        )
        .await?;

        transaction.commit().await?;

        Ok(ProjectRecord {
            id: project_id,
            project_key: project_key.to_owned(),
            name: name.to_owned(),
            description: description.map(str::to_owned),
            default_view: ProjectDefaultView::Board,
        })
    }
}

async fn persist_initial_issue_schema(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
    is_portuguese: bool,
    timestamp: &str,
) -> Result<(), sqlx::Error> {
    let issue_type_name = if is_portuguese { "Tarefa" } else { "Task" };
    sqlx::query(
        "INSERT INTO issue_types \
         (id, project_id, system_key, name, icon_key, color_key, archived_at, \
          created_at, updated_at) \
         VALUES (?, ?, 'task', ?, NULL, NULL, NULL, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(project_id)
    .bind(issue_type_name)
    .bind(timestamp)
    .bind(timestamp)
    .execute(&mut **transaction)
    .await?;

    let statuses = if is_portuguese {
        [
            ("backlog", "Backlog", "backlog", 0_i64, false),
            ("todo", "A fazer", "backlog", 1, true),
            ("in_progress", "Em progresso", "active", 2, true),
            ("done", "Concluído", "done", 3, true),
            ("canceled", "Cancelado", "canceled", 4, false),
        ]
    } else {
        [
            ("backlog", "Backlog", "backlog", 0_i64, false),
            ("todo", "To do", "backlog", 1, true),
            ("in_progress", "In progress", "active", 2, true),
            ("done", "Done", "done", 3, true),
            ("canceled", "Canceled", "canceled", 4, false),
        ]
    };

    for (system_key, name, category, position, board_visible) in statuses {
        sqlx::query(
            "INSERT INTO statuses \
             (id, project_id, system_key, name, category, position, board_visible, \
              archived_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(project_id)
        .bind(system_key)
        .bind(name)
        .bind(category)
        .bind(position)
        .bind(if board_visible { 1_i64 } else { 0_i64 })
        .bind(timestamp)
        .bind(timestamp)
        .execute(&mut **transaction)
        .await?;
    }

    Ok(())
}

fn validate_project_key(project_key: &str) -> Result<(), ProjectCreationError> {
    if !(2..=6).contains(&project_key.len())
        || !project_key.bytes().all(|byte| byte.is_ascii_uppercase())
    {
        return Err(ProjectCreationError::InvalidProjectKey);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ProjectCreationError, validate_project_key};

    #[test]
    fn project_keys_are_two_to_six_uppercase_ascii_letters() {
        for valid in ["AB", "DEV", "ABCDEF"] {
            assert!(validate_project_key(valid).is_ok());
        }

        for invalid in ["A", "ABCDEFG", "dev", "A1", "ÉV", " A", "A ", ""] {
            assert!(matches!(
                validate_project_key(invalid),
                Err(ProjectCreationError::InvalidProjectKey)
            ));
        }
    }
}
