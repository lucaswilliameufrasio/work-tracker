use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    access::{OrganizationRole, ProjectRole},
    auth::time::format_timestamp,
};

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
    SessionMfaElevated,
    TotpDisabled,
    MfaRecoveryStarted,
    MfaRecoveryEnrollmentStarted,
    MfaRecoveryCompleted,
    ProjectMemberAdded,
    ProjectMemberRoleChanged,
    ProjectMemberRemoved,
    OrganizationMemberAdded,
    OrganizationMemberRoleChanged,
    OrganizationMemberRemoved,
    OrganizationInviteCreated,
    OrganizationInviteReissued,
    OrganizationInviteCanceled,
    OrganizationInviteAccepted,
    UserBanned,
    UserUnbanned,
    UserDisabled,
    UserReactivated,
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
            Self::SessionMfaElevated => "auth.session.mfa_elevated",
            Self::TotpDisabled => "auth.mfa.totp_disabled",
            Self::MfaRecoveryStarted => "auth.mfa.recovery_started",
            Self::MfaRecoveryEnrollmentStarted => "auth.mfa.recovery_enrollment_started",
            Self::MfaRecoveryCompleted => "auth.mfa.recovery_completed",
            Self::ProjectMemberAdded => "project.member.added",
            Self::ProjectMemberRoleChanged => "project.member.role_changed",
            Self::ProjectMemberRemoved => "project.member.removed",
            Self::OrganizationMemberAdded => "organization.member.added",
            Self::OrganizationMemberRoleChanged => "organization.member.role_changed",
            Self::OrganizationMemberRemoved => "organization.member.removed",
            Self::OrganizationInviteCreated => "organization.invite.created",
            Self::OrganizationInviteReissued => "organization.invite.reissued",
            Self::OrganizationInviteCanceled => "organization.invite.canceled",
            Self::OrganizationInviteAccepted => "organization.invite.accepted",
            Self::UserBanned => "auth.user.banned",
            Self::UserUnbanned => "auth.user.unbanned",
            Self::UserDisabled => "auth.user.disabled",
            Self::UserReactivated => "auth.user.reactivated",
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

#[derive(Clone, PartialEq, Eq)]
pub struct AuditEventTarget {
    kind: &'static str,
    id: String,
    subject_user_id: Option<String>,
    project_role_before: Option<String>,
    project_role_after: Option<String>,
    organization_role_before: Option<String>,
    organization_role_after: Option<String>,
    reason: Option<String>,
}

impl fmt::Debug for AuditEventTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditEventTarget")
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("subject_user_id", &self.subject_user_id)
            .field("project_role_before", &self.project_role_before)
            .field("project_role_after", &self.project_role_after)
            .field("organization_role_before", &self.organization_role_before)
            .field("organization_role_after", &self.organization_role_after)
            .field("reason_present", &self.reason.is_some())
            .finish()
    }
}

impl AuditEventTarget {
    pub fn user(id: impl Into<String>) -> Self {
        Self {
            kind: "user",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn user_ban(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: "user",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: Some(reason.into()),
        }
    }

    pub fn email_address(id: impl Into<String>) -> Self {
        Self {
            kind: "email_address",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn session(id: impl Into<String>) -> Self {
        Self {
            kind: "session",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn project(id: impl Into<String>) -> Self {
        Self {
            kind: "project",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn project_membership(
        project_id: impl Into<String>,
        subject_user_id: impl Into<String>,
        role_before: Option<ProjectRole>,
        role_after: Option<ProjectRole>,
    ) -> Self {
        Self {
            kind: "project",
            id: project_id.into(),
            subject_user_id: Some(subject_user_id.into()),
            project_role_before: role_before.map(project_role_as_str).map(str::to_owned),
            project_role_after: role_after.map(project_role_as_str).map(str::to_owned),
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn organization_membership(
        user_id: impl Into<String>,
        role_before: Option<OrganizationRole>,
        role_after: Option<OrganizationRole>,
    ) -> Self {
        let user_id = user_id.into();
        Self {
            kind: "user",
            id: user_id.clone(),
            subject_user_id: Some(user_id),
            project_role_before: None,
            project_role_after: None,
            organization_role_before: role_before.map(organization_role_as_str).map(str::to_owned),
            organization_role_after: role_after.map(organization_role_as_str).map(str::to_owned),
            reason: None,
        }
    }

    pub fn mfa_credential(id: impl Into<String>) -> Self {
        Self {
            kind: "mfa_credential",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn mfa_recovery_session(id: impl Into<String>) -> Self {
        Self {
            kind: "mfa_recovery_session",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }

    pub fn organization_invite(id: impl Into<String>) -> Self {
        Self {
            kind: "invite",
            id: id.into(),
            subject_user_id: None,
            project_role_before: None,
            project_role_after: None,
            organization_role_before: None,
            organization_role_after: None,
            reason: None,
        }
    }
}

/// Appends a typed event to a caller-owned transaction so the event and the
/// protected mutation commit or roll back together. Membership events may
/// include only the affected user and before/after membership roles. Account
/// bans additionally carry their required, bounded reason; there is no
/// arbitrary details field for passwords, tokens, addresses, or content.
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
           (id, event_kind, result, actor_user_id, target_kind, target_id, \
            subject_user_id, project_role_before, project_role_after, \
            organization_role_before, organization_role_after, reason, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(event_kind.as_str())
    .bind(result.as_str())
    .bind(actor_user_id)
    .bind(target.kind)
    .bind(&target.id)
    .bind(&target.subject_user_id)
    .bind(&target.project_role_before)
    .bind(&target.project_role_after)
    .bind(&target.organization_role_before)
    .bind(&target.organization_role_after)
    .bind(&target.reason)
    .bind(format_timestamp(occurred_at))
    .execute(&mut **transaction)
    .await?;
    Ok(event_id)
}

fn project_role_as_str(role: ProjectRole) -> &'static str {
    match role {
        ProjectRole::Manager => "manager",
        ProjectRole::Member => "member",
        ProjectRole::Viewer => "viewer",
    }
}

fn organization_role_as_str(role: OrganizationRole) -> &'static str {
    match role {
        OrganizationRole::Owner => "owner",
        OrganizationRole::Admin => "admin",
        OrganizationRole::Member => "member",
        OrganizationRole::Guest => "guest",
    }
}
