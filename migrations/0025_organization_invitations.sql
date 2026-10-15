CREATE TABLE organization_invites (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    organization_id INTEGER NOT NULL DEFAULT 1
        REFERENCES organizations (id) ON DELETE RESTRICT,
    email TEXT NOT NULL CHECK (email = trim(email) AND length(trim(email)) > 0),
    email_key TEXT NOT NULL CHECK (email_key = lower(trim(email))),
    role TEXT NOT NULL CHECK (role IN ('member', 'guest')),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    created_by_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    accepted_at TEXT,
    canceled_at TEXT,
    CHECK (accepted_at IS NULL OR canceled_at IS NULL),
    UNIQUE (id, organization_id)
);

CREATE UNIQUE INDEX organization_invites_one_unresolved_email_idx
    ON organization_invites (organization_id, email_key)
    WHERE accepted_at IS NULL AND canceled_at IS NULL;

CREATE INDEX organization_invites_created_idx
    ON organization_invites (organization_id, created_at DESC, id DESC);

CREATE TABLE organization_invite_projects (
    invite_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    organization_id INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('manager', 'member', 'viewer')),
    PRIMARY KEY (invite_id, project_id),
    FOREIGN KEY (invite_id, organization_id)
        REFERENCES organization_invites (id, organization_id) ON DELETE RESTRICT,
    FOREIGN KEY (project_id, organization_id)
        REFERENCES projects (id, organization_id) ON DELETE RESTRICT
);

CREATE INDEX organization_invite_projects_project_idx
    ON organization_invite_projects (project_id, invite_id);

CREATE TRIGGER organization_invite_guest_cannot_be_project_manager
BEFORE INSERT ON organization_invite_projects
WHEN NEW.role = 'manager'
 AND EXISTS (
     SELECT 1 FROM organization_invites
     WHERE organization_invites.id = NEW.invite_id
       AND organization_invites.role = 'guest'
 )
BEGIN
    SELECT RAISE(ABORT, 'organization guest invites cannot grant project-manager access');
END;

CREATE TRIGGER organization_invites_identity_is_immutable
BEFORE UPDATE OF
    id, organization_id, email, email_key, role, token_hash,
    created_by_user_id, created_at, expires_at
ON organization_invites
BEGIN
    SELECT RAISE(ABORT, 'organization invite identity is immutable');
END;

CREATE TRIGGER organization_invites_lifecycle_is_one_way
BEFORE UPDATE OF accepted_at, canceled_at ON organization_invites
WHEN OLD.accepted_at IS NOT NULL
  OR OLD.canceled_at IS NOT NULL
  OR NOT (
      (NEW.accepted_at IS NOT NULL AND NEW.canceled_at IS NULL)
      OR (NEW.accepted_at IS NULL AND NEW.canceled_at IS NOT NULL)
  )
BEGIN
    SELECT RAISE(ABORT, 'organization invite lifecycle is one-way');
END;

CREATE TRIGGER organization_invites_cannot_be_deleted
BEFORE DELETE ON organization_invites
BEGIN
    SELECT RAISE(ABORT, 'organization invites must be retained');
END;

CREATE TRIGGER organization_invite_projects_are_immutable
BEFORE UPDATE ON organization_invite_projects
BEGIN
    SELECT RAISE(ABORT, 'organization invite project assignments are immutable');
END;

CREATE TRIGGER organization_invite_projects_cannot_be_deleted
BEFORE DELETE ON organization_invite_projects
BEGIN
    SELECT RAISE(ABORT, 'organization invite project assignments must be retained');
END;

DROP TRIGGER audit_events_no_update;
DROP TRIGGER audit_events_no_delete;
DROP INDEX audit_events_occurred_idx;
DROP INDEX audit_events_actor_idx;
DROP INDEX audit_events_project_subject_idx;
ALTER TABLE audit_events RENAME TO audit_events_v24;

