use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{
    EncryptedTotpSecret, TotpError, TotpPolicy, TotpSecret, TotpSecretBox, TotpSecretBoxError,
};
use crate::auth::time::format_timestamp;

const PENDING_ENROLLMENT_LIFETIME: TimeDelta = TimeDelta::minutes(10);

#[derive(Debug, Error)]
pub enum TotpCredentialError {
    #[error("user account is not active")]
    UserNotActive,
    #[error("user already has an active TOTP credential")]
    AlreadyEnabled,
    #[error("TOTP enrollment does not exist")]
    EnrollmentMissing,
    #[error("TOTP enrollment has expired")]
    EnrollmentExpired,
    #[error("stored TOTP credential is invalid")]
    InvalidCredential,
    #[error("TOTP enrollment deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("TOTP credential persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("TOTP audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
    #[error(transparent)]
    Totp(#[from] TotpError),
    #[error(transparent)]
    SecretBox(#[from] TotpSecretBoxError),
}

/// Secret material returned once by `begin_enrollment`; the encoded seed is
/// zeroized when this value is dropped and is intentionally not `Debug`.
pub struct TotpEnrollment {
    credential_id: Uuid,
    secret: Zeroizing<String>,
    expires_at: DateTime<Utc>,
}

impl TotpEnrollment {
    pub fn credential_id(&self) -> Uuid {
        self.credential_id
    }

    /// Return only to the authenticated setup response. Never log, cache, or
    /// place this seed in a URL.
    pub fn secret_for_display(&self) -> &str {
        self.secret.as_str()
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[derive(Clone)]
pub struct TotpCredentialStore {
    pool: SqlitePool,
    secret_box: TotpSecretBox,
}

impl TotpCredentialStore {
    pub fn new(pool: SqlitePool, secret_box: TotpSecretBox) -> Self {
        Self { pool, secret_box }
    }

    /// Starts or replaces a pending enrollment for an active account. This is
    /// an internal persistence API, not proof that the caller is that user.
    pub async fn begin_enrollment(
        &self,
        user_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<TotpEnrollment, TotpCredentialError> {
        let expires_at = now
            .checked_add_signed(PENDING_ENROLLMENT_LIFETIME)
            .ok_or(TotpCredentialError::DeadlineOutOfRange)?;
        let secret = TotpSecret::generate()?;
        let credential_id = Uuid::now_v7();
        let encrypted_secret = self.secret_box.encrypt(&user_id, &credential_id, &secret)?;
        let user_id_text = user_id.to_string();
        let credential_id_text = credential_id.to_string();
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let account_state: Option<String> =
            sqlx::query_scalar("SELECT state FROM users WHERE id = ?")
                .bind(&user_id_text)
                .fetch_optional(&mut *transaction)
                .await?;
        if account_state.as_deref() != Some("active") {
            transaction.rollback().await?;
            return Err(TotpCredentialError::UserNotActive);
        }

        let existing_activation: Option<Option<String>> =
            sqlx::query_scalar("SELECT activated_at FROM mfa_credentials WHERE user_id = ?")
                .bind(&user_id_text)
                .fetch_optional(&mut *transaction)
                .await?;
        if existing_activation.is_some_and(|activated_at| activated_at.is_some()) {
            transaction.rollback().await?;
            return Err(TotpCredentialError::AlreadyEnabled);
        }

        sqlx::query("DELETE FROM mfa_credentials WHERE user_id = ? AND activated_at IS NULL")
            .bind(&user_id_text)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO mfa_credentials \
             (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL)",
        )
        .bind(&credential_id_text)
        .bind(&user_id_text)
        .bind(encrypted_secret.as_bytes())
        .bind(&now_text)
        .bind(&expires_at_text)
        .execute(&mut *transaction)
        .await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(&user_id_text),
            AuditEventKind::TotpEnrollmentStarted,
            AuditEventResult::Success,
            &AuditEventTarget::mfa_credential(&credential_id_text),
            now,
        )
        .await?;
        transaction.commit().await?;

        Ok(TotpEnrollment {
            credential_id,
            secret: secret.reveal_for_enrollment(),
            expires_at,
        })
    }

    /// Confirms a pending enrollment. An incorrect code returns `Ok(false)`
    /// without changing the pending record; an expired enrollment is deleted
    /// and audited before returning `EnrollmentExpired`.
    pub async fn confirm_enrollment(
        &self,
        user_id: Uuid,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, TotpCredentialError> {
        let user_id_text = user_id.to_string();
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT mfa_credentials.id, mfa_credentials.secret_envelope, \
                    mfa_credentials.expires_at, mfa_credentials.activated_at, users.state \
             FROM mfa_credentials \
             JOIN users ON users.id = mfa_credentials.user_id \
             WHERE mfa_credentials.user_id = ?",
        )
        .bind(&user_id_text)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Err(TotpCredentialError::EnrollmentMissing);
        };
        let account_state: String = row.try_get("state")?;
        if account_state != "active" {
            transaction.rollback().await?;
            return Err(TotpCredentialError::UserNotActive);
        }
        let activation: Option<String> = row.try_get("activated_at")?;
        if activation.is_some() {
            transaction.rollback().await?;
            return Err(TotpCredentialError::AlreadyEnabled);
        }

        let credential_id_text: String = row.try_get("id")?;
        let credential_id = Uuid::parse_str(&credential_id_text)
            .map_err(|_| TotpCredentialError::InvalidCredential)?;
        let expires_at_text: String = row.try_get("expires_at")?;
        let expires_at = DateTime::parse_from_rfc3339(&expires_at_text)
            .map_err(|_| TotpCredentialError::InvalidCredential)?
            .with_timezone(&Utc);
        if expires_at <= now {
            let deleted = sqlx::query(
                "DELETE FROM mfa_credentials \
                 WHERE id = ? AND user_id = ? AND activated_at IS NULL",
            )
            .bind(&credential_id_text)
            .bind(&user_id_text)
            .execute(&mut *transaction)
            .await?;
            if deleted.rows_affected() != 1 {
                return Err(TotpCredentialError::InvalidCredential);
            }
            audit::append_in_transaction(
                &mut transaction,
                Some(&user_id_text),
                AuditEventKind::TotpEnrollmentExpired,
                AuditEventResult::Success,
                &AuditEventTarget::mfa_credential(&credential_id_text),
                now,
            )
            .await?;
            transaction.commit().await?;
            return Err(TotpCredentialError::EnrollmentExpired);
        }

        let envelope_bytes: Vec<u8> = row.try_get("secret_envelope")?;
        let envelope = EncryptedTotpSecret::from_bytes(&envelope_bytes)?;
        let secret = self
            .secret_box
            .decrypt(&user_id, &credential_id, &envelope)?;
        let matched_step =
            secret.matching_step(code, now, None, TotpPolicy::authenticator_default());
        let Some(matched_step) = matched_step else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let matched_step =
            i64::try_from(matched_step).map_err(|_| TotpCredentialError::InvalidCredential)?;
        let updated = sqlx::query(
            "UPDATE mfa_credentials \
             SET expires_at = NULL, activated_at = ?, last_accepted_step = ? \
             WHERE id = ? AND user_id = ? AND activated_at IS NULL AND expires_at > ?",
        )
        .bind(&now_text)
        .bind(matched_step)
        .bind(&credential_id_text)
        .bind(&user_id_text)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(TotpCredentialError::InvalidCredential);
        }
        audit::append_in_transaction(
            &mut transaction,
            Some(&user_id_text),
            AuditEventKind::TotpEnrollmentConfirmed,
            AuditEventResult::Success,
            &AuditEventTarget::mfa_credential(&credential_id_text),
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(true)
    }

    /// Verifies and consumes a newer TOTP step inside the caller's transaction.
    /// The caller must use `BEGIN IMMEDIATE` and commit this counter together
    /// with any protected mutation. Roll back the caller transaction on `Err`.
    /// A successful result grants no authority by itself and is not
    /// session-bound proof.
    pub async fn verify_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: Uuid,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, TotpCredentialError> {
        let user_id_text = user_id.to_string();
        let row = sqlx::query(
            "SELECT mfa_credentials.id, mfa_credentials.secret_envelope, \
                    mfa_credentials.last_accepted_step \
             FROM mfa_credentials \
             JOIN users ON users.id = mfa_credentials.user_id \
             WHERE mfa_credentials.user_id = ? \
               AND mfa_credentials.activated_at IS NOT NULL \
               AND users.state = 'active'",
        )
        .bind(&user_id_text)
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };

        let credential_id_text: String = row.try_get("id")?;
        let credential_id = Uuid::parse_str(&credential_id_text)
            .map_err(|_| TotpCredentialError::InvalidCredential)?;
        let envelope_bytes: Vec<u8> = row.try_get("secret_envelope")?;
        let envelope = EncryptedTotpSecret::from_bytes(&envelope_bytes)?;
        let secret = self
            .secret_box
            .decrypt(&user_id, &credential_id, &envelope)?;
        let last_accepted_step_db: i64 = row.try_get("last_accepted_step")?;
        let last_accepted_step = u64::try_from(last_accepted_step_db)
            .map_err(|_| TotpCredentialError::InvalidCredential)?;
        let Some(matched_step) = secret.matching_step(
            code,
            now,
            Some(last_accepted_step),
            TotpPolicy::authenticator_default(),
        ) else {
            return Ok(false);
        };
        let matched_step =
            i64::try_from(matched_step).map_err(|_| TotpCredentialError::InvalidCredential)?;
        let updated = sqlx::query(
            "UPDATE mfa_credentials SET last_accepted_step = ? \
             WHERE id = ? AND user_id = ? AND activated_at IS NOT NULL \
               AND last_accepted_step = ? \
               AND EXISTS (SELECT 1 FROM users \
                           WHERE users.id = mfa_credentials.user_id \
                             AND users.state = 'active')",
        )
        .bind(matched_step)
        .bind(&credential_id_text)
        .bind(&user_id_text)
        .bind(last_accepted_step_db)
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Ok(false);
        }
        audit::append_in_transaction(
            transaction,
            Some(&user_id_text),
            AuditEventKind::TotpVerified,
            AuditEventResult::Success,
            &AuditEventTarget::mfa_credential(&credential_id_text),
            now,
        )
        .await?;
        Ok(true)
    }
}
