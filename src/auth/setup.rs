use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use thiserror::Error;

use super::{SessionToken, SessionTokenError, time::format_timestamp};

#[derive(Debug, Error)]
pub enum SetupTokenError {
    #[error("installation setup is already complete")]
    SetupComplete,
    #[error("secure setup token operation failed: {0}")]
    Token(#[from] SessionTokenError),
    #[error("setup token persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

pub struct SetupToken(SessionToken);

impl SetupToken {
    pub fn parse(value: &str) -> Result<Self, SessionTokenError> {
        SessionToken::parse(value).map(Self)
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.0.digest()
    }
}

#[derive(Clone)]
pub struct SetupTokenStore {
    pool: SqlitePool,
}

impl SetupTokenStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Issues a new local bootstrap token and invalidates any previous token.
    /// The caller is responsible for revealing the returned value only once to
    /// an interactive local operator.
    pub async fn issue(&self, now: DateTime<Utc>) -> Result<SetupToken, SetupTokenError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let completed_at = sqlx::query_scalar::<_, Option<String>>(
            "SELECT setup_completed_at FROM installation_state WHERE singleton = 1",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if completed_at.is_some() {
            transaction.rollback().await?;
            return Err(SetupTokenError::SetupComplete);
        }

        let token = SetupToken(SessionToken::generate()?);
        let token_hash = token.digest().to_vec();
        let issued_at = format_timestamp(now);
        sqlx::query(
            "INSERT INTO setup_bootstrap_tokens (singleton, token_hash, issued_at, consumed_at) \
             VALUES (1, ?, ?, NULL) \
             ON CONFLICT (singleton) DO UPDATE SET \
                 token_hash = excluded.token_hash, \
                 issued_at = excluded.issued_at, \
                 consumed_at = NULL",
        )
        .bind(token_hash)
        .bind(issued_at)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(token)
    }

    /// Checks whether a candidate is the current unconsumed setup token.
    /// Final setup must consume it in the same transaction that marks setup
    /// complete and creates the initial organization/admin.
    pub async fn is_current(&self, token: &SetupToken) -> Result<bool, SetupTokenError> {
        let matches: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM setup_bootstrap_tokens AS tokens \
                 JOIN installation_state AS state ON state.singleton = tokens.singleton \
                 WHERE tokens.singleton = 1 AND tokens.token_hash = ? \
                   AND tokens.consumed_at IS NULL \
                   AND state.setup_completed_at IS NULL \
             )",
        )
        .bind(token.digest().to_vec())
        .fetch_one(&self.pool)
        .await?;
        Ok(matches)
    }

    pub(crate) async fn consume_in_transaction(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        token_hash: &[u8],
        now: DateTime<Utc>,
    ) -> Result<(), SetupTokenError> {
        let now_text = format_timestamp(now);
        let consumed = sqlx::query(
            "UPDATE setup_bootstrap_tokens SET consumed_at = ? \
             WHERE singleton = 1 AND token_hash = ? AND consumed_at IS NULL",
        )
        .bind(&now_text)
        .bind(token_hash)
        .execute(&mut **transaction)
        .await?;
        if consumed.rows_affected() != 1 {
            return Err(SetupTokenError::SetupComplete);
        }
        let completed = sqlx::query(
            "UPDATE installation_state SET setup_completed_at = ? \
             WHERE singleton = 1 AND setup_completed_at IS NULL",
        )
        .bind(&now_text)
        .execute(&mut **transaction)
        .await?;
        if completed.rows_affected() != 1 {
            return Err(SetupTokenError::SetupComplete);
        }
        Ok(())
    }
}
