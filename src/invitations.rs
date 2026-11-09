use std::collections::HashMap;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool, Transaction};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    access::{
        AccessCapability, AccessContext, AccessStore, AccessStoreError, OrganizationRole,
        ProjectRole, authorize,
    },
    audit::{self, AuditEventError, AuditEventKind, AuditEventResult, AuditEventTarget},
    auth::{
        EmailAddressError, PasswordError, PasswordService, normalize_email_address,
        reauthentication::RecentReauthentication,
        session::{IssuedSession, SessionError, SessionStore},
        time::format_timestamp,
    },
};

const INVITATION_TOKEN_DOMAIN: &[u8] = b"puxiri:organization-invite:v1:";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvitationTokenError {
    #[error("invitation token encoding is invalid")]
    InvalidEncoding,
    #[error("secure invitation token generation failed")]
    RandomnessFailed,
}

/// Opaque, one-time organization invitation token. Its value is exposed only
/// to the caller that issues or presents the invitation.
pub struct OrganizationInvitationToken(Zeroizing<String>);

impl OrganizationInvitationToken {
    pub fn parse(value: &str) -> Result<Self, InvitationTokenError> {
        if value.len() != 43
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(InvitationTokenError::InvalidEncoding);
        }

        let decoded = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| InvitationTokenError::InvalidEncoding)?,
        );
        let canonical = Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice()));
        if decoded.len() != 32 || canonical.as_str() != value {
            return Err(InvitationTokenError::InvalidEncoding);
        }

        Ok(Self(Zeroizing::new(value.to_owned())))
    }

    fn generate() -> Result<Self, InvitationTokenError> {
        let mut random_bytes = Zeroizing::new([0_u8; 32]);
        OsRng
            .try_fill_bytes(&mut *random_bytes)
            .map_err(|_| InvitationTokenError::RandomnessFailed)?;
        Ok(Self(Zeroizing::new(
            URL_SAFE_NO_PAD.encode(random_bytes.as_slice()),
        )))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(INVITATION_TOKEN_DOMAIN);
        digest.update(self.0.as_bytes());
        digest.finalize().into()
    }
}

