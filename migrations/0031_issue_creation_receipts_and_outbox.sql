CREATE TABLE issue_creation_receipts (
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    operation TEXT NOT NULL CHECK (operation = 'issue.create'),
    key_digest BLOB NOT NULL CHECK (typeof(key_digest) = 'blob' AND length(key_digest) = 32),
    request_fingerprint BLOB NOT NULL CHECK (typeof(request_fingerprint) = 'blob' AND length(request_fingerprint) = 32),
    master_key_id BLOB NOT NULL CHECK (typeof(master_key_id) = 'blob' AND length(master_key_id) = 16),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    issue_id TEXT NOT NULL CHECK (length(issue_id) = 36),
    issue_number INTEGER NOT NULL CHECK (typeof(issue_number) = 'integer' AND issue_number > 0),
    created_at TEXT NOT NULL,
    PRIMARY KEY (actor_user_id, operation, key_digest)
);

-- An immutable, content-free versioned envelope, not a job/lease or consumer.
CREATE TABLE outbox_events (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    event_kind TEXT NOT NULL CHECK (event_kind = 'issue.created'),
    payload_version INTEGER NOT NULL CHECK (payload_version = 1),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    issue_version INTEGER NOT NULL CHECK (typeof(issue_version) = 'integer' AND issue_version = 1),
    occurred_at TEXT NOT NULL,
    UNIQUE (issue_id, issue_version, event_kind),
    FOREIGN KEY (project_id, issue_id) REFERENCES issues (project_id, id) ON DELETE RESTRICT
);
CREATE INDEX outbox_events_time_idx ON outbox_events (occurred_at, id);

CREATE TRIGGER issue_creation_receipt_is_immutable
BEFORE UPDATE ON issue_creation_receipts
BEGIN
    SELECT RAISE(ABORT, 'creation receipts are immutable');
END;

CREATE TRIGGER issue_creation_receipt_cannot_be_deleted
BEFORE DELETE ON issue_creation_receipts
BEGIN
    SELECT RAISE(ABORT, 'creation receipt slots cannot be reused');
END;

CREATE TRIGGER outbox_event_payload_is_immutable
BEFORE UPDATE ON outbox_events
BEGIN
    SELECT RAISE(ABORT, 'outbox event envelopes are immutable');
END;
