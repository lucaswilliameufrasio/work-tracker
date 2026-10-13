use sqlx::{Row, SqlitePool, Transaction};
use thiserror::Error;

use crate::access::{AccessCapability, AccessStore, AccessStoreError, OrganizationRole, authorize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationDirectoryEntry {
    pub user_id: String,
    pub display_name: String,
    pub role: OrganizationRole,
    pub primary_email: Option<String>,
}

#[derive(Debug, Error)]
pub enum OrganizationDirectoryError {
    #[error("actor cannot read the organization directory")]
    Forbidden,
    #[error("persisted organization role is invalid")]
    CorruptRole,
    #[error("organization access context could not be loaded: {0}")]
    Access(#[from] AccessStoreError),
    #[error("organization directory query failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct OrganizationDirectoryStore {
    pool: SqlitePool,
}

impl OrganizationDirectoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Lists organization identities to active non-guest members. Only owners
    /// and admins receive confirmed primary email addresses. The actor ID
    /// must come from an already validated request session.
    pub async fn list(
        &self,
        actor_user_id: &str,
    ) -> Result<Vec<OrganizationDirectoryEntry>, OrganizationDirectoryError> {
        let mut transaction = self.pool.begin().await?;
        let Some(context) = AccessStore::load_organization_context_in_transaction(
            &mut transaction,
            actor_user_id,
            false,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Err(OrganizationDirectoryError::Forbidden);
        };
        if authorize(&context, AccessCapability::ReadOrganizationDirectory).is_err() {
            transaction.rollback().await?;
            return Err(OrganizationDirectoryError::Forbidden);
        }

        let can_read_email = matches!(
            context.organization_role(),
            Some(OrganizationRole::Owner | OrganizationRole::Admin)
        );
        let entries = list_on_transaction(&mut transaction, can_read_email).await?;
        transaction.commit().await?;
        Ok(entries)
    }
}

async fn list_on_transaction(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    can_read_email: bool,
) -> Result<Vec<OrganizationDirectoryEntry>, OrganizationDirectoryError> {
    let rows = sqlx::query(
        "SELECT organization_members.user_id, users.display_name, organization_members.role, \
                CASE WHEN ? = 1 THEN ( \
                    SELECT user_emails.address FROM user_emails \
                    WHERE user_emails.user_id = organization_members.user_id \
                      AND user_emails.is_primary = 1 \
                      AND user_emails.confirmed_at IS NOT NULL \
                    LIMIT 1 \
                ) ELSE NULL END AS primary_email \
         FROM organization_members \
         JOIN users ON users.id = organization_members.user_id \
         WHERE organization_members.organization_id = 1 \
         ORDER BY users.display_name COLLATE NOCASE, organization_members.user_id",
    )
    .bind(if can_read_email { 1_i64 } else { 0_i64 })
    .fetch_all(&mut **transaction)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let role = row.try_get::<String, _>("role")?;
        entries.push(OrganizationDirectoryEntry {
            user_id: row.try_get("user_id")?,
            display_name: row.try_get("display_name")?,
            role: parse_organization_role(&role)?,
            primary_email: row.try_get("primary_email")?,
        });
    }

    Ok(entries)
}

fn parse_organization_role(value: &str) -> Result<OrganizationRole, OrganizationDirectoryError> {
    match value {
        "owner" => Ok(OrganizationRole::Owner),
        "admin" => Ok(OrganizationRole::Admin),
        "member" => Ok(OrganizationRole::Member),
        "guest" => Ok(OrganizationRole::Guest),
        _ => Err(OrganizationDirectoryError::CorruptRole),
    }
}
