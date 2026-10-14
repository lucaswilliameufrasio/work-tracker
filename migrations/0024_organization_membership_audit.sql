DROP TRIGGER audit_events_no_update;
DROP TRIGGER audit_events_no_delete;
DROP INDEX audit_events_occurred_idx;
DROP INDEX audit_events_actor_idx;
DROP INDEX audit_events_project_subject_idx;
ALTER TABLE audit_events RENAME TO audit_events_v23;

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
        'organization.member.role_changed',
        'organization.member.removed'
    )),
    result TEXT NOT NULL CHECK (result IN ('success', 'failure')),
    actor_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    target_kind TEXT NOT NULL CHECK (
        target_kind IN (
            'user', 'email_address', 'session', 'project', 'mfa_credential',
            'mfa_recovery_session'
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
            event_kind NOT IN (
                'project.member.added',
                'project.member.role_changed',
                'project.member.removed',
                'organization.member.role_changed',
                'organization.member.removed'
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
                'organization.member.role_changed',
                'organization.member.removed'
            )
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
    NULL, NULL, occurred_at
FROM audit_events_v23;

DROP TABLE audit_events_v23;

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