#[derive(Debug, Error)]
pub enum OrganizationInvitationError {
    #[error("invitation issuance and acceptance are disabled")]
    Disabled,
    #[error("actor cannot manage invitations or lacks the required project scope")]
    Forbidden,
    #[error("invitation email address is invalid")]
    InvalidEmail(#[from] EmailAddressError),
    #[error("an organization guest invitation requires at least one project")]
    GuestRequiresProject,
    #[error("guest invitations cannot grant project-manager access")]
    GuestCannotBeProjectManager,
    #[error("the same project cannot appear more than once in an invitation")]
    DuplicateProject,
    #[error("an assigned project does not exist or is archived")]
    ProjectUnavailable,
    #[error("the invitation target is already an organization member")]
    TargetAlreadyMember,
    #[error("a different or unauthorized invitation is already outstanding for this email")]
    InvitationAlreadyOutstanding,
    #[error("the invitation does not exist")]
    InvitationNotFound,
    #[error("the invitation has already been accepted")]
    InvitationAlreadyAccepted,
    #[error("the invitation is no longer pending")]
    InvitationNotPending,
    #[error("the invitation has expired")]
    InvitationExpired,
    #[error("the invitation token is invalid")]
    InvalidToken,
    #[error("the accepting account must be active")]
    UserNotActive,
    #[error("the invitation email must be confirmed on the accepting account")]
    EmailNotConfirmed,
    #[error("the accepting account already belongs to the organization")]
    AlreadyOrganizationMember,
    #[error("an account already uses the invited email; sign in to accept this invitation")]
    AccountAlreadyExists,
    #[error("display name must not be empty")]
    InvalidDisplayName,
    #[error("recent password reauthentication is required")]
    ReauthenticationRequired,
    #[error("invitation expiry is outside the supported timestamp range")]
    DeadlineOutOfRange,
    #[error("invitation lifetime must be positive")]
    InvalidPolicy,
    #[error("persisted invitation or membership data is malformed")]
    CorruptState,
    #[error("invitation token operation failed: {0}")]
    Token(#[from] InvitationTokenError),
    #[error("registration password was rejected: {0}")]
    Password(#[from] PasswordError),
    #[error("registration session creation failed: {0}")]
    Session(#[from] SessionError),
    #[error("invitation authorization context could not be loaded: {0}")]
    Access(#[from] AccessStoreError),
    #[error("invitation persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invitation audit persistence failed: {0}")]
    Audit(#[from] AuditEventError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationInviteRole {
    Member,
    Guest,
}

impl OrganizationInviteRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Guest => "guest",
        }
    }

    fn organization_role(self) -> OrganizationRole {
        match self {
            Self::Member => OrganizationRole::Member,
            Self::Guest => OrganizationRole::Guest,
        }
    }

    fn parse(value: &str) -> Result<Self, OrganizationInvitationError> {
        match value {
            "member" => Ok(Self::Member),
            "guest" => Ok(Self::Guest),
            _ => Err(OrganizationInvitationError::CorruptState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationProjectAssignment {
    pub project_id: String,
    pub role: ProjectRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationInvitationStatus {
    Pending,
    Accepted,
    Expired,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationInvitationRecord {
    pub id: String,
    pub email: Option<String>,
    pub role: OrganizationInviteRole,
    pub status: OrganizationInvitationStatus,
    pub created_by_user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub canceled_at: Option<DateTime<Utc>>,
    pub projects: Vec<InvitationProjectAssignment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvitationIssueResult {
    Created,
    Reissued,
}

pub struct IssuedOrganizationInvitation {
    pub id: String,
    pub token: OrganizationInvitationToken,
    pub expires_at: DateTime<Utc>,
    pub result: InvitationIssueResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedOrganizationInvitation {
    pub invitation_id: String,
    pub role: OrganizationInviteRole,
    pub projects: Vec<InvitationProjectAssignment>,
}

pub struct RegisteredOrganizationInvitee {
    pub user_id: String,
    pub invitation_id: String,
    pub role: OrganizationInviteRole,
    pub projects: Vec<InvitationProjectAssignment>,
    pub session: IssuedSession,
}

struct PendingOrganizationInvitation {
    id: String,
    email: String,
    email_key: String,
    role: OrganizationInviteRole,
    projects: Vec<InvitationProjectAssignment>,
}

#[derive(Clone, Copy)]
pub struct InvitationPolicy {
    enabled: bool,
    lifetime: TimeDelta,
}

impl InvitationPolicy {
    pub fn new(enabled: bool, lifetime: TimeDelta) -> Result<Self, OrganizationInvitationError> {
        if lifetime <= TimeDelta::zero() {
            return Err(OrganizationInvitationError::InvalidPolicy);
        }
        Ok(Self { enabled, lifetime })
    }

    pub fn initial() -> Self {
        Self {
            enabled: true,
            lifetime: TimeDelta::days(7),
        }
    }
}

#[derive(Clone)]
pub struct OrganizationInvitationStore {
    pool: SqlitePool,
    policy: InvitationPolicy,
}

impl OrganizationInvitationStore {
    pub fn new(pool: SqlitePool, policy: InvitationPolicy) -> Self {
        Self { pool, policy }
    }

    /// Creates or reissues an organization invitation. The clear token is
    /// returned once; only its domain-separated digest is stored.
    pub async fn issue(
        &self,
        actor_user_id: &str,
        email_address: &str,
        role: OrganizationInviteRole,
        projects: &[InvitationProjectAssignment],
        now: DateTime<Utc>,
    ) -> Result<IssuedOrganizationInvitation, OrganizationInvitationError> {
        if !self.policy.enabled {
            return Err(OrganizationInvitationError::Disabled);
        }

        let email = normalize_email_address(email_address)?;
        let mut projects = projects.to_vec();
        projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        for pair in projects.windows(2) {
            if pair[0].project_id == pair[1].project_id {
                return Err(OrganizationInvitationError::DuplicateProject);
            }
        }
        if role == OrganizationInviteRole::Guest && projects.is_empty() {
            return Err(OrganizationInvitationError::GuestRequiresProject);
        }
        if role == OrganizationInviteRole::Guest
            && projects
                .iter()
                .any(|assignment| assignment.role == ProjectRole::Manager)
        {
            return Err(OrganizationInvitationError::GuestCannotBeProjectManager);
        }

        let expires_at = now
            .checked_add_signed(self.policy.lifetime)
            .ok_or(OrganizationInvitationError::DeadlineOutOfRange)?;
        let now_text = format_timestamp(now);
        let expires_at_text = format_timestamp(expires_at);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor_context = load_organization_context(&mut transaction, actor_user_id).await?;
        let is_organization_admin = is_organization_admin(&actor_context);
        if !is_organization_admin && (role != OrganizationInviteRole::Guest || projects.is_empty())
        {
            return Err(OrganizationInvitationError::Forbidden);
        }

        for assignment in &projects {
            ensure_project_is_active(&mut transaction, &assignment.project_id).await?;
            if !is_organization_admin {
                if assignment.role == ProjectRole::Manager {
                    return Err(OrganizationInvitationError::Forbidden);
                }
                let project_context = AccessStore::load_project_context_in_transaction(
                    &mut transaction,
                    actor_user_id,
                    &assignment.project_id,
                    false,
                )
                .await?
                .ok_or(OrganizationInvitationError::Forbidden)?;
                if project_context.project_role() != Some(ProjectRole::Manager)
                    || authorize(&project_context, AccessCapability::ManageProjectMembership)
                        .is_err()
                {
                    return Err(OrganizationInvitationError::Forbidden);
                }
            }
        }

        let already_member = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS( \
                 SELECT 1 FROM user_emails \
                 JOIN organization_members \
                   ON organization_members.organization_id = 1 \
                  AND organization_members.user_id = user_emails.user_id \
                 WHERE user_emails.address_key = ? \
             )",
        )
        .bind(&email.key)
        .fetch_one(&mut *transaction)
        .await?;
        if already_member != 0 {
            return Err(OrganizationInvitationError::TargetAlreadyMember);
        }

        let existing_invites = sqlx::query(
            "SELECT id, created_by_user_id, role FROM organization_invites \
             WHERE organization_id = 1 AND email_key = ? \
               AND accepted_at IS NULL AND canceled_at IS NULL \
             ORDER BY created_at, id",
        )
        .bind(&email.key)
        .fetch_all(&mut *transaction)
        .await?;
        if !is_organization_admin {
            for invite in &existing_invites {
                let creator = invite.try_get::<String, _>("created_by_user_id")?;
                let existing_role = invite.try_get::<String, _>("role")?;
                if creator != actor_user_id || existing_role != "guest" {
                    return Err(OrganizationInvitationError::InvitationAlreadyOutstanding);
                }
            }
        }

        let token = OrganizationInvitationToken::generate()?;
        let invite_id = Uuid::now_v7().to_string();
        for invite in &existing_invites {
            let previous_id = invite.try_get::<String, _>("id")?;
            let canceled = sqlx::query(
                "UPDATE organization_invites SET canceled_at = ? \
                 WHERE id = ? AND accepted_at IS NULL AND canceled_at IS NULL",
            )
            .bind(&now_text)
            .bind(&previous_id)
            .execute(&mut *transaction)
            .await?;
            if canceled.rows_affected() != 1 {
                return Err(OrganizationInvitationError::CorruptState);
            }
            append_invite_event(
                &mut transaction,
                actor_user_id,
                AuditEventKind::OrganizationInviteCanceled,
                &previous_id,
                now,
            )
            .await?;
        }

        sqlx::query(
            "INSERT INTO organization_invites \
             (id, organization_id, email, email_key, role, token_hash, \
              created_by_user_id, created_at, expires_at) \
             VALUES (?, 1, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&invite_id)
        .bind(&email.address)
        .bind(&email.key)
        .bind(role.as_str())
        .bind(token.digest().to_vec())
        .bind(actor_user_id)
        .bind(&now_text)
        .bind(&expires_at_text)
        .execute(&mut *transaction)
        .await?;

        for assignment in &projects {
            sqlx::query(
                "INSERT INTO organization_invite_projects \
                 (invite_id, project_id, organization_id, role) \
                 VALUES (?, ?, 1, ?)",
            )
            .bind(&invite_id)
            .bind(&assignment.project_id)
            .bind(project_role_as_str(assignment.role))
            .execute(&mut *transaction)
            .await?;
        }

        let event_kind = if existing_invites.is_empty() {
            AuditEventKind::OrganizationInviteCreated
        } else {
            AuditEventKind::OrganizationInviteReissued
        };
        append_invite_event(&mut transaction, actor_user_id, event_kind, &invite_id, now).await?;
        transaction.commit().await?;

        Ok(IssuedOrganizationInvitation {
            id: invite_id,
            token,
            expires_at,
            result: if existing_invites.is_empty() {
                InvitationIssueResult::Created
            } else {
                InvitationIssueResult::Reissued
            },
        })
    }

    /// Lists invitations and their derived lifecycle state for organization
    /// owners and admins. Expiration is computed at read time.
    pub async fn list(
        &self,
        actor_user_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrganizationInvitationRecord>, OrganizationInvitationError> {
        let mut transaction = self.pool.begin().await?;
        let context = load_organization_context(&mut transaction, actor_user_id).await?;
        if !is_organization_admin(&context) {
            transaction.rollback().await?;
            return Err(OrganizationInvitationError::Forbidden);
        }

        let rows = sqlx::query(
            "SELECT id, email, recipient_redacted_at, role, created_by_user_id, \
                    created_at, expires_at, accepted_at, canceled_at \
             FROM organization_invites WHERE organization_id = 1 \
             ORDER BY created_at DESC, id DESC",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let project_rows = sqlx::query(
            "SELECT organization_invite_projects.invite_id, \
                    organization_invite_projects.project_id, \
                    organization_invite_projects.role \
             FROM organization_invite_projects \
             JOIN organization_invites \
               ON organization_invites.id = organization_invite_projects.invite_id \
             WHERE organization_invites.organization_id = 1 \
             ORDER BY organization_invite_projects.invite_id, \
                      organization_invite_projects.project_id",
        )
        .fetch_all(&mut *transaction)
        .await?;

        let mut projects_by_invite: HashMap<String, Vec<InvitationProjectAssignment>> =
            HashMap::new();
        for row in project_rows {
            let invite_id = row.try_get::<String, _>("invite_id")?;
            let project_id = row.try_get::<String, _>("project_id")?;
            let role = parse_project_role(&row.try_get::<String, _>("role")?)?;
            projects_by_invite
                .entry(invite_id)
                .or_default()
                .push(InvitationProjectAssignment { project_id, role });
        }

        let mut invitations = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.try_get::<String, _>("id")?;
            let created_at = parse_timestamp(&row.try_get::<String, _>("created_at")?)?;
            let expires_at = parse_timestamp(&row.try_get::<String, _>("expires_at")?)?;
            let accepted_at =
                parse_optional_timestamp(row.try_get::<Option<String>, _>("accepted_at")?)?;
            let canceled_at =
                parse_optional_timestamp(row.try_get::<Option<String>, _>("canceled_at")?)?;
            let recipient_redacted_at = parse_optional_timestamp(
                row.try_get::<Option<String>, _>("recipient_redacted_at")?,
            )?;
            let status = invitation_status(accepted_at, canceled_at, expires_at, now);
            invitations.push(OrganizationInvitationRecord {
                id: id.clone(),
                email: if recipient_redacted_at.is_some() {
                    None
                } else {
                    Some(row.try_get("email")?)
                },
                role: OrganizationInviteRole::parse(&row.try_get::<String, _>("role")?)?,
                status,
                created_by_user_id: row.try_get("created_by_user_id")?,
                created_at,
                expires_at,
                accepted_at,
                canceled_at,
                projects: projects_by_invite.remove(&id).unwrap_or_default(),
            });
        }

        transaction.commit().await?;
        Ok(invitations)
    }

    /// Cancels a pending or expired invitation. Accepted invitations are
    /// immutable and cannot be canceled retroactively.
    pub async fn cancel(
        &self,
        actor_user_id: &str,
        invitation_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), OrganizationInvitationError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = load_organization_context(&mut transaction, actor_user_id).await?;
        if !is_organization_admin(&context) {
            return Err(OrganizationInvitationError::Forbidden);
        }

        let invite = sqlx::query(
            "SELECT accepted_at, canceled_at FROM organization_invites \
             WHERE id = ? AND organization_id = 1",
        )
        .bind(invitation_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(invite) = invite else {
            return Err(OrganizationInvitationError::InvitationNotFound);
        };
        if invite
            .try_get::<Option<String>, _>("accepted_at")?
            .is_some()
        {
            return Err(OrganizationInvitationError::InvitationAlreadyAccepted);
        }
        if invite
            .try_get::<Option<String>, _>("canceled_at")?
            .is_some()
        {
            return Err(OrganizationInvitationError::InvitationNotPending);
        }

        let canceled = sqlx::query(
            "UPDATE organization_invites SET canceled_at = ? \
             WHERE id = ? AND organization_id = 1 \
               AND accepted_at IS NULL AND canceled_at IS NULL",
        )
        .bind(format_timestamp(now))
        .bind(invitation_id)
        .execute(&mut *transaction)
        .await?;
        if canceled.rows_affected() != 1 {
            return Err(OrganizationInvitationError::CorruptState);
        }
        append_invite_event(
            &mut transaction,
            actor_user_id,
            AuditEventKind::OrganizationInviteCanceled,
            invitation_id,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Accepts an invitation for the current active account only after recent
    /// password reauthentication and confirmation of the invited address.
    /// The invitation, organization membership, project memberships, and audit
    /// records commit atomically.
    pub async fn accept(
        &self,
        proof: &RecentReauthentication,
        token: &OrganizationInvitationToken,
        now: DateTime<Utc>,
    ) -> Result<AcceptedOrganizationInvitation, OrganizationInvitationError> {
        if !self.policy.enabled {
            return Err(OrganizationInvitationError::Disabled);
        }

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_current_reauthentication(&mut transaction, proof, now).await?;
        let user_id = proof.user_id.as_str();
        let invitation = load_pending_invitation(&mut transaction, token, now).await?;

        let confirmed_email = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM user_emails \
             WHERE user_id = ? AND address_key = ? AND confirmed_at IS NOT NULL)",
        )
        .bind(user_id)
        .bind(&invitation.email_key)
        .fetch_one(&mut *transaction)
        .await?;
        if confirmed_email == 0 {
            return Err(OrganizationInvitationError::EmailNotConfirmed);
        }

        let already_member = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM organization_members \
             WHERE organization_id = 1 AND user_id = ?)",
        )
        .bind(user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if already_member != 0 {
            return Err(OrganizationInvitationError::AlreadyOrganizationMember);
        }

        let accepted = complete_invitation(&mut transaction, user_id, &invitation, now).await?;
        transaction.commit().await?;
        Ok(accepted)
    }

    /// Registers a new account from an invite-only link. Possession of the
    /// invite token proves control of its addressed email under the product's
    /// invitation contract; no second confirmation email is required. Hashing
    /// happens before the writer transaction, then invite consumption, account
    /// creation, memberships, audit rows, and the initial session commit once.
    pub async fn register(
        &self,
        token: &OrganizationInvitationToken,
        display_name: &str,
        password: String,
        password_service: &PasswordService,
        sessions: &SessionStore,
        now: DateTime<Utc>,
    ) -> Result<RegisteredOrganizationInvitee, OrganizationInvitationError> {
        if !self.policy.enabled {
            return Err(OrganizationInvitationError::Disabled);
        }
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return Err(OrganizationInvitationError::InvalidDisplayName);
        }

        let mut preflight = self.pool.begin().await?;
        let invitation = load_pending_invitation(&mut preflight, token, now).await?;
        ensure_invitation_email_is_unclaimed(&mut preflight, &invitation.email_key).await?;
        preflight.commit().await?;

        let password_hash = password_service.hash(password).await?;
        let user_id = Uuid::now_v7().to_string();
        let email_id = Uuid::now_v7().to_string();
        let now_text = format_timestamp(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let invitation = load_pending_invitation(&mut transaction, token, now).await?;
        ensure_invitation_email_is_unclaimed(&mut transaction, &invitation.email_key).await?;

        sqlx::query(
            "INSERT INTO users \
             (id, display_name, state, created_at, updated_at) \
             VALUES (?, ?, 'active', ?, ?)",
        )
        .bind(&user_id)
        .bind(display_name)
        .bind(&now_text)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO user_emails \
             (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
             VALUES (?, ?, ?, ?, 1, ?, ?)",
        )
        .bind(&email_id)
        .bind(&user_id)
        .bind(&invitation.email)
        .bind(&invitation.email_key)
        .bind(&now_text)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) \
             VALUES (?, ?, ?)",
        )
        .bind(&user_id)
        .bind(&password_hash)
        .bind(&now_text)
        .execute(&mut *transaction)
        .await?;

        for (event_kind, target) in [
            (
                AuditEventKind::EmailAddressAdded,
                AuditEventTarget::email_address(&email_id),
            ),
            (
                AuditEventKind::EmailAddressConfirmed,
                AuditEventTarget::email_address(&email_id),
            ),
        ] {
            audit::append_in_transaction(
                &mut transaction,
                Some(&user_id),
                event_kind,
                AuditEventResult::Success,
                &target,
                now,
            )
            .await?;
        }
        audit::append_in_transaction(
            &mut transaction,
            Some(&user_id),
            AuditEventKind::PasswordChanged,
            AuditEventResult::Success,
            &AuditEventTarget::user(&user_id),
            now,
        )
        .await?;

        let accepted = complete_invitation(&mut transaction, &user_id, &invitation, now).await?;
        let session = sessions
            .issue_for_active_user_in_transaction(&mut transaction, &user_id, now)
            .await?;
        transaction.commit().await?;

        Ok(RegisteredOrganizationInvitee {
            user_id,
            invitation_id: accepted.invitation_id,
            role: accepted.role,
            projects: accepted.projects,
            session,
        })
    }
}

pub(crate) async fn accept_pending_invitation_for_email(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    email_key: &str,
    now: DateTime<Utc>,
) -> Result<bool, OrganizationInvitationError> {
    let now_text = format_timestamp(now);
    let invitation = sqlx::query(
        "SELECT id, email, email_key, role, expires_at, accepted_at, canceled_at \
         FROM organization_invites \
         WHERE organization_id = 1 AND email_key = ? \
           AND accepted_at IS NULL AND canceled_at IS NULL AND expires_at > ? \
         ORDER BY created_at ASC \
         LIMIT 1",
    )
    .bind(email_key)
    .bind(&now_text)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(invitation) = invitation else {
        return Ok(false);
    };
    let id = invitation.try_get::<String, _>("id")?;
    let role = OrganizationInviteRole::parse(&invitation.try_get::<String, _>("role")?)?;
    let projects = load_invitation_projects(transaction, &id).await?;
    let pending = PendingOrganizationInvitation {
        id,
        email: invitation.try_get("email")?,
        email_key: invitation.try_get("email_key")?,
        role,
        projects,
    };
    complete_invitation(transaction, user_id, &pending, now).await?;
    Ok(true)
}

async fn load_pending_invitation(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    token: &OrganizationInvitationToken,
    now: DateTime<Utc>,
) -> Result<PendingOrganizationInvitation, OrganizationInvitationError> {
    let invitation = sqlx::query(
        "SELECT id, email, email_key, role, expires_at, accepted_at, canceled_at \
         FROM organization_invites WHERE organization_id = 1 AND token_hash = ?",
    )
    .bind(token.digest().to_vec())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(invitation) = invitation else {
        return Err(OrganizationInvitationError::InvalidToken);
    };
    if invitation
        .try_get::<Option<String>, _>("accepted_at")?
        .is_some()
    {
        return Err(OrganizationInvitationError::InvitationAlreadyAccepted);
    }
    if invitation
        .try_get::<Option<String>, _>("canceled_at")?
        .is_some()
    {
        return Err(OrganizationInvitationError::InvitationNotPending);
    }

    let expires_at = parse_timestamp(&invitation.try_get::<String, _>("expires_at")?)?;
    if expires_at <= now {
        return Err(OrganizationInvitationError::InvitationExpired);
    }
    let id = invitation.try_get::<String, _>("id")?;
    let role = OrganizationInviteRole::parse(&invitation.try_get::<String, _>("role")?)?;
    let projects = load_invitation_projects(transaction, &id).await?;
    if role == OrganizationInviteRole::Guest && projects.is_empty() {
        return Err(OrganizationInvitationError::CorruptState);
    }
    if role == OrganizationInviteRole::Guest
        && projects
            .iter()
            .any(|assignment| assignment.role == ProjectRole::Manager)
    {
        return Err(OrganizationInvitationError::CorruptState);
    }
    for assignment in &projects {
        ensure_project_is_active(transaction, &assignment.project_id).await?;
    }

    Ok(PendingOrganizationInvitation {
        id,
        email: invitation.try_get("email")?,
        email_key: invitation.try_get("email_key")?,
        role,
        projects,
    })
}

async fn ensure_invitation_email_is_unclaimed(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    email_key: &str,
) -> Result<(), OrganizationInvitationError> {
    let already_registered = sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS(SELECT 1 FROM user_emails WHERE address_key = ?)",
    )
    .bind(email_key)
    .fetch_one(&mut **transaction)
    .await?;
    if already_registered != 0 {
        return Err(OrganizationInvitationError::AccountAlreadyExists);
    }
    Ok(())
}

async fn complete_invitation(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    invitation: &PendingOrganizationInvitation,
    now: DateTime<Utc>,
) -> Result<AcceptedOrganizationInvitation, OrganizationInvitationError> {
    let now_text = format_timestamp(now);
    let consumed = sqlx::query(
        "UPDATE organization_invites SET accepted_at = ? \
         WHERE id = ? AND organization_id = 1 AND accepted_at IS NULL \
           AND canceled_at IS NULL AND expires_at > ?",
    )
    .bind(&now_text)
    .bind(&invitation.id)
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;
    if consumed.rows_affected() != 1 {
        return Err(OrganizationInvitationError::InvitationNotPending);
    }

    sqlx::query(
        "INSERT INTO organization_members \
         (organization_id, user_id, role, created_at) VALUES (1, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(invitation.role.as_str())
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;

    for assignment in &invitation.projects {
        let inserted = sqlx::query(
            "INSERT INTO project_members \
             (project_id, organization_id, user_id, role, created_at) \
             SELECT projects.id, projects.organization_id, ?, ?, ? \
             FROM projects WHERE projects.id = ? \
               AND projects.organization_id = 1 AND projects.archived_at IS NULL",
        )
        .bind(user_id)
        .bind(project_role_as_str(assignment.role))
        .bind(&now_text)
        .bind(&assignment.project_id)
        .execute(&mut **transaction)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(OrganizationInvitationError::ProjectUnavailable);
        }
    }

    append_invite_event(
        transaction,
        user_id,
        AuditEventKind::OrganizationInviteAccepted,
        &invitation.id,
        now,
    )
    .await?;
    audit::append_in_transaction(
        transaction,
        Some(user_id),
        AuditEventKind::OrganizationMemberAdded,
        AuditEventResult::Success,
        &AuditEventTarget::organization_membership(
            user_id,
            None,
            Some(invitation.role.organization_role()),
        ),
        now,
    )
    .await?;
    for assignment in &invitation.projects {
        audit::append_in_transaction(
            transaction,
            Some(user_id),
            AuditEventKind::ProjectMemberAdded,
            AuditEventResult::Success,
            &AuditEventTarget::project_membership(
                &assignment.project_id,
                user_id,
                None,
                Some(assignment.role),
            ),
            now,
        )
        .await?;
    }

    Ok(AcceptedOrganizationInvitation {
        invitation_id: invitation.id.clone(),
        role: invitation.role,
        projects: invitation.projects.clone(),
    })
}

async fn load_organization_context(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
) -> Result<AccessContext, OrganizationInvitationError> {
    AccessStore::load_organization_context_in_transaction(transaction, actor_user_id, false)
        .await?
        .ok_or(OrganizationInvitationError::Forbidden)
}

fn is_organization_admin(context: &AccessContext) -> bool {
    matches!(
        context.organization_role(),
        Some(OrganizationRole::Owner | OrganizationRole::Admin)
    ) && authorize(context, AccessCapability::ManageOrganizationMembership).is_ok()
}

async fn ensure_project_is_active(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
) -> Result<(), OrganizationInvitationError> {
    let archived_at = sqlx::query_scalar::<_, Option<String>>(
        "SELECT archived_at FROM projects WHERE id = ? AND organization_id = 1",
    )
    .bind(project_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if !matches!(archived_at, Some(None)) {
        return Err(OrganizationInvitationError::ProjectUnavailable);
    }
    Ok(())
}

async fn require_current_reauthentication(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    proof: &RecentReauthentication,
    now: DateTime<Utc>,
) -> Result<(), OrganizationInvitationError> {
    if !proof.is_current_at(now) {
        return Err(OrganizationInvitationError::ReauthenticationRequired);
    }
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM users WHERE id = ?")
        .bind(&proof.user_id)
        .fetch_optional(&mut **transaction)
        .await?;
    if state.as_deref() != Some("active") {
        return Err(OrganizationInvitationError::UserNotActive);
    }

    let valid_session = sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS( \
             SELECT 1 FROM sessions \
             WHERE id = ? AND user_id = ? AND token_hash = ? \
               AND reauthenticated_at = ? AND revoked_at IS NULL \
               AND expires_at > ? \
         )",
    )
    .bind(&proof.session_id)
    .bind(&proof.user_id)
    .bind(&proof.token_digest)
    .bind(format_timestamp(proof.authenticated_at))
    .bind(format_timestamp(now))
    .fetch_one(&mut **transaction)
    .await?;
    if valid_session != 1 {
        return Err(OrganizationInvitationError::ReauthenticationRequired);
    }
    Ok(())
}

async fn load_invitation_projects(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    invitation_id: &str,
) -> Result<Vec<InvitationProjectAssignment>, OrganizationInvitationError> {
    let rows = sqlx::query(
        "SELECT project_id, role FROM organization_invite_projects \
         WHERE invite_id = ? ORDER BY project_id",
    )
    .bind(invitation_id)
    .fetch_all(&mut **transaction)
    .await?;

    let mut projects = Vec::with_capacity(rows.len());
    for row in rows {
        projects.push(InvitationProjectAssignment {
            project_id: row.try_get("project_id")?,
            role: parse_project_role(&row.try_get::<String, _>("role")?)?,
        });
    }
    Ok(projects)
}

async fn append_invite_event(
    transaction: &mut Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    event_kind: AuditEventKind,
    invitation_id: &str,
    now: DateTime<Utc>,
) -> Result<(), AuditEventError> {
    audit::append_in_transaction(
        transaction,
        Some(actor_user_id),
        event_kind,
        AuditEventResult::Success,
        &AuditEventTarget::organization_invite(invitation_id),
        now,
    )
    .await?;
    Ok(())
}

fn invitation_status(
    accepted_at: Option<DateTime<Utc>>,
    canceled_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> OrganizationInvitationStatus {
    if accepted_at.is_some() {
        return OrganizationInvitationStatus::Accepted;
    }
    if canceled_at.is_some() {
        return OrganizationInvitationStatus::Canceled;
    }
    if expires_at <= now {
        return OrganizationInvitationStatus::Expired;
    }
    OrganizationInvitationStatus::Pending
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, OrganizationInvitationError> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| OrganizationInvitationError::CorruptState)
}

fn parse_optional_timestamp(
    value: Option<String>,
) -> Result<Option<DateTime<Utc>>, OrganizationInvitationError> {
    value
        .map(|timestamp| parse_timestamp(&timestamp))
        .transpose()
}

fn parse_project_role(value: &str) -> Result<ProjectRole, OrganizationInvitationError> {
    match value {
        "manager" => Ok(ProjectRole::Manager),
        "member" => Ok(ProjectRole::Member),
        "viewer" => Ok(ProjectRole::Viewer),
        _ => Err(OrganizationInvitationError::CorruptState),
    }
}

fn project_role_as_str(role: ProjectRole) -> &'static str {
    match role {
        ProjectRole::Manager => "manager",
        ProjectRole::Member => "member",
        ProjectRole::Viewer => "viewer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_token_is_canonical_and_digest_is_domain_separated() {
        let token = OrganizationInvitationToken::generate().unwrap();
        let parsed = OrganizationInvitationToken::parse(token.expose()).unwrap();
        assert_eq!(token.digest(), parsed.digest());
        assert_eq!(token.expose(), parsed.expose());
        assert_eq!(token.expose().len(), 43);
        assert_ne!(
            Sha256::digest(token.expose().as_bytes()).as_slice(),
            token.digest()
        );
        assert_eq!(
            OrganizationInvitationToken::parse("not-a-token").err(),
            Some(InvitationTokenError::InvalidEncoding)
        );
    }

    #[test]
    fn invitation_policy_rejects_non_positive_lifetimes() {
        assert!(matches!(
            InvitationPolicy::new(true, TimeDelta::zero()),
            Err(OrganizationInvitationError::InvalidPolicy)
        ));
        assert!(matches!(
            InvitationPolicy::new(true, TimeDelta::seconds(-1)),
            Err(OrganizationInvitationError::InvalidPolicy)
        ));
    }
}
