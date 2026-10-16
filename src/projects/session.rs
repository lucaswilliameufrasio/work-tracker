use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::auth::{
    SessionToken,
    session::{SessionError, SessionPolicy, SessionStore},
};

use super::{
    ProjectListOptions, ProjectListPage, ProjectOperationalPage, ProjectReadError, ProjectStore,
};

#[derive(Debug, Error)]
pub enum SessionProjectReadError {
    #[error("an active authenticated session is required")]
    Unauthenticated,
    #[error("session authentication failed: {0}")]
    Session(#[from] SessionError),
    #[error("project query failed: {0}")]
    Project(#[from] ProjectReadError),
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
