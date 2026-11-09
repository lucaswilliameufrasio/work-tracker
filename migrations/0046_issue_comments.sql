CREATE TABLE issue_comments (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    author_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    body TEXT NOT NULL CHECK (
        length(trim(body)) > 0
        AND length(CAST(body AS BLOB)) <= 65536
    ),
    version INTEGER NOT NULL DEFAULT 1 CHECK (
        typeof(version) = 'integer' AND version >= 1
    ),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (project_id, issue_id)
        REFERENCES issues (project_id, id) ON DELETE RESTRICT
);

CREATE INDEX issue_comments_issue_idx
    ON issue_comments (project_id, issue_id, id);

CREATE TABLE issue_comment_events (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    comment_id TEXT NOT NULL CHECK (length(comment_id) = 36),
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (
        event_kind IN ('comment.created', 'comment.edited', 'comment.removed')
    ),
    occurred_at TEXT NOT NULL,
    FOREIGN KEY (project_id, issue_id)
        REFERENCES issues (project_id, id) ON DELETE RESTRICT
);

CREATE INDEX issue_comment_events_issue_idx
    ON issue_comment_events (project_id, issue_id, id);

CREATE TABLE issue_comment_receipts (
    actor_user_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (
        operation IN ('comment.create', 'comment.edit', 'comment.remove')
    ),
    key_digest BLOB NOT NULL CHECK (length(key_digest) = 32),
    request_fingerprint BLOB NOT NULL CHECK (length(request_fingerprint) = 32),
    comment_id TEXT NOT NULL CHECK (length(comment_id) = 36),
    result_version INTEGER CHECK (
        result_version IS NULL
        OR (typeof(result_version) = 'integer' AND result_version >= 1)
    ),
    changed INTEGER NOT NULL CHECK (changed IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (actor_user_id, operation, key_digest)
);
