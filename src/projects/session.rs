use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::access::{AccessCapability, AccessStore, authorize};
use crate::auth::{
    SessionToken,
    session::{SessionError, SessionPolicy, SessionStore},
};

use super::{
    ProjectListOptions, ProjectListPage, ProjectOperationalPage, ProjectReadError, ProjectStore,
    membership::{
        ProjectMembershipError, ProjectMembershipPage, ProjectMembershipRecord,
        ProjectMembershipStore,
    },
};

#[derive(Debug, Error)]
pub enum SessionProjectReadError {
    #[error("an active authenticated session is required")]
    Unauthenticated,
    #[error("session authentication failed: {0}")]
    Session(#[from] SessionError),
    #[error("project query failed: {0}")]
    Project(#[from] ProjectReadError),
    #[error("project membership query failed: {0}")]
    Membership(#[from] ProjectMembershipError),
    #[error("session-bound query transaction failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Request-facing queries derive identity from a session token, never input IDs.
/// Session identity, authorization and data all share one SQLite read snapshot.
#[derive(Clone)]
pub struct SessionProjectReader {
    pool: SqlitePool,
    sessions: SessionStore,
}

impl SessionProjectReader {
    pub async fn list_project_members(
        &self,
        token: &SessionToken,
        project_id: &str,
        after_user_id: Option<&str>,
        page_size: u16,
        now: DateTime<Utc>,
    ) -> Result<Option<ProjectMembershipPage>, SessionProjectReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionProjectReadError::Unauthenticated)?;
        let page = match ProjectMembershipStore::list_page_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            after_user_id,
            page_size,
        )
        .await
        {
            Ok(page) => Some(page),
            Err(ProjectMembershipError::Forbidden) => None,
            Err(error) => return Err(error.into()),
        };
        transaction.commit().await?;
        Ok(page)
    }

    pub async fn get_project_member(
        &self,
        token: &SessionToken,
        project_id: &str,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<ProjectMembershipRecord>, SessionProjectReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionProjectReadError::Unauthenticated)?;
        let member = match ProjectMembershipStore::get_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            user_id,
        )
        .await
        {
            Ok(member) => member,
            Err(ProjectMembershipError::Forbidden) => None,
            Err(error) => return Err(error.into()),
        };
        transaction.commit().await?;
        Ok(member)
    }

    pub async fn can_create_issues(
        &self,
        token: &SessionToken,
        project_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<bool>, SessionProjectReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionProjectReadError::Unauthenticated)?;
        let context = AccessStore::load_project_context_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            false,
        )
        .await
        .map_err(ProjectReadError::from)?;
        let capability = context.and_then(|context| {
            if authorize(&context, AccessCapability::ReadProjectContent).is_err() {
                return None;
            }
            Some(authorize(&context, AccessCapability::MutateIssues).is_ok())
        });
        transaction.commit().await?;
        Ok(capability)
    }

    pub fn new(pool: SqlitePool, policy: SessionPolicy) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
        }
    }

    pub async fn list_accessible(
        &self,
        token: &SessionToken,
        options: ProjectListOptions<'_>,
        now: DateTime<Utc>,
    ) -> Result<ProjectListPage, SessionProjectReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionProjectReadError::Unauthenticated)?;
        let page =
            ProjectStore::accessible_in_transaction(&mut transaction, principal.user_id(), options)
                .await?;
        transaction.commit().await?;
        Ok(page)
    }

    pub async fn list_operational_catalog(
        &self,
        token: &SessionToken,
        options: ProjectListOptions<'_>,
        now: DateTime<Utc>,
    ) -> Result<ProjectOperationalPage, SessionProjectReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionProjectReadError::Unauthenticated)?;
        let page = ProjectStore::operational_catalog_in_transaction(
            &mut transaction,
            principal.user_id(),
            options,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }
}
