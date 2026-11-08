CREATE TABLE mfa_recovery_enrollments (
    credential_id TEXT PRIMARY KEY NOT NULL CHECK (length(credential_id) = 36),
    recovery_session_id TEXT NOT NULL UNIQUE
        REFERENCES mfa_recovery_sessions (id) ON DELETE RESTRICT,
    secret_envelope BLOB NOT NULL CHECK (
        typeof(secret_envelope) = 'blob' AND length(secret_envelope) = 65
    ),
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    CHECK (expires_at > created_at)
);

CREATE INDEX mfa_recovery_enrollments_expiry_idx
    ON mfa_recovery_enrollments (expires_at);

DROP TRIGGER audit_events_no_update;
DROP INDEX audit_events_occurred_idx;
DROP INDEX audit_events_actor_idx;
ALTER TABLE audit_events RENAME TO audit_events_v19;

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
        'auth.mfa.recovery_enrollment_started'
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
    occurred_at TEXT NOT NULL
);

INSERT INTO audit_events (
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
)
SELECT
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
FROM audit_events_v19;

DROP TABLE audit_events_v19;

CREATE INDEX audit_events_occurred_idx ON audit_events (occurred_at DESC, id DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, occurred_at DESC, id DESC);

CREATE TRIGGER audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;
