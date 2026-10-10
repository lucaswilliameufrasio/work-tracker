use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

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

#[derive(Clone)]
pub struct SessionStore {
    pool: SqlitePool,
    policy: SessionPolicy,
}

impl SessionStore {
    pub fn new(pool: SqlitePool, policy: SessionPolicy) -> Self {
        Self { pool, policy }
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
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)
}
