CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    job_kind TEXT NOT NULL CHECK (
        length(job_kind) BETWEEN 1 AND 64
        AND substr(job_kind, 1, 1) GLOB '[a-z0-9]'
        AND job_kind NOT GLOB '*[^a-z0-9._-]*'
    ),
    payload_version INTEGER NOT NULL CHECK (
        typeof(payload_version) = 'integer' AND payload_version > 0
    ),
    payload_json TEXT NOT NULL CHECK (
        length(CAST(payload_json AS BLOB)) <= 65536
        AND json_valid(payload_json)
        AND CASE WHEN json_valid(payload_json)
            THEN json_type(payload_json) = 'object'
            ELSE 0
        END
    ),
    dedupe_key TEXT NOT NULL CHECK (
        length(CAST(dedupe_key AS BLOB)) BETWEEN 1 AND 255
        AND dedupe_key NOT GLOB '*[^!-~]*'
    ),
    source_outbox_event_id TEXT REFERENCES outbox_events (id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (
        state IN ('pending', 'running', 'succeeded', 'failed', 'canceled')
    ),
    available_at TEXT NOT NULL,
    attempts INTEGER NOT NULL CHECK (
        typeof(attempts) = 'integer' AND attempts BETWEEN 0 AND 65535
    ),
    max_attempts INTEGER NOT NULL CHECK (
        typeof(max_attempts) = 'integer' AND max_attempts BETWEEN 1 AND 65535
    ),
    claim_epoch INTEGER NOT NULL CHECK (
        typeof(claim_epoch) = 'integer' AND claim_epoch >= 0
    ),
    claim_token TEXT CHECK (
        claim_token IS NULL OR length(claim_token) = 36
    ),
    lease_expires_at TEXT,
    last_error_code TEXT CHECK (
        last_error_code IS NULL OR (
            length(last_error_code) BETWEEN 1 AND 64
            AND substr(last_error_code, 1, 1) GLOB '[a-z0-9]'
            AND last_error_code NOT GLOB '*[^a-z0-9._-]*'
        )
    ),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT,
    UNIQUE (dedupe_key),
    CHECK (attempts <= max_attempts),
    CHECK (
        (state = 'pending' AND attempts < max_attempts)
        OR (state = 'running' AND attempts > 0 AND claim_epoch > 0)
        OR (state IN ('succeeded', 'failed') AND attempts > 0)
        OR state = 'canceled'
    ),
    CHECK (
        (state = 'running' AND claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
        OR (state != 'running' AND claim_token IS NULL AND lease_expires_at IS NULL)
    ),
    CHECK (
        (state IN ('succeeded', 'failed', 'canceled') AND finished_at IS NOT NULL)
        OR (state IN ('pending', 'running') AND finished_at IS NULL)
    )
);

CREATE INDEX jobs_pending_available_idx ON jobs (available_at, id)
    WHERE state = 'pending';
CREATE INDEX jobs_running_lease_idx ON jobs (lease_expires_at, id)
    WHERE state = 'running';
CREATE INDEX jobs_source_event_idx ON jobs (source_outbox_event_id, id)
    WHERE source_outbox_event_id IS NOT NULL;

CREATE TRIGGER job_identity_is_immutable
BEFORE UPDATE OF
    id, job_kind, payload_version, payload_json, dedupe_key,
    source_outbox_event_id, max_attempts, created_at
ON jobs
BEGIN
    SELECT RAISE(ABORT, 'job identity and payload are immutable');
END;

CREATE TRIGGER job_state_transition_is_valid
BEFORE UPDATE OF state ON jobs
WHEN NOT (
    (OLD.state = 'pending' AND NEW.state IN ('running', 'canceled'))
    OR (OLD.state = 'running' AND NEW.state IN (
        'pending', 'succeeded', 'failed', 'canceled'
    ))
)
BEGIN
    SELECT RAISE(ABORT, 'invalid job state transition');
END;
