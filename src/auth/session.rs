use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget};

use super::time::format_timestamp;
use super::{SessionToken, SessionTokenError};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session policy is invalid")]
    InvalidPolicy,
    #[error("session deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("user is not eligible for an authenticated session")]
    UserNotActive,
    #[error("secure session token generation failed: {0}")]
    TokenGeneration(#[from] SessionTokenError),
    #[error("session persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("session audit persistence failed: {0}")]
    Audit(#[from] crate::audit::AuditEventError),
}

#[derive(Clone, Copy)]
pub struct SessionPolicy {
    absolute_lifetime: TimeDelta,
    inactivity_lifetime: TimeDelta,
    activity_update_interval: TimeDelta,
}

impl SessionPolicy {
    pub fn new(
        absolute_lifetime: TimeDelta,
        inactivity_lifetime: TimeDelta,
        activity_update_interval: TimeDelta,
    ) -> Result<Self, SessionError> {
        if absolute_lifetime <= TimeDelta::zero()
            || inactivity_lifetime <= TimeDelta::zero()
            || activity_update_interval <= TimeDelta::zero()
            || activity_update_interval >= inactivity_lifetime
        {
            return Err(SessionError::InvalidPolicy);
        }

        Ok(Self {
            absolute_lifetime,
            inactivity_lifetime,
            activity_update_interval,
        })
    }

    pub fn initial(activity_update_interval: TimeDelta) -> Result<Self, SessionError> {
        Self::new(
            TimeDelta::days(7),
            TimeDelta::hours(24),
            activity_update_interval,
        )
    }

    pub fn absolute_lifetime(self) -> TimeDelta {
        self.absolute_lifetime
    }

    pub fn inactivity_lifetime(self) -> TimeDelta {
        self.inactivity_lifetime
    }

    pub fn activity_update_interval(self) -> TimeDelta {
        self.activity_update_interval
    }
}

pub struct IssuedSession {
    pub id: String,
    pub token: SessionToken,
    pub expires_at: DateTime<Utc>,
}

pub struct AuthenticatedSession {
    pub id: String,
    pub user_id: String,
}

/// Identity derived only from a current persisted session. This is a snapshot,
/// not cached authority; protected work must share the validation transaction.
pub struct SessionPrincipal {
    session_id: String,
    user_id: String,
}

impl SessionPrincipal {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub is_current: bool,
}

pub enum SessionRevocationOutcome {
    Unauthenticated,
    NotFound,
    Revoked { is_current: bool },
}

pub(crate) struct MfaSessionAssurance {
    pub(crate) session_id: String,
    pub(crate) user_id: String,
    pub(crate) authenticated_at: DateTime<Utc>,
    pub(crate) valid_until: DateTime<Utc>,
}

#[derive(Clone)]
struct SessionRow {
    id: String,
    user_id: String,
    created_at: String,
    last_activity_at: String,
    expires_at: String,
    account_state: String,
}

#[derive(Clone)]
pub struct SessionStore {
    pool: SqlitePool,
    policy: SessionPolicy,
}

impl SessionStore {
    pub fn new(pool: SqlitePool, policy: SessionPolicy) -> Self {
        Self { pool, policy }
    }

    pub(crate) async fn begin_immediate(&self) -> Result<Transaction<'_, Sqlite>, SessionError> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    pub async fn issue(
        &self,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedSession, SessionError> {
        let expires_at = now
            .checked_add_signed(self.policy.absolute_lifetime)
            .ok_or(SessionError::DeadlineOutOfRange)?;
        let session_id = Uuid::now_v7().to_string();
        let token = SessionToken::generate()?;
        let token_hash = token.digest().to_vec();
        let now = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);

        let result = sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
             SELECT ?, ?, id, ?, ?, ? FROM users WHERE id = ? AND state = 'active'",
        )
        .bind(&session_id)
        .bind(token_hash)
        .bind(&now)
        .bind(&now)
        .bind(expires_at_text)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() != 1 {
            return Err(SessionError::UserNotActive);
        }

        Ok(IssuedSession {
            id: session_id,
            token,
            expires_at,
        })
    }

    /// Issues a session inside a caller-owned transaction. This is reserved
    /// for flows that create the active account and its authenticated session
    /// atomically; callers must establish the account's authentication proof
    /// before invoking it.
    pub(crate) async fn issue_for_active_user_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedSession, SessionError> {
        let expires_at = now
            .checked_add_signed(self.policy.absolute_lifetime)
            .ok_or(SessionError::DeadlineOutOfRange)?;
        let session_id = Uuid::now_v7().to_string();
        let token = SessionToken::generate()?;
        let token_hash = token.digest().to_vec();
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);

        let result = sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at) \
             SELECT ?, ?, id, ?, ?, ? FROM users WHERE id = ? AND state = 'active'",
        )
        .bind(&session_id)
        .bind(token_hash)
        .bind(&now_text)
        .bind(&now_text)
        .bind(expires_at_text)
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;

        if result.rows_affected() != 1 {
            return Err(SessionError::UserNotActive);
        }

        Ok(IssuedSession {
            id: session_id,
            token,
            expires_at,
        })
    }

    /// Issues a session only if the exact password PHC verified by the caller
    /// is still current and belongs to this confirmed email. This closes the
    /// verify-then-password-reset race that could otherwise create a fresh
    /// session after a reset had already revoked all existing sessions.
    #[cfg(test)]
    pub(crate) async fn issue_if_credential_matches(
        &self,
        user_id: &str,
        email_key: &str,
        verified_password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<IssuedSession>, SessionError> {
        let mut transaction = self.begin_immediate().await?;
        let result = self
            .issue_if_credential_matches_in_transaction(
                &mut transaction,
                user_id,
                email_key,
                verified_password_hash,
                now,
                None,
            )
            .await?;
        match result {
            Some(issued) => {
                transaction.commit().await?;
                Ok(Some(issued))
            }
            None => {
                transaction.rollback().await?;
                Ok(None)
            }
        }
    }

    pub(crate) async fn issue_if_credential_matches_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        email_key: &str,
        verified_password_hash: &str,
        now: DateTime<Utc>,
        mfa_authenticated_at: Option<DateTime<Utc>>,
    ) -> Result<Option<IssuedSession>, SessionError> {
        let expires_at = now
            .checked_add_signed(self.policy.absolute_lifetime)
            .ok_or(SessionError::DeadlineOutOfRange)?;
        let session_id = Uuid::now_v7().to_string();
        let token = SessionToken::generate()?;
        let token_hash = token.digest().to_vec();
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mfa_authenticated_at = mfa_authenticated_at.map(format_timestamp);

        let result = sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at, \
              reauthenticated_at, revoked_at, mfa_authenticated_at) \
             SELECT ?, ?, users.id, ?, ?, ?, NULL, NULL, ? \
             FROM users \
             JOIN password_credentials AS credentials \
               ON credentials.user_id = users.id \
             JOIN user_emails ON user_emails.user_id = users.id \
             WHERE users.id = ? AND users.state = 'active' \
               AND credentials.password_hash = ? \
               AND user_emails.address_key = ? \
               AND user_emails.confirmed_at IS NOT NULL",
        )
        .bind(&session_id)
        .bind(token_hash)
        .bind(&now_text)
        .bind(&now_text)
        .bind(expires_at_text)
        .bind(mfa_authenticated_at)
        .bind(user_id)
        .bind(verified_password_hash)
        .bind(email_key)
        .execute(&mut **transaction)
        .await?;

        if result.rows_affected() != 1 {
            return Ok(None);
        }

        Ok(Some(IssuedSession {
            id: session_id,
            token,
            expires_at,
        }))
    }

    pub async fn validate(
        &self,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<AuthenticatedSession>, SessionError> {
        let token_hash = token.digest().to_vec();
        let Some(row) = self.load_by_digest(&token_hash).await? else {
            return Ok(None);
        };
        let Some((session, last_activity_at)) = self.validated_row(row, now) else {
            return Ok(None);
        };

        let elapsed = now.signed_duration_since(last_activity_at);
        if elapsed < self.policy.activity_update_interval {
            return Ok(Some(session));
        }

        let previous_activity = format_timestamp(last_activity_at);
        let current_activity = format_timestamp(now);
        let update = sqlx::query(
            "UPDATE sessions SET last_activity_at = ? \
             WHERE id = ? AND token_hash = ? AND revoked_at IS NULL \
               AND last_activity_at = ? \
               AND EXISTS (SELECT 1 FROM users \
                           WHERE users.id = sessions.user_id AND users.state = 'active')",
        )
        .bind(current_activity)
        .bind(&session.id)
        .bind(&token_hash)
        .bind(previous_activity)
        .execute(&self.pool)
        .await?;

        if update.rows_affected() == 1 {
            return Ok(Some(session));
        }

        // Another request may have refreshed the activity timestamp, or a
        // concurrent revoke/ban may have won. Reload once and fail closed if
        // the current record is no longer valid.
        let Some(row) = self.load_by_digest(&token_hash).await? else {
            return Ok(None);
        };
        Ok(self.validated_row(row, now).map(|(session, _)| session))
    }

    pub(crate) async fn current_for_mfa_step_up_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<AuthenticatedSession>, SessionError> {
        Ok(self
            .authenticate_in_transaction(transaction, token, now)
            .await?
            .map(|principal| AuthenticatedSession {
                id: principal.session_id,
                user_id: principal.user_id,
            }))
    }

    /// Validates current identity in the caller's transaction without touching
    /// activity or creating privileged assurance. Do not reuse the principal
    /// outside that snapshot as authorization for another operation.
    pub async fn authenticate_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<SessionPrincipal>, SessionError> {
        let token_hash = token.digest().to_vec();
        let row = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, \
                    users.state AS account_state \
             FROM sessions JOIN users ON users.id = sessions.user_id \
             WHERE sessions.token_hash = ? AND sessions.revoked_at IS NULL",
        )
        .bind(&token_hash)
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_row = SessionRow {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            created_at: row.try_get("created_at")?,
            last_activity_at: row.try_get("last_activity_at")?,
            expires_at: row.try_get("expires_at")?,
            account_state: row.try_get("account_state")?,
        };
        Ok(self
            .validated_row(session_row, now)
            .map(|(session, _)| SessionPrincipal {
                session_id: session.id,
                user_id: session.user_id,
            }))
    }

    /// Rotates the exact live bearer after its MFA factor has been verified in
    /// the same transaction. It preserves the original absolute deadline and
    /// never copies password reauthentication assurance.
    pub(crate) async fn rotate_after_mfa_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<IssuedSession>, SessionError> {
        let token_hash = token.digest().to_vec();
        let row = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, \
                    users.state AS account_state \
             FROM sessions JOIN users ON users.id = sessions.user_id \
             WHERE sessions.token_hash = ? AND sessions.revoked_at IS NULL",
        )
        .bind(&token_hash)
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_row = SessionRow {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            created_at: row.try_get("created_at")?,
            last_activity_at: row.try_get("last_activity_at")?,
            expires_at: row.try_get("expires_at")?,
            account_state: row.try_get("account_state")?,
        };
        let Some((session, _)) = self.validated_row(session_row.clone(), now) else {
            return Ok(None);
        };
        let Some(created_at) = parse_timestamp(&session_row.created_at) else {
            return Ok(None);
        };
        let Some(persisted_expiry) = parse_timestamp(&session_row.expires_at) else {
            return Ok(None);
        };
        let Some(absolute_expiry) = created_at.checked_add_signed(self.policy.absolute_lifetime)
        else {
            return Ok(None);
        };
        let Some(inactivity_expiry) = now.checked_add_signed(self.policy.inactivity_lifetime)
        else {
            return Ok(None);
        };
        let expires_at = persisted_expiry.min(absolute_expiry).min(inactivity_expiry);
        if expires_at <= now {
            return Ok(None);
        }

        let new_session_id = Uuid::now_v7().to_string();
        let new_token = SessionToken::generate()?;
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let created_at_text = format_timestamp(created_at);
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ? \
             WHERE id = ? AND user_id = ? AND token_hash = ? AND revoked_at IS NULL \
               AND EXISTS (SELECT 1 FROM users \
                           WHERE users.id = sessions.user_id AND users.state = 'active')",
        )
        .bind(&now_text)
        .bind(&session.id)
        .bind(&session.user_id)
        .bind(&token_hash)
        .execute(&mut **transaction)
        .await?;
        if revoked.rows_affected() != 1 {
            return Ok(None);
        }

        let inserted = sqlx::query(
            "INSERT INTO sessions \
             (id, token_hash, user_id, created_at, last_activity_at, expires_at, \
              reauthenticated_at, revoked_at, mfa_authenticated_at) \
             SELECT ?, ?, users.id, ?, ?, ?, NULL, NULL, ? \
             FROM users WHERE users.id = ? AND users.state = 'active'",
        )
        .bind(&new_session_id)
        .bind(new_token.digest().as_slice())
        .bind(created_at_text)
        .bind(&now_text)
        .bind(&expires_at_text)
        .bind(&now_text)
        .bind(&session.user_id)
        .execute(&mut **transaction)
        .await?;
        if inserted.rows_affected() != 1 {
            return Ok(None);
        }

        audit::append_in_transaction(
            transaction,
            Some(&session.user_id),
            AuditEventKind::SessionRevoked,
            AuditEventResult::Success,
            &AuditEventTarget::session(&session.id),
            now,
        )
        .await?;
        audit::append_in_transaction(
            transaction,
            Some(&session.user_id),
            AuditEventKind::SessionMfaElevated,
            AuditEventResult::Success,
            &AuditEventTarget::session(&new_session_id),
            now,
        )
        .await?;

        Ok(Some(IssuedSession {
            id: new_session_id,
            token: new_token,
            expires_at,
        }))
    }

    pub(crate) async fn recent_mfa_assurance(
        &self,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<MfaSessionAssurance>, SessionError> {
        let token_hash = token.digest().to_vec();
        let row = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, \
                    sessions.mfa_authenticated_at, users.state AS account_state \
             FROM sessions JOIN users ON users.id = sessions.user_id \
             WHERE sessions.token_hash = ? AND sessions.revoked_at IS NULL",
        )
        .bind(&token_hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_row = SessionRow {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            created_at: row.try_get("created_at")?,
            last_activity_at: row.try_get("last_activity_at")?,
            expires_at: row.try_get("expires_at")?,
            account_state: row.try_get("account_state")?,
        };
        let Some((session, last_activity_at)) = self.validated_row(session_row.clone(), now) else {
            return Ok(None);
        };
        let Some(authenticated_at_text) =
            row.try_get::<Option<String>, _>("mfa_authenticated_at")?
        else {
            return Ok(None);
        };
        let Some(authenticated_at) = parse_timestamp(&authenticated_at_text) else {
            return Ok(None);
        };
        let age = now.signed_duration_since(authenticated_at);
        if age < TimeDelta::zero() || age > TimeDelta::minutes(10) {
            return Ok(None);
        }
        let Some(created_at) = parse_timestamp(&session_row.created_at) else {
            return Ok(None);
        };
        let Some(persisted_expiry) = parse_timestamp(&session_row.expires_at) else {
            return Ok(None);
        };
        let Some(absolute_expiry) = created_at.checked_add_signed(self.policy.absolute_lifetime)
        else {
            return Ok(None);
        };
        let Some(inactivity_expiry) =
            last_activity_at.checked_add_signed(self.policy.inactivity_lifetime)
        else {
            return Ok(None);
        };
        let valid_until = persisted_expiry.min(absolute_expiry).min(inactivity_expiry);
        if now >= valid_until {
            return Ok(None);
        }
        Ok(Some(MfaSessionAssurance {
            session_id: session.id,
            user_id: session.user_id,
            authenticated_at,
            valid_until,
        }))
    }

    pub async fn revoke(
        &self,
        user_id: &str,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, SessionError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !Self::revoke_in_transaction(&mut transaction, user_id, session_id, now).await? {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    /// Revokes only the exact live bearer supplied by the authenticated caller.
    /// Identity, revocation and audit share one write transaction.
    pub async fn revoke_current(
        &self,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<bool, SessionError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(principal) = self
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(false);
        };
        if !Self::revoke_in_transaction(
            &mut transaction,
            principal.user_id(),
            principal.session_id(),
            now,
        )
        .await?
        {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    /// Revalidates the bearer and scopes the selected target to its owner in
    /// the same transaction as revocation/audit. A target ID grants no access.
    pub async fn revoke_owned(
        &self,
        token: &SessionToken,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<SessionRevocationOutcome, SessionError> {
        let mut transaction = self.begin_immediate().await?;
        let Some(principal) = self
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(SessionRevocationOutcome::Unauthenticated);
        };
        if !Self::revoke_in_transaction(&mut transaction, principal.user_id(), session_id, now)
            .await?
        {
            transaction.rollback().await?;
            return Ok(SessionRevocationOutcome::NotFound);
        }
        transaction.commit().await?;
        Ok(SessionRevocationOutcome::Revoked {
            is_current: session_id == principal.session_id(),
        })
    }

    /// Ends every session of the bearer-derived user, including the bearer.
    /// Authentication, global invalidation and audit share one write lock.
    pub async fn revoke_all_owned(
        &self,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<bool, SessionError> {
        let mut transaction = self.begin_immediate().await?;
        let Some(principal) = self
            .authenticate_in_transaction(&mut transaction, token, now)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(false);
        };
        Self::revoke_all_in_transaction(&mut transaction, principal.user_id(), now).await?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn revoke_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, SessionError> {
        let result = sqlx::query(
            "UPDATE sessions SET revoked_at = ? \
             WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
        )
        .bind(format_timestamp(now))
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;
        if result.rows_affected() != 1 {
            return Ok(false);
        }
        audit::append_in_transaction(
            transaction,
            Some(user_id),
            AuditEventKind::SessionRevoked,
            AuditEventResult::Success,
            &AuditEventTarget::session(session_id),
            now,
        )
        .await?;
        Ok(true)
    }

    pub async fn revoke_all(&self, user_id: &str, now: DateTime<Utc>) -> Result<u64, SessionError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let count = Self::revoke_all_in_transaction(&mut transaction, user_id, now).await?;
        transaction.commit().await?;
        Ok(count)
    }

    async fn revoke_all_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<u64, SessionError> {
        let result = sqlx::query(
            "UPDATE sessions SET revoked_at = ? \
             WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(format_timestamp(now))
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(0);
        }
        audit::append_in_transaction(
            transaction,
            Some(user_id),
            AuditEventKind::SessionsRevoked,
            AuditEventResult::Success,
            &AuditEventTarget::user(user_id),
            now,
        )
        .await?;
        Ok(result.rows_affected())
    }

    pub(crate) async fn record_reauthentication(
        &self,
        session: &AuthenticatedSession,
        token: &SessionToken,
        verified_password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, SessionError> {
        let token_hash = token.digest().to_vec();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, \
                    users.state AS account_state, credentials.password_hash \
             FROM sessions \
             JOIN users ON users.id = sessions.user_id \
             JOIN password_credentials AS credentials \
               ON credentials.user_id = users.id \
             WHERE sessions.id = ? AND sessions.user_id = ? \
               AND sessions.token_hash = ? AND sessions.revoked_at IS NULL \
               AND users.state = 'active'",
        )
        .bind(&session.id)
        .bind(&session.user_id)
        .bind(&token_hash)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let created_at_text: String = row.try_get("created_at")?;
        let expires_at_text: String = row.try_get("expires_at")?;
        let password_hash: String = row.try_get("password_hash")?;
        if password_hash != verified_password_hash {
            transaction.rollback().await?;
            return Ok(None);
        }
        let session_row = SessionRow {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            created_at: created_at_text.clone(),
            last_activity_at: row.try_get("last_activity_at")?,
            expires_at: expires_at_text.clone(),
            account_state: row.try_get("account_state")?,
        };
        let Some((validated, last_activity_at)) = self.validated_row(session_row, now) else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if validated.id != session.id || validated.user_id != session.user_id {
            transaction.rollback().await?;
            return Ok(None);
        }
        let Some(inactivity_expiry) =
            last_activity_at.checked_add_signed(self.policy.inactivity_lifetime)
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let Some(created_at) = parse_timestamp(&created_at_text) else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let Some(absolute_expiry) = created_at.checked_add_signed(self.policy.absolute_lifetime)
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let Some(persisted_expiry) = parse_timestamp(&expires_at_text) else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let session_valid_until = inactivity_expiry.min(absolute_expiry).min(persisted_expiry);
        let updated = sqlx::query(
            "UPDATE sessions SET reauthenticated_at = ? \
             WHERE id = ? AND user_id = ? AND token_hash = ? \
               AND revoked_at IS NULL \
               AND EXISTS (SELECT 1 FROM users \
                           JOIN password_credentials AS credentials \
                             ON credentials.user_id = users.id \
                           WHERE users.id = sessions.user_id \
                             AND users.state = 'active' \
                             AND credentials.password_hash = ?)",
        )
        .bind(format_timestamp(now))
        .bind(&session.id)
        .bind(&session.user_id)
        .bind(&token_hash)
        .bind(verified_password_hash)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        transaction.commit().await?;
        Ok(Some(session_valid_until))
    }

    /// Lists only currently valid sessions owned by the authenticated user.
    /// No bearer, IP address, or user-agent value is persisted or returned.
    pub async fn list_active(
        &self,
        user_id: &str,
        current_session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let rows = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, \
                    users.state AS account_state \
             FROM sessions JOIN users ON users.id = sessions.user_id \
             WHERE sessions.user_id = ? AND sessions.revoked_at IS NULL \
             ORDER BY sessions.created_at DESC, sessions.id DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let session_row = SessionRow {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                created_at: row.try_get("created_at")?,
                last_activity_at: row.try_get("last_activity_at")?,
                expires_at: row.try_get("expires_at")?,
                account_state: row.try_get("account_state")?,
            };
            let Some(created_at) = parse_timestamp(&session_row.created_at) else {
                continue;
            };
            let Some(last_activity_at) = parse_timestamp(&session_row.last_activity_at) else {
                continue;
            };
            let Some(expires_at) = parse_timestamp(&session_row.expires_at) else {
                continue;
            };
            if self.validated_row(session_row, now).is_none() {
                continue;
            }
            summaries.push(SessionSummary {
                id: row.try_get("id")?,
                created_at,
                last_activity_at,
                expires_at,
                is_current: row.try_get::<String, _>("id")? == current_session_id,
            });
        }
        Ok(summaries)
    }

    async fn load_by_digest(&self, token_hash: &[u8]) -> Result<Option<SessionRow>, SessionError> {
        let row = sqlx::query(
            "SELECT sessions.id, sessions.user_id, sessions.created_at, \
                    sessions.last_activity_at, sessions.expires_at, users.state AS account_state \
             FROM sessions JOIN users ON users.id = sessions.user_id \
             WHERE sessions.token_hash = ? AND sessions.revoked_at IS NULL",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(SessionRow {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                created_at: row.try_get("created_at")?,
                last_activity_at: row.try_get("last_activity_at")?,
                expires_at: row.try_get("expires_at")?,
                account_state: row.try_get("account_state")?,
            })
        })
        .transpose()
        .map_err(SessionError::Database)
    }

    fn validated_row(
        &self,
        row: SessionRow,
        now: DateTime<Utc>,
    ) -> Option<(AuthenticatedSession, DateTime<Utc>)> {
        if row.account_state != "active" {
            return None;
        }

        let created_at = parse_timestamp(&row.created_at)?;
        let last_activity_at = parse_timestamp(&row.last_activity_at)?;
        let expires_at = parse_timestamp(&row.expires_at)?;
        let maximum_expiry = created_at.checked_add_signed(self.policy.absolute_lifetime)?;
        let inactivity_expiry =
            last_activity_at.checked_add_signed(self.policy.inactivity_lifetime)?;

        if created_at > now
            || last_activity_at < created_at
            || last_activity_at > now
            || expires_at <= now
            || expires_at > maximum_expiry
            || inactivity_expiry <= now
        {
            return None;
        }

        Some((
            AuthenticatedSession {
                id: row.id,
                user_id: row.user_id,
            },
            last_activity_at,
        ))
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PasswordService;
    use crate::db::Database;
    use tempfile::TempDir;

    #[tokio::test]
    async fn password_login_session_requires_the_verified_credential_to_remain_current() {
        let _guard = crate::auth::PASSWORD_KDF_TEST_LOCK.lock().await;
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        let user_id = Uuid::now_v7().to_string();
        let email_id = Uuid::now_v7().to_string();
        let password_service = PasswordService::default();
        let old_password_hash = password_service
            .hash(String::from("a valid old password for race"))
            .await
            .unwrap();
        let new_password_hash = password_service
            .hash(String::from("a valid new password for race"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'Login race', 'active', \
                     '2026-10-10T00:00:00.000000000Z', \
                     '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES (?, ?, 'login-race@example.test', 'login-race@example.test', 0, \
                     '2026-10-10T00:00:00.000000000Z', \
                     '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(email_id)
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .bind(&old_password_hash)
        .execute(database.pool())
        .await
        .unwrap();

        sqlx::query("UPDATE password_credentials SET password_hash = ?")
            .bind(&new_password_hash)
            .execute(database.pool())
            .await
            .unwrap();
        let store = SessionStore::new(
            database.pool().clone(),
            SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
        );
        assert!(
            store
                .issue_if_credential_matches(
                    &user_id,
                    "login-race@example.test",
                    &old_password_hash,
                    DateTime::parse_from_rfc3339("2026-10-10T12:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                )
                .await
                .unwrap()
                .is_none()
        );
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(sessions, 0);
    }

    #[tokio::test]
    async fn reauthentication_commit_rechecks_the_exact_password_hash() {
        let _guard = crate::auth::PASSWORD_KDF_TEST_LOCK.lock().await;
        let directory = TempDir::new().unwrap();
        let database = Database::open(directory.path()).await.unwrap();
        let user_id = Uuid::now_v7().to_string();
        let password_service = PasswordService::default();
        let verified_hash = password_service
            .hash(String::from("a valid original reauth password"))
            .await
            .unwrap();
        let replacement_hash = password_service
            .hash(String::from("a valid replacement reauth password"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, display_name, state, created_at, updated_at) \
             VALUES (?, 'Reauthentication race', 'active', \
                     '2026-10-10T00:00:00.000000000Z', \
                     '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, '2026-10-10T00:00:00.000000000Z')",
        )
        .bind(&user_id)
        .bind(&verified_hash)
        .execute(database.pool())
        .await
        .unwrap();
        let sessions = SessionStore::new(
            database.pool().clone(),
            SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
        );
        let now = DateTime::parse_from_rfc3339("2026-10-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let issued = sessions.issue(&user_id, now).await.unwrap();
        let authenticated = sessions
            .validate(&issued.token, now)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE password_credentials SET password_hash = ? WHERE user_id = ?")
            .bind(replacement_hash)
            .bind(&user_id)
            .execute(database.pool())
            .await
            .unwrap();

        let recorded = sessions
            .record_reauthentication(
                &authenticated,
                &issued.token,
                &verified_hash,
                now + TimeDelta::seconds(1),
            )
            .await
            .unwrap();
        assert!(recorded.is_none());
        let reauthenticated_at: Option<String> =
            sqlx::query_scalar("SELECT reauthenticated_at FROM sessions WHERE id = ?")
                .bind(&issued.id)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert!(reauthenticated_at.is_none());
    }
}
