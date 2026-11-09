use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::time::format_timestamp;

const RECOVERY_CODE_BYTES: usize = 16;
const MAX_RECOVERY_CODES: u8 = 20;

#[derive(Debug, Error)]
pub enum RecoveryCodeError {
    #[error("recovery-code policy is invalid")]
    InvalidPolicy,
    #[error("secure recovery-code generation failed")]
    RandomnessFailed,
    #[error("user is not eligible for recovery codes")]
    UserNotActive,
    #[error("recovery-code persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Copy)]
pub struct RecoveryCodePolicy {
    code_count: u8,
}

impl RecoveryCodePolicy {
    pub fn new(code_count: u8) -> Result<Self, RecoveryCodeError> {
        if code_count == 0 || code_count > MAX_RECOVERY_CODES {
            return Err(RecoveryCodeError::InvalidPolicy);
        }
        Ok(Self { code_count })
    }
}

pub struct RecoveryCodeBatch {
    codes: Vec<Zeroizing<String>>,
}

impl RecoveryCodeBatch {
    /// One-time display values. Callers must not persist, log, or include them
    /// in URLs; discard the batch after presenting it to the authenticated user.
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.codes.iter().map(|code| code.as_str())
    }
}

struct GeneratedRecoveryCode {
    value: Zeroizing<String>,
    digest: [u8; 32],
}

#[derive(Clone)]
pub struct RecoveryCodeStore {
    pool: SqlitePool,
}

impl RecoveryCodeStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Rotates all unused codes and stores only their SHA-256 digests. The
    /// caller chooses the batch size; no count is specified by the plan.
    pub async fn issue(
        &self,
        user_id: &str,
        policy: RecoveryCodePolicy,
        now: DateTime<Utc>,
    ) -> Result<RecoveryCodeBatch, RecoveryCodeError> {
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let batch = self
            .issue_in_transaction(&mut transaction, user_id, policy, now)
            .await?;
        transaction.commit().await?;
        Ok(batch)
    }

    /// Rotates the active batch in a caller-owned `BEGIN IMMEDIATE` transaction.
    /// Recovery completion uses this to commit code rotation with factor
    /// replacement and recovery-token invalidation. The caller must roll back
    /// if any later operation fails.
    pub(crate) async fn issue_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        policy: RecoveryCodePolicy,
        now: DateTime<Utc>,
    ) -> Result<RecoveryCodeBatch, RecoveryCodeError> {
        let mut generated = Vec::with_capacity(usize::from(policy.code_count));
        for _ in 0..policy.code_count {
            generated.push(generate_code()?);
        }

        let now_text = format_timestamp(now);
        let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut **transaction)
            .await?;
        if state.as_deref() != Some("active") {
            return Err(RecoveryCodeError::UserNotActive);
        }

        sqlx::query(
            "UPDATE recovery_codes SET invalidated_at = ? \
             WHERE user_id = ? AND consumed_at IS NULL AND invalidated_at IS NULL",
        )
        .bind(&now_text)
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;

        for code in &generated {
            sqlx::query(
                "INSERT INTO recovery_codes \
                 (id, user_id, code_hash, created_at, consumed_at, invalidated_at) \
                 VALUES (?, ?, ?, ?, NULL, NULL)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(user_id)
            .bind(code.digest.as_slice())
            .bind(&now_text)
            .execute(&mut **transaction)
            .await?;
        }

        Ok(RecoveryCodeBatch {
            codes: generated.into_iter().map(|code| code.value).collect(),
        })
    }

    /// Consumes one code for the specified active user. Attempts should be
    /// rate-limited by the authentication caller. A successful consume must be
    /// included in the transaction that issues recovery access in product flow.
    pub async fn consume(
        &self,
        user_id: &str,
        value: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, RecoveryCodeError> {
        let Some(digest) = digest_candidate(value) else {
            return Ok(false);
        };
        let mut transaction = crate::db::admitted_write(&self.pool).await?;
        let consumed = self
            .consume_digest_in_transaction(&mut transaction, user_id, &digest, now)
            .await?;
        transaction.commit().await?;
        Ok(consumed)
    }

    /// Consumes a code inside a caller-owned `BEGIN IMMEDIATE` transaction so
    /// recovery access/session issuance can commit or roll back atomically with
    /// the one-time code. The caller must commit the transaction on success.
    pub async fn consume_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        value: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, RecoveryCodeError> {
        let Some(digest) = digest_candidate(value) else {
            return Ok(false);
        };
        self.consume_digest_in_transaction(transaction, user_id, &digest, now)
            .await
    }

    async fn consume_digest_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        digest: &[u8; 32],
        now: DateTime<Utc>,
    ) -> Result<bool, RecoveryCodeError> {
        let result = sqlx::query(
            "UPDATE recovery_codes SET consumed_at = ? \
             WHERE user_id = ? AND code_hash = ? \
               AND consumed_at IS NULL AND invalidated_at IS NULL \
               AND EXISTS (SELECT 1 FROM users \
                           WHERE users.id = recovery_codes.user_id \
                             AND users.state = 'active')",
        )
        .bind(format_timestamp(now))
        .bind(user_id)
        .bind(digest.as_slice())
        .execute(&mut **transaction)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

fn generate_code() -> Result<GeneratedRecoveryCode, RecoveryCodeError> {
    let mut random_bytes = Zeroizing::new([0_u8; RECOVERY_CODE_BYTES]);
    OsRng
        .try_fill_bytes(&mut *random_bytes)
        .map_err(|_| RecoveryCodeError::RandomnessFailed)?;
    let value = Zeroizing::new(URL_SAFE_NO_PAD.encode(random_bytes.as_slice()));
    let digest = Sha256::digest(value.as_bytes()).into();
    Ok(GeneratedRecoveryCode { value, digest })
}

fn digest_candidate(value: &str) -> Option<[u8; 32]> {
    if value.len() != 22
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(value).ok()?);
    if decoded.len() != RECOVERY_CODE_BYTES {
        return None;
    }
    let canonical = Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice()));
    if canonical.as_str() != value {
        return None;
    }
    Some(Sha256::digest(value.as_bytes()).into())
}
