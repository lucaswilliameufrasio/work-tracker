use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Row, Sqlite, SqliteConnection, SqlitePool, Transaction};
use thiserror::Error;

use super::{master_key::MasterKey, time::format_timestamp};

#[derive(Debug, Error)]
pub enum AuthRateLimitError {
    #[error("authentication rate-limit policy is invalid")]
    InvalidPolicy,
    #[error("authentication rate-limit deadline is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("persisted authentication rate-limit state is malformed")]
    CorruptState,
    #[error("authentication rate-limit master key changed; explicit state migration is required")]
    MasterKeyChanged,
    #[error("authentication rate-limit persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Copy)]
pub struct AuthRateLimitPolicy {
    window: TimeDelta,
    failure_threshold: u32,
    base_backoff: TimeDelta,
    maximum_backoff: TimeDelta,
}

impl AuthRateLimitPolicy {
    pub fn new(
        window: TimeDelta,
        failure_threshold: u32,
        base_backoff: TimeDelta,
        maximum_backoff: TimeDelta,
    ) -> Result<Self, AuthRateLimitError> {
        if window <= TimeDelta::zero()
            || failure_threshold == 0
            || base_backoff <= TimeDelta::zero()
            || maximum_backoff < base_backoff
        {
            return Err(AuthRateLimitError::InvalidPolicy);
        }
        Ok(Self {
            window,
            failure_threshold,
            base_backoff,
            maximum_backoff,
        })
    }
}

#[derive(Clone, Copy)]
pub struct AuthRateLimitPolicies {
    ip: AuthRateLimitPolicy,
    identity: AuthRateLimitPolicy,
    installation: AuthRateLimitPolicy,
}

impl AuthRateLimitPolicies {
    pub fn new(
        ip: AuthRateLimitPolicy,
        identity: AuthRateLimitPolicy,
        installation: AuthRateLimitPolicy,
    ) -> Self {
        Self {
            ip,
            identity,
            installation,
        }
    }

    fn scopes(self) -> [AuthRateLimitPolicy; 3] {
        [self.ip, self.identity, self.installation]
    }

    fn maximum_window(self) -> TimeDelta {
        self.scopes()
            .into_iter()
            .map(|policy| policy.window)
            .max()
            .expect("exactly three authentication rate-limit scopes")
    }
}

#[derive(Clone)]
pub struct AuthRateLimiter {
    pool: SqlitePool,
    master_key: MasterKey,
    policies: AuthRateLimitPolicies,
}

struct BucketState {
    failure_count: u32,
    window_started_at: DateTime<Utc>,
    blocked_until: Option<DateTime<Utc>>,
}

impl AuthRateLimiter {
    pub async fn open(
        pool: SqlitePool,
        master_key: MasterKey,
        policies: AuthRateLimitPolicies,
    ) -> Result<Self, AuthRateLimitError> {
        let limiter = Self {
            pool,
            master_key,
            policies,
        };
        limiter.initialize_key_state().await?;
        Ok(limiter)
    }

