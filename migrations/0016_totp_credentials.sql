CREATE TABLE mfa_credentials (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    user_id TEXT NOT NULL UNIQUE REFERENCES users (id) ON DELETE RESTRICT,
    secret_envelope BLOB NOT NULL CHECK (
        typeof(secret_envelope) = 'blob' AND length(secret_envelope) = 65
    ),
    created_at TEXT NOT NULL,
    expires_at TEXT,
    activated_at TEXT,
    last_accepted_step INTEGER,
    CHECK (expires_at IS NULL OR expires_at > created_at),
    CHECK (
        (expires_at IS NOT NULL AND activated_at IS NULL AND last_accepted_step IS NULL)
        OR (
            expires_at IS NULL
            AND activated_at IS NOT NULL
            AND last_accepted_step IS NOT NULL
            AND last_accepted_step >= 0
        )
    )
);

CREATE INDEX mfa_credentials_pending_expiry_idx
    ON mfa_credentials (expires_at)
    WHERE activated_at IS NULL;

DROP TRIGGER audit_events_no_update;
DROP INDEX audit_events_occurred_idx;
DROP INDEX audit_events_actor_idx;
ALTER TABLE audit_events RENAME TO audit_events_v15;

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
        'auth.mfa.totp_verified'
    )),
    result TEXT NOT NULL CHECK (result IN ('success', 'failure')),
    actor_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('user', 'email_address', 'session', 'project', 'mfa_credential')
    ),
    target_id TEXT NOT NULL CHECK (length(target_id) = 36),
    occurred_at TEXT NOT NULL
);

INSERT INTO audit_events (
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
)
SELECT
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
FROM audit_events_v15;

DROP TABLE audit_events_v15;

CREATE INDEX audit_events_occurred_idx ON audit_events (occurred_at DESC, id DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, occurred_at DESC, id DESC);

CREATE TRIGGER audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;