CREATE TABLE audit_events (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'auth.email.address_added',
        'auth.email.address_confirmed',
        'auth.email.primary_changed',
        'auth.email.address_removed',
        'auth.password.reset_completed',
        'auth.password.changed',
        'auth.session.revoked',
        'auth.sessions.revoked',
        'project.created',
        'auth.mfa.totp_enrollment_started',
        'auth.mfa.totp_enrollment_confirmed',
        'auth.mfa.totp_enrollment_expired',
        'auth.mfa.totp_verified',
        'auth.session.mfa_elevated',
        'auth.mfa.totp_disabled',
        'auth.mfa.recovery_started',
        'auth.mfa.recovery_enrollment_started',
        'auth.mfa.recovery_completed',
        'project.member.added',
        'project.member.role_changed',
        'project.member.removed',
        'organization.member.added',
        'organization.member.role_changed',
        'organization.member.removed',
        'organization.invite.created',
        'organization.invite.reissued',
        'organization.invite.canceled',
        'organization.invite.accepted'
    )),
    result TEXT NOT NULL CHECK (result IN ('success', 'failure')),
    actor_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    target_kind TEXT NOT NULL CHECK (
        target_kind IN (
            'user', 'email_address', 'session', 'project', 'mfa_credential',
            'mfa_recovery_session', 'invite'
        )
    ),
    target_id TEXT NOT NULL CHECK (length(target_id) = 36),
    subject_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    project_role_before TEXT,
    project_role_after TEXT,
    organization_role_before TEXT,
    organization_role_after TEXT,
    occurred_at TEXT NOT NULL,
    CHECK (
        (
            event_kind = 'project.member.added'
            AND target_kind = 'project'
            AND subject_user_id IS NOT NULL
            AND project_role_before IS NULL
            AND project_role_after IS NOT NULL
            AND project_role_after IN ('manager', 'member', 'viewer')
            AND organization_role_before IS NULL
            AND organization_role_after IS NULL
        )
        OR (
            event_kind = 'project.member.role_changed'
            AND target_kind = 'project'
            AND subject_user_id IS NOT NULL
            AND project_role_before IS NOT NULL
            AND project_role_before IN ('manager', 'member', 'viewer')
            AND project_role_after IS NOT NULL
            AND project_role_after IN ('manager', 'member', 'viewer')
            AND project_role_before != project_role_after
            AND organization_role_before IS NULL
            AND organization_role_after IS NULL
        )
        OR (
            event_kind = 'project.member.removed'
            AND target_kind = 'project'
            AND subject_user_id IS NOT NULL
            AND project_role_before IS NOT NULL
            AND project_role_before IN ('manager', 'member', 'viewer')
            AND project_role_after IS NULL
            AND organization_role_before IS NULL
            AND organization_role_after IS NULL
        )
        OR (
            event_kind = 'organization.member.added'
            AND target_kind = 'user'
            AND subject_user_id IS NOT NULL
            AND target_id = subject_user_id
            AND project_role_before IS NULL
            AND project_role_after IS NULL
            AND organization_role_before IS NULL
            AND organization_role_after IS NOT NULL
            AND organization_role_after IN ('owner', 'admin', 'member', 'guest')
        )
        OR (
            event_kind = 'organization.member.role_changed'
            AND target_kind = 'user'
            AND subject_user_id IS NOT NULL
            AND target_id = subject_user_id
            AND project_role_before IS NULL
            AND project_role_after IS NULL
            AND organization_role_before IS NOT NULL
            AND organization_role_before IN ('owner', 'admin', 'member', 'guest')
            AND organization_role_after IS NOT NULL
            AND organization_role_after IN ('owner', 'admin', 'member', 'guest')
            AND organization_role_before != organization_role_after
        )
        OR (
            event_kind = 'organization.member.removed'
            AND target_kind = 'user'
            AND subject_user_id IS NOT NULL
            AND target_id = subject_user_id
            AND project_role_before IS NULL
            AND project_role_after IS NULL
            AND organization_role_before IS NOT NULL
            AND organization_role_before IN ('owner', 'admin', 'member', 'guest')
            AND organization_role_after IS NULL
        )
        OR (
            event_kind IN (
                'organization.invite.created',
                'organization.invite.reissued',
                'organization.invite.canceled',
                'organization.invite.accepted'
            )
            AND target_kind = 'invite'
            AND subject_user_id IS NULL
            AND project_role_before IS NULL
            AND project_role_after IS NULL
            AND organization_role_before IS NULL
            AND organization_role_after IS NULL
        )
        OR (
            event_kind NOT IN (
                'project.member.added',
                'project.member.role_changed',
                'project.member.removed',
                'organization.member.added',
                'organization.member.role_changed',
                'organization.member.removed',
                'organization.invite.created',
                'organization.invite.reissued',
                'organization.invite.canceled',
                'organization.invite.accepted'
            )
            AND subject_user_id IS NULL
            AND project_role_before IS NULL
            AND project_role_after IS NULL
            AND organization_role_before IS NULL
            AND organization_role_after IS NULL
        )
    ),
    CHECK (
        subject_user_id IS NULL
        OR (
            target_kind = 'project'
            AND event_kind IN (
                'project.member.added',
                'project.member.role_changed',
                'project.member.removed'
            )
        )
        OR (
            target_kind = 'user'
            AND target_id = subject_user_id
            AND event_kind IN (
                'organization.member.added',
                'organization.member.role_changed',
                'organization.member.removed'
            )
        )
    ),
    CHECK (
        target_kind != 'invite'
        OR event_kind IN (
            'organization.invite.created',
            'organization.invite.reissued',
            'organization.invite.canceled',
            'organization.invite.accepted'
        )
    )
);

INSERT INTO audit_events (
    id, event_kind, result, actor_user_id, target_kind, target_id,
    subject_user_id, project_role_before, project_role_after,
    organization_role_before, organization_role_after, occurred_at
)
SELECT
    id, event_kind, result, actor_user_id, target_kind, target_id,
    subject_user_id, project_role_before, project_role_after,
    organization_role_before, organization_role_after, occurred_at
FROM audit_events_v24;

DROP TABLE audit_events_v24;

CREATE INDEX audit_events_occurred_idx ON audit_events (occurred_at DESC, id DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, occurred_at DESC, id DESC);
CREATE INDEX audit_events_project_subject_idx
    ON audit_events (target_id, subject_user_id, occurred_at DESC, id DESC)
    WHERE target_kind = 'project' AND subject_user_id IS NOT NULL;

CREATE TRIGGER audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;

CREATE TRIGGER audit_events_no_delete
BEFORE DELETE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;