    async fn initialize_key_state(&self) -> Result<(), AuthRateLimitError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let key_id = self.master_key.key_id();
        let stored_key_id = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT master_key_id FROM auth_rate_limit_key_state WHERE singleton = 1",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        match stored_key_id {
            Some(stored) if stored.as_slice() == key_id.as_slice() => {}
            Some(stored) => {
                transaction.rollback().await?;
                if stored.len() != 16 {
                    return Err(AuthRateLimitError::CorruptState);
                }
                return Err(AuthRateLimitError::MasterKeyChanged);
            }
            None => {
                sqlx::query(
                    "INSERT INTO auth_rate_limit_key_state (singleton, master_key_id) \
                     VALUES (1, ?)",
                )
                .bind(key_id.as_slice())
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Returns the latest active delay among IP, normalized identity, and
    /// installation buckets. `identity_key` must be canonicalized by the caller.
    /// Resolve client IP from the socket peer unless a proxy is explicitly trusted.
    pub async fn blocked_until(
        &self,
        client_ip: IpAddr,
        identity_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, AuthRateLimitError> {
        let keys = self.subject_hashes(client_ip, identity_key);
        let mut connection = self.pool.acquire().await?;
        let mut latest = None;
        for key in &keys {
            let state = load_bucket(&mut connection, key).await?;
            let Some(state) = state else {
                continue;
            };
            if state.window_started_at > now {
                return Err(AuthRateLimitError::CorruptState);
            }
            if let Some(blocked_until) = state.blocked_until
                && blocked_until > now
            {
                latest = Some(latest.map_or(blocked_until, |current: DateTime<Utc>| {
                    current.max(blocked_until)
                }));
            }
        }
        Ok(latest)
    }

    /// Records one failed password attempt atomically across all three scopes.
    /// Callers should check `blocked_until` before running the KDF. This write
    /// transaction serializes concurrent failure updates across the scopes.
    pub async fn record_failure(
        &self,
        client_ip: IpAddr,
        identity_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, AuthRateLimitError> {
        let keys = self.subject_hashes(client_ip, identity_key);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut states = Vec::with_capacity(keys.len());
        let mut active_block = None;

        for key in &keys {
            let state = load_bucket(&mut transaction, key).await?;
            if let Some(state) = &state {
                if state.window_started_at > now {
                    transaction.rollback().await?;
                    return Err(AuthRateLimitError::CorruptState);
                }
                if let Some(blocked_until) = state.blocked_until
                    && blocked_until > now
                {
                    active_block = Some(
                        active_block.map_or(blocked_until, |current: DateTime<Utc>| {
                            current.max(blocked_until)
                        }),
                    );
                }
            }
            states.push(state);
        }

        if active_block.is_some() {
            transaction.rollback().await?;
            return Ok(active_block);
        }

        let mut latest_block = None;
        for ((key, state), policy) in keys.iter().zip(states).zip(self.policies.scopes()) {
            let next = self.next_bucket(state, now, policy)?;
            if let Some(blocked_until) = next.blocked_until {
                latest_block = Some(
                    latest_block.map_or(blocked_until, |current: DateTime<Utc>| {
                        current.max(blocked_until)
                    }),
                );
            }
            sqlx::query(
                "INSERT INTO auth_rate_limit_buckets \
                 (subject_hash, failure_count, window_started_at, blocked_until) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (subject_hash) DO UPDATE SET \
                     failure_count = excluded.failure_count, \
                     window_started_at = excluded.window_started_at, \
                     blocked_until = excluded.blocked_until",
            )
            .bind(key.as_slice())
            .bind(i64::from(next.failure_count))
            .bind(format_timestamp(next.window_started_at))
            .bind(next.blocked_until.map(format_timestamp))
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;
        Ok(latest_block)
    }

    /// Clears only the successful identity bucket; IP and installation history
    /// remain so a success on one account cannot erase password-spray evidence.
    pub async fn record_success(&self, identity_key: &str) -> Result<(), AuthRateLimitError> {
        let key = self
            .master_key
            .keyed_digest(b"identity", identity_key.as_bytes());
        sqlx::query("DELETE FROM auth_rate_limit_buckets WHERE subject_hash = ?")
            .bind(key.as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn record_success_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        identity_key: &str,
    ) -> Result<(), AuthRateLimitError> {
        let key = self
            .master_key
            .keyed_digest(b"identity", identity_key.as_bytes());
        sqlx::query("DELETE FROM auth_rate_limit_buckets WHERE subject_hash = ?")
            .bind(key.as_slice())
            .execute(&mut **transaction)
            .await?;
        Ok(())
    }

    /// Removes windows that have aged out and whose backoff has elapsed. A
    /// bounded maintenance task should call this periodically.
    pub async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, AuthRateLimitError> {
        let cutoff = now
            .checked_sub_signed(self.policies.maximum_window())
            .ok_or(AuthRateLimitError::DeadlineOutOfRange)?;
        let result = sqlx::query(
            "DELETE FROM auth_rate_limit_buckets \
             WHERE window_started_at <= ? \
               AND (blocked_until IS NULL OR blocked_until <= ?)",
        )
        .bind(format_timestamp(cutoff))
        .bind(format_timestamp(now))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    fn subject_hashes(&self, client_ip: IpAddr, identity_key: &str) -> [[u8; 32]; 3] {
        let ip = client_ip.to_string();
        [
            self.master_key.keyed_digest(b"client-ip", ip.as_bytes()),
            self.master_key
                .keyed_digest(b"identity", identity_key.as_bytes()),
            self.master_key.keyed_digest(b"installation", b"singleton"),
        ]
    }

    fn next_bucket(
        &self,
        existing: Option<BucketState>,
        now: DateTime<Utc>,
        policy: AuthRateLimitPolicy,
    ) -> Result<BucketState, AuthRateLimitError> {
        let (failure_count, window_started_at) = match existing {
            Some(state) => {
                if now < state.window_started_at {
                    return Err(AuthRateLimitError::CorruptState);
                }
                if now.signed_duration_since(state.window_started_at) < policy.window {
                    (
                        state.failure_count.saturating_add(1),
                        state.window_started_at,
                    )
                } else {
                    (1, now)
                }
            }
            None => (1, now),
        };

        let blocked_until = if failure_count < policy.failure_threshold {
            None
        } else {
            let mut remaining_doublings = failure_count - policy.failure_threshold;
            let mut delay = policy.base_backoff;
            while remaining_doublings > 0 && delay < policy.maximum_backoff {
                delay = delay
                    .checked_add(&delay)
                    .unwrap_or(policy.maximum_backoff)
                    .min(policy.maximum_backoff);
                remaining_doublings -= 1;
            }
            Some(
                now.checked_add_signed(delay)
                    .ok_or(AuthRateLimitError::DeadlineOutOfRange)?,
            )
        };

        Ok(BucketState {
            failure_count,
            window_started_at,
            blocked_until,
        })
    }
}

async fn load_bucket(
    connection: &mut SqliteConnection,
    subject_hash: &[u8; 32],
) -> Result<Option<BucketState>, AuthRateLimitError> {
    let row = sqlx::query(
        "SELECT failure_count, window_started_at, blocked_until \
         FROM auth_rate_limit_buckets WHERE subject_hash = ?",
    )
    .bind(subject_hash.as_slice())
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    let failure_count: i64 = row.try_get("failure_count")?;
    let window_started_at: String = row.try_get("window_started_at")?;
    let blocked_until: Option<String> = row.try_get("blocked_until")?;
    if failure_count <= 0 || failure_count > i64::from(u32::MAX) {
        return Err(AuthRateLimitError::CorruptState);
    }
    let window_started_at =
        parse_timestamp(&window_started_at).ok_or(AuthRateLimitError::CorruptState)?;
    let blocked_until = match blocked_until {
        Some(value) => Some(parse_timestamp(&value).ok_or(AuthRateLimitError::CorruptState)?),
        None => None,
    };
    if blocked_until.is_some_and(|deadline| deadline < window_started_at) {
        return Err(AuthRateLimitError::CorruptState);
    }
    Ok(Some(BucketState {
        failure_count: u32::try_from(failure_count)
            .map_err(|_| AuthRateLimitError::CorruptState)?,
        window_started_at,
        blocked_until,
    }))
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}
