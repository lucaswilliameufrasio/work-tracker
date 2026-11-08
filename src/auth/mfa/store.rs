use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::{
    EncryptedTotpSecret, TotpError, TotpPolicy, TotpSecret, TotpSecretBox, TotpSecretBoxError,
    step_up::RecentMfaProof,
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
    #[error("a recent MFA proof for the current session is required")]
    MfaProofRequired,
    #[error("installation-admin TOTP cannot be disabled")]
    InstallationAdminCannotDisable,
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

    /// Disables optional TOTP only after revalidating a recent, session-bound
    /// MFA proof. Installation-admin credentials are mandatory and cannot be
    /// removed by this self-service operation. Existing MFA markers are
    /// cleared atomically so an outstanding proof cannot survive disable.
    pub async fn disable_with_mfa_proof(
        &self,
        proof: &RecentMfaProof,
        now: DateTime<Utc>,
    ) -> Result<bool, TotpCredentialError> {
        if !proof.is_current_at(now) {
            return Err(TotpCredentialError::MfaProofRequired);
        }

        let authenticated_at_text = format_timestamp(proof.authenticated_at);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let proof_is_current: i64 = sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM sessions \
                 JOIN users ON users.id = sessions.user_id \
                 WHERE sessions.id = ? AND sessions.user_id = ? \
                   AND sessions.token_hash = ? \
                   AND sessions.mfa_authenticated_at = ? \
                   AND sessions.revoked_at IS NULL AND users.state = 'active')",
        )
        .bind(&proof.session_id)
        .bind(&proof.user_id)
        .bind(&proof.token_digest)
        .bind(authenticated_at_text)
        .fetch_one(&mut *transaction)
        .await?;
        if proof_is_current != 1 {
            transaction.rollback().await?;
            return Err(TotpCredentialError::MfaProofRequired);
        }

        let is_installation_admin: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM installation_admins WHERE user_id = ?)",
        )
        .bind(&proof.user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if is_installation_admin == 1 {
            transaction.rollback().await?;
            return Err(TotpCredentialError::InstallationAdminCannotDisable);
        }

        let active_credential = sqlx::query(
            "SELECT id FROM mfa_credentials \
             WHERE user_id = ? AND activated_at IS NOT NULL",
        )
        .bind(&proof.user_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(active_credential) = active_credential else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let credential_id: String = active_credential.try_get("id")?;
        let deleted = sqlx::query(
            "DELETE FROM mfa_credentials \
             WHERE id = ? AND user_id = ? AND activated_at IS NOT NULL",
        )
        .bind(&credential_id)
        .bind(&proof.user_id)
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(TotpCredentialError::InvalidCredential);
        }

        sqlx::query(
            "UPDATE sessions SET mfa_authenticated_at = NULL \
             WHERE user_id = ? AND mfa_authenticated_at IS NOT NULL",
        )
        .bind(&proof.user_id)
        .execute(&mut *transaction)
        .await?;
        audit::append_in_transaction(
            &mut transaction,
            Some(&proof.user_id),
            AuditEventKind::TotpDisabled,
            AuditEventResult::Success,
            &AuditEventTarget::mfa_credential(&credential_id),
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{
        auth::{master_key::MasterKeyStore, mfa::step_up::RecentMfaProof},
        db::Database,
    };

    use super::{TotpCredentialError, TotpCredentialStore};
    use crate::auth::mfa::TotpSecretBox;

    fn at(timestamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&Utc)
    }

    async fn fixture(
        is_installation_admin: bool,
    ) -> (
        TempDir,
        Database,
        TotpCredentialStore,
        RecentMfaProof,
        String,
        DateTime<Utc>,
    ) {
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        let user_id = Uuid::now_v7().to_string();
        let credential_id = Uuid::now_v7().to_string();
        let proof_session_id = Uuid::now_v7().to_string();
        let other_session_id = Uuid::now_v7().to_string();
        let now = at("2026-10-10T12:00:00Z");
        let now_text = "2026-10-10T12:00:00.000000000Z";

        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'MFA disable test', 'active', ?, ?)",
        )
        .bind(&user_id)
        .bind(now_text)
        .bind(now_text)
        .execute(database.pool())
        .await
        .unwrap();
        if is_installation_admin {
            sqlx::query(
                "INSERT INTO organizations \
                 (id, name, locale, timezone, created_at, updated_at) \
                 VALUES (1, 'MFA disable test', 'en', 'UTC', ?, ?)",
            )
            .bind(now_text)
            .bind(now_text)
            .execute(database.pool())
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO organization_members \
                 (organization_id, user_id, role, created_at) VALUES (1, ?, 'admin', ?)",
            )
            .bind(&user_id)
            .bind(now_text)
            .execute(database.pool())
            .await
            .unwrap();
            sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, ?)")
                .bind(&user_id)
                .bind(now_text)
                .execute(database.pool())
                .await
                .unwrap();
        }

        for (session_id, token_hash) in [
            (&proof_session_id, [1_u8; 32]),
            (&other_session_id, [2_u8; 32]),
        ] {
            sqlx::query(
                "INSERT INTO sessions \
                 (id, token_hash, user_id, created_at, last_activity_at, expires_at, \
                  reauthenticated_at, revoked_at, mfa_authenticated_at) \
                 VALUES (?, ?, ?, ?, ?, '2026-10-17T12:00:00.000000000Z', NULL, NULL, ?)",
            )
            .bind(session_id)
            .bind(token_hash.as_slice())
            .bind(&user_id)
            .bind(now_text)
            .bind(now_text)
            .bind(now_text)
            .execute(database.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO mfa_credentials \
             (id, user_id, secret_envelope, created_at, expires_at, activated_at, last_accepted_step) \
             VALUES (?, ?, zeroblob(65), ?, NULL, ?, 123)",
        )
        .bind(&credential_id)
        .bind(&user_id)
        .bind(now_text)
        .bind(now_text)
        .execute(database.pool())
        .await
        .unwrap();

        let master_key =
            MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
        let store =
            TotpCredentialStore::new(database.pool().clone(), TotpSecretBox::new(master_key));
        let proof = RecentMfaProof {
            user_id: user_id.clone(),
            session_id: proof_session_id,
            token_digest: vec![1_u8; 32],
            authenticated_at: now,
            session_valid_until: now + TimeDelta::hours(24),
        };
        (directory, database, store, proof, credential_id, now)
    }

    #[tokio::test]
    async fn disable_removes_optional_credential_and_clears_all_session_assurance_atomically() {
        let (_directory, database, store, proof, credential_id, now) = fixture(false).await;

        assert!(
            store
                .disable_with_mfa_proof(&proof, now + TimeDelta::seconds(1))
                .await
                .unwrap()
        );

        let credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mfa_credentials WHERE id = ?")
                .bind(&credential_id)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(credential_count, 0);
        let remaining_markers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE mfa_authenticated_at IS NOT NULL",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(remaining_markers, 0);
        let audit_event: (String, String) = sqlx::query_as(
            "SELECT event_kind, target_id FROM audit_events WHERE event_kind = 'auth.mfa.totp_disabled'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(audit_event.0, "auth.mfa.totp_disabled");
        assert_eq!(audit_event.1, credential_id);
    }

    #[tokio::test]
    async fn disable_rejects_installation_admin_and_stale_session_proofs() {
        let (_directory, admin_database, admin_store, admin_proof, credential_id, now) =
            fixture(true).await;
        assert!(matches!(
            admin_store
                .disable_with_mfa_proof(&admin_proof, now + TimeDelta::seconds(1))
                .await,
            Err(TotpCredentialError::InstallationAdminCannotDisable)
        ));
        let admin_credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mfa_credentials WHERE id = ?")
                .bind(&credential_id)
                .fetch_one(admin_database.pool())
                .await
                .unwrap();
        assert_eq!(admin_credential_count, 1);

        let (_directory, database, store, proof, credential_id, now) = fixture(false).await;
        sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ?")
            .bind("2026-10-10T12:00:00.500000000Z")
            .bind(&proof.session_id)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(matches!(
            store
                .disable_with_mfa_proof(&proof, now + TimeDelta::seconds(1))
                .await,
            Err(TotpCredentialError::MfaProofRequired)
        ));
        let credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mfa_credentials WHERE id = ?")
                .bind(credential_id)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(credential_count, 1);
    }

    #[tokio::test]
    async fn disable_audit_failure_rolls_back_credential_deletion_and_marker_clearing() {
        let (_directory, database, store, proof, credential_id, now) = fixture(false).await;
        sqlx::query(
            "CREATE TRIGGER fail_totp_disabled_audit BEFORE INSERT ON audit_events \
             WHEN NEW.event_kind = 'auth.mfa.totp_disabled' \
             BEGIN SELECT RAISE(ABORT, 'fixture TOTP disable audit failure'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        assert!(
            store
                .disable_with_mfa_proof(&proof, now + TimeDelta::seconds(1))
                .await
                .is_err()
        );
        sqlx::query("DROP TRIGGER fail_totp_disabled_audit")
            .execute(database.pool())
            .await
            .unwrap();

        let credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mfa_credentials WHERE id = ?")
                .bind(&credential_id)
                .fetch_one(database.pool())
                .await
                .unwrap();
        let remaining_markers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE mfa_authenticated_at IS NOT NULL",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(credential_count, 1);
        assert_eq!(remaining_markers, 2);
        assert!(
            store
                .disable_with_mfa_proof(&proof, now + TimeDelta::seconds(1))
                .await
                .unwrap()
        );
    }
}
