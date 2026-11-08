use chrono::{DateTime, TimeDelta, Utc};
use thiserror::Error;
use uuid::Uuid;

use super::store::{TotpCredentialError, TotpCredentialStore};
use crate::auth::{
    SessionToken,
    session::{IssuedSession, SessionError, SessionStore},
};

const RECENT_MFA_LIFETIME: TimeDelta = TimeDelta::minutes(10);

#[derive(Debug, Error)]
pub enum MfaStepUpError {
    #[error("MFA session step-up transaction failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("MFA session step-up failed: {0}")]
    Session(#[from] SessionError),
    #[error("MFA credential verification failed: {0}")]
    Credential(#[from] TotpCredentialError),
}

/// A non-serializable proof bound to the exact rotated session and bearer.
/// Its fields cannot be constructed by callers outside this crate.
pub struct RecentMfaProof {
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) token_digest: Vec<u8>,
    pub(crate) authenticated_at: DateTime<Utc>,
    pub(crate) session_valid_until: DateTime<Utc>,
}

impl RecentMfaProof {
    pub(crate) fn is_current_at(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.authenticated_at);
        age >= TimeDelta::zero() && age <= RECENT_MFA_LIFETIME && now < self.session_valid_until
    }
}

/// A successful step-up returns both the replacement bearer and proof for
/// completing a protected action in the same request. The transport must
/// install the new token; the old token is already revoked.
pub struct MfaElevation {
    session: IssuedSession,
    proof: RecentMfaProof,
}

impl MfaElevation {
    pub fn session(&self) -> &IssuedSession {
        &self.session
    }

    pub fn proof(&self) -> &RecentMfaProof {
        &self.proof
    }

    pub fn into_parts(self) -> (IssuedSession, RecentMfaProof) {
        (self.session, self.proof)
    }
}

#[derive(Clone)]
pub struct MfaStepUpService {
    sessions: SessionStore,
    credentials: TotpCredentialStore,
}

impl MfaStepUpService {
    pub fn new(sessions: SessionStore, credentials: TotpCredentialStore) -> Self {
        Self {
            sessions,
            credentials,
        }
    }

    /// Verifies TOTP for the user resolved from the exact current session,
    /// consumes the step, rotates the session, and records MFA assurance in a
    /// single transaction. Callers must rate-limit before invoking this API.
    pub async fn elevate(
        &self,
        token: &SessionToken,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<MfaElevation>, MfaStepUpError> {
        let mut transaction = self.sessions.begin_immediate().await?;
        let Some(current_session) = self
            .sessions
            .current_for_mfa_step_up_in_transaction(&mut transaction, token, now)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let Ok(user_id) = Uuid::parse_str(&current_session.user_id) else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if !self
            .credentials
            .verify_in_transaction(&mut transaction, user_id, code, now)
            .await?
        {
            transaction.rollback().await?;
            return Ok(None);
        }
        let Some(session) = self
            .sessions
            .rotate_after_mfa_in_transaction(&mut transaction, token, now)
            .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        transaction.commit().await?;

        let proof = RecentMfaProof {
            user_id: current_session.user_id,
            session_id: session.id.clone(),
            token_digest: session.token.digest().to_vec(),
            authenticated_at: now,
            session_valid_until: session.expires_at,
        };
        Ok(Some(MfaElevation { session, proof }))
    }

    /// Reconstructs recent assurance only from the exact live session bearer
    /// and its persisted MFA marker. It never accepts caller-supplied identity.
    pub async fn recent_proof(
        &self,
        token: &SessionToken,
        now: DateTime<Utc>,
    ) -> Result<Option<RecentMfaProof>, MfaStepUpError> {
        let Some(assurance) = self.sessions.recent_mfa_assurance(token, now).await? else {
            return Ok(None);
        };
        Ok(Some(RecentMfaProof {
            user_id: assurance.user_id,
            session_id: assurance.session_id,
            token_digest: token.digest().to_vec(),
            authenticated_at: assurance.authenticated_at,
            session_valid_until: assurance.valid_until,
        }))
    }
}
