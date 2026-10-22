use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::auth::{
    SessionToken,
    session::{SessionError, SessionPolicy, SessionStore},
};

use super::{
    IssueHistoryCursor, IssueHistoryPage, IssueListPage, IssueReadError, IssueRecord, IssueStore,
};

#[derive(Debug, Error)]
pub enum SessionIssueReadError {
    #[error("an active authenticated session is required")]
    Unauthenticated,
    #[error("session authentication failed: {0}")]
    Session(#[from] SessionError),
    #[error("issue query failed: {0}")]
    Issue(#[from] IssueReadError),
    #[error("session-bound query transaction failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Copy)]
pub struct IssueListOptions {
    pub after_issue_number: Option<i64>,
    pub page_size: u16,
}

#[derive(Clone)]
pub struct SessionIssueReader {
    pool: SqlitePool,
    sessions: SessionStore,
}

impl SessionIssueReader {
    pub async fn read_board_column(
        &self,
        token: &SessionToken,
        project_id: &str,
        status_id: &str,
        options: super::BoardOptions<'_>,
        now: DateTime<Utc>,
    ) -> Result<super::BoardColumnPage, SessionIssueReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionIssueReadError::Unauthenticated)?;
        let page = IssueStore::board_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            status_id,
            options,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }
    pub async fn get_project_issue(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        now: DateTime<Utc>,
    ) -> Result<IssueRecord, SessionIssueReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionIssueReadError::Unauthenticated)?;
        let issue = IssueStore::record_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            issue_number,
        )
        .await?;
        transaction.commit().await?;
        Ok(issue)
    }

    pub async fn list_history(
        &self,
        token: &SessionToken,
        project_id: &str,
        issue_number: i64,
        after: Option<&IssueHistoryCursor>,
        page_size: u16,
        now: DateTime<Utc>,
    ) -> Result<IssueHistoryPage, SessionIssueReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionIssueReadError::Unauthenticated)?;
        let page = IssueStore::history_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            issue_number,
            after,
            page_size,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }
    pub fn new(pool: SqlitePool, policy: SessionPolicy) -> Self {
        Self {
            sessions: SessionStore::new(pool.clone(), policy),
            pool,
        }
    }

    /// Identity, explicit membership and content share one read snapshot. The
    /// request supplies only resource/query selectors, never an actor identity.
    pub async fn list_project_issues(
        &self,
        token: &SessionToken,
        project_id: &str,
        options: IssueListOptions,
        now: DateTime<Utc>,
    ) -> Result<IssueListPage, SessionIssueReadError> {
        let mut transaction = self.pool.begin().await?;
        let principal = self
            .sessions
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
            .ok_or(SessionIssueReadError::Unauthenticated)?;
        let page = IssueStore::list_in_transaction(
            &mut transaction,
            principal.user_id(),
            project_id,
            options.after_issue_number,
            options.page_size,
        )
        .await?;
        transaction.commit().await?;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::Database, issues::NewIssue, projects::ProjectStore};
    use chrono::TimeDelta;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[tokio::test]
    async fn auth_membership_and_issue_helpers_share_snapshot_before_concurrent_revocation() {
        let directory = TempDir::new().unwrap();
        let db = Database::open(directory.path()).await.unwrap();
        sqlx::query("INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) VALUES (1, 'Snapshot fixture', 'en', 'UTC', 'now', 'now')").execute(db.pool()).await.unwrap();
        let owner = Uuid::now_v7().to_string();
        let viewer = Uuid::now_v7().to_string();
        for (id, role) in [(&owner, "owner"), (&viewer, "guest")] {
            sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Snapshot fixture', 'active', 'now', 'now')").bind(id).execute(db.pool()).await.unwrap();
            sqlx::query("INSERT INTO organization_members (organization_id, user_id, role, created_at) VALUES (1, ?, ?, 'now')").bind(id).bind(role).execute(db.pool()).await.unwrap();
        }
        let now = Utc::now();
        let project = ProjectStore::new(db.pool().clone())
            .create(&owner, "SNAP", "Snapshot", None, now)
            .await
            .unwrap();
        sqlx::query("INSERT INTO project_members (project_id, organization_id, user_id, role, created_at) VALUES (?, 1, ?, 'viewer', 'now')").bind(&project.id).bind(&viewer).execute(db.pool()).await.unwrap();
        let issue = IssueStore::new(db.pool().clone())
            .create(
                &owner,
                &project.id,
                NewIssue {
                    title: "Before",
                    description: None,
                    assignee_user_id: None,
                },
                now,
            )
            .await
            .unwrap();
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
        sqlx::query("UPDATE issues SET title = 'After', version = version + 1 WHERE id = ?")
            .bind(&issue.id)
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

        let old = IssueStore::list_in_transaction(
            &mut snapshot,
            principal.user_id(),
            &project.id,
            None,
            50,
        )
        .await
        .unwrap();
        assert_eq!(old.items[0].title, "Before");
        let old_detail = IssueStore::record_in_transaction(
            &mut snapshot,
            principal.user_id(),
            &project.id,
            issue.issue_number,
        )
        .await
        .unwrap();
        assert_eq!(old_detail.title, "Before");
        let board = IssueStore::board_in_transaction(
            &mut snapshot,
            principal.user_id(),
            &project.id,
            &issue.status_id,
            super::super::BoardOptions {
                after: None,
                page_size: 20,
            },
        )
        .await
        .unwrap();
        assert_eq!(board.total_count, 1);
        assert_eq!(board.items[0].title, "Before");
        let old_history = IssueStore::history_in_transaction(
            &mut snapshot,
            principal.user_id(),
            &project.id,
            issue.issue_number,
            None,
            50,
        )
        .await
        .unwrap();
        assert_eq!(old_history.items.len(), 1);
        snapshot.commit().await.unwrap();
        let reader = SessionIssueReader::new(db.pool().clone(), policy);
        assert!(matches!(
            reader
                .read_board_column(
                    &issued.token,
                    &project.id,
                    &issue.status_id,
                    super::super::BoardOptions {
                        after: None,
                        page_size: 20
                    },
                    now
                )
                .await,
            Err(SessionIssueReadError::Unauthenticated)
        ));
        assert!(matches!(
            reader
                .get_project_issue(&issued.token, &project.id, issue.issue_number, now)
                .await,
            Err(SessionIssueReadError::Unauthenticated)
        ));
        assert!(matches!(
            reader
                .list_history(
                    &issued.token,
                    &project.id,
                    issue.issue_number,
                    None,
                    50,
                    now
                )
                .await,
            Err(SessionIssueReadError::Unauthenticated)
        ));
        assert!(matches!(
            reader
                .list_project_issues(
                    &issued.token,
                    &project.id,
                    IssueListOptions {
                        after_issue_number: None,
                        page_size: 50
                    },
                    now
                )
                .await,
            Err(SessionIssueReadError::Unauthenticated)
        ));
        let new_token = sessions.issue(&viewer, now).await.unwrap();
        assert!(matches!(
            reader
                .list_project_issues(
                    &new_token.token,
                    &project.id,
                    IssueListOptions {
                        after_issue_number: None,
                        page_size: 50
                    },
                    now
                )
                .await,
            Err(SessionIssueReadError::Issue(IssueReadError::Unavailable))
        ));
        sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
            .bind(&viewer)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(matches!(
            reader
                .list_project_issues(
                    &new_token.token,
                    &project.id,
                    IssueListOptions {
                        after_issue_number: None,
                        page_size: 50
                    },
                    now
                )
                .await,
            Err(SessionIssueReadError::Unauthenticated)
        ));
    }
}
