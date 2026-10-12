use chrono::{DateTime, TimeDelta, Utc};
use thiserror::Error;

use super::{
    SessionToken,
    credentials::{CredentialError, CredentialStore},
    session::{SessionError, SessionStore},
};

const RECENT_REAUTHENTICATION_LIFETIME_SECONDS: i64 = 10 * 60;

#[derive(Debug, Error)]
pub enum ReauthenticationError {
    #[error("password reauthentication operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("session reauthentication operation failed: {0}")]
    Session(#[from] SessionError),
}

/// An opaque, short-lived proof that a live session rechecked its password.
/// Its fields are crate-private so callers cannot manufacture proof values.
pub struct RecentReauthentication {
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) token_digest: Vec<u8>,
    pub(crate) authenticated_at: DateTime<Utc>,
    pub(crate) session_valid_until: DateTime<Utc>,
}

#[derive(Clone)]
pub struct ReauthenticationService {
    credentials: CredentialStore,
    sessions: SessionStore,
    dummy_hash: String,
}

impl ReauthenticationService {
    pub fn new(credentials: CredentialStore, sessions: SessionStore, dummy_hash: String) -> Self {
        Self {
            credentials,
            sessions,
            dummy_hash,
        }
    }

    /// Rechecks the password for the current opaque session and records the
    /// proof only if the exact PHC verified remains current at commit time.
    /// Public callers must apply the configured request rate limits first.
    pub async fn reauthenticate_password(
        &self,
        token: &SessionToken,
        password: String,
        now: DateTime<Utc>,
    ) -> Result<Option<RecentReauthentication>, ReauthenticationError> {
        let Some(session) = self.sessions.validate(token, now).await? else {
            return Ok(None);
        };
        let Some(verified_hash) = self
            .credentials
            .verify_password_hash_for_user(&session.user_id, password, &self.dummy_hash)
            .await?
        else {
            return Ok(None);
        };
        let Some(session_valid_until) = self
            .sessions
            .record_reauthentication(&session, token, &verified_hash, now)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(RecentReauthentication {
            user_id: session.user_id,
            session_id: session.id,
            token_digest: token.digest().to_vec(),
            authenticated_at: now,
            session_valid_until,
        }))
    }
}

impl RecentReauthentication {
    pub(crate) fn is_current_at(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.authenticated_at);
        age >= TimeDelta::zero()
            && age <= TimeDelta::seconds(RECENT_REAUTHENTICATION_LIFETIME_SECONDS)
            && now < self.session_valid_until
    }
}
