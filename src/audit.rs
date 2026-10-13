use chrono::{DateTime, Utc};
use sqlx::{Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::auth::time::format_timestamp;

#[derive(Debug, Error)]
pub enum AuditEventError {
    #[error("audit event persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEventKind {
    EmailAddressAdded,
    EmailAddressConfirmed,
    EmailAddressPrimaryChanged,
    EmailAddressRemoved,
    PasswordResetCompleted,
    PasswordChanged,
    SessionRevoked,
    SessionsRevoked,
    ProjectCreated,
    TotpEnrollmentStarted,
    TotpEnrollmentConfirmed,
    TotpEnrollmentExpired,
    TotpVerified,
}

impl AuditEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::EmailAddressAdded => "auth.email.address_added",
            Self::EmailAddressConfirmed => "auth.email.address_confirmed",
            Self::EmailAddressPrimaryChanged => "auth.email.primary_changed",
            Self::EmailAddressRemoved => "auth.email.address_removed",
            Self::PasswordResetCompleted => "auth.password.reset_completed",
            Self::PasswordChanged => "auth.password.changed",
            Self::SessionRevoked => "auth.session.revoked",
            Self::SessionsRevoked => "auth.sessions.revoked",
            Self::ProjectCreated => "project.created",
            Self::TotpEnrollmentStarted => "auth.mfa.totp_enrollment_started",
            Self::TotpEnrollmentConfirmed => "auth.mfa.totp_enrollment_confirmed",
            Self::TotpEnrollmentExpired => "auth.mfa.totp_enrollment_expired",
            Self::TotpVerified => "auth.mfa.totp_verified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEventResult {
    Success,
    Failure,
}

impl AuditEventResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEventTarget {
    kind: &'static str,
    id: String,
}

impl AuditEventTarget {
    pub fn user(id: impl Into<String>) -> Self {
        Self {
            kind: "user",
            id: id.into(),
        }
    }

    pub fn email_address(id: impl Into<String>) -> Self {
        Self {
            kind: "email_address",
            id: id.into(),
        }
    }

    pub fn session(id: impl Into<String>) -> Self {
        Self {
            kind: "session",
            id: id.into(),
        }
    }

    pub fn project(id: impl Into<String>) -> Self {
        Self {
            kind: "project",
            id: id.into(),
        }
    }

    pub fn mfa_credential(id: impl Into<String>) -> Self {
        Self {
            kind: "mfa_credential",
            id: id.into(),
        }
    }
}

/// Appends a minimal event to a caller-owned transaction so the event and the
/// protected mutation commit or roll back together. No payload/details field
/// exists: callers cannot attach passwords, tokens, addresses, or content.
pub async fn append_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    actor_user_id: Option<&str>,
    event_kind: AuditEventKind,
    result: AuditEventResult,
    target: &AuditEventTarget,
    occurred_at: DateTime<Utc>,
) -> Result<String, AuditEventError> {
    let event_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO audit_events \
         (id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(event_kind.as_str())
    .bind(result.as_str())
    .bind(actor_user_id)
    .bind(target.kind)
    .bind(&target.id)
    .bind(format_timestamp(occurred_at))
    .execute(&mut **transaction)
    .await?;
    Ok(event_id)
}
