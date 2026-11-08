CREATE TABLE issue_events_v35 (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'issue.created', 'issue.title_updated', 'issue.description_updated',
        'issue.text_updated', 'issue.moved', 'issue.field_updated'
    )),
    occurred_at TEXT NOT NULL,
    field_key TEXT CHECK (
        field_key IS NULL OR field_key = 'priority'
    ),
    before_value_json TEXT CHECK (
        before_value_json IS NULL OR (
            length(CAST(before_value_json AS BLOB)) <= 4096
            AND json_valid(before_value_json)
            AND CASE WHEN json_valid(before_value_json)
                THEN json_type(before_value_json) = 'text'
                    AND json_extract(before_value_json, '$') IN (
                        'none', 'low', 'medium', 'high', 'urgent'
                    )
                ELSE 0
            END
        )
    ),
    after_value_json TEXT CHECK (
        after_value_json IS NULL OR (
            length(CAST(after_value_json AS BLOB)) <= 4096
            AND json_valid(after_value_json)
            AND CASE WHEN json_valid(after_value_json)
                THEN json_type(after_value_json) = 'text'
                    AND json_extract(after_value_json, '$') IN (
                        'none', 'low', 'medium', 'high', 'urgent'
                    )
                ELSE 0
            END
        )
    ),
    CHECK (
        (event_kind = 'issue.field_updated'
            AND field_key IS NOT NULL
            AND before_value_json IS NOT NULL
            AND after_value_json IS NOT NULL
            AND before_value_json != after_value_json)
        OR
        (event_kind != 'issue.field_updated'
            AND field_key IS NULL
            AND before_value_json IS NULL
            AND after_value_json IS NULL)
    ),
    FOREIGN KEY (project_id, issue_id)
        REFERENCES issues (project_id, id) ON DELETE RESTRICT
);
INSERT INTO issue_events_v35 (
    id, project_id, issue_id, actor_user_id, event_kind, occurred_at
)
SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at
FROM issue_events;
DROP TABLE issue_events;
ALTER TABLE issue_events_v35 RENAME TO issue_events;
CREATE UNIQUE INDEX issue_events_one_creation_idx
    ON issue_events (issue_id) WHERE event_kind = 'issue.created';
CREATE INDEX issue_events_issue_time_idx
    ON issue_events (project_id, issue_id, occurred_at DESC, id DESC);

CREATE TABLE outbox_events_v35 (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'issue.created', 'issue.title_updated', 'issue.description_updated',
        'issue.text_updated', 'issue.moved', 'issue.field_updated'
    )),
    payload_version INTEGER NOT NULL CHECK (payload_version = 1),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    issue_version INTEGER NOT NULL CHECK (
        typeof(issue_version) = 'integer' AND (
            (event_kind = 'issue.created' AND issue_version = 1)
            OR (event_kind != 'issue.created' AND issue_version >= 2)
        )
    ),
    occurred_at TEXT NOT NULL,
    UNIQUE (issue_id, issue_version, event_kind),
    FOREIGN KEY (project_id, issue_id)
        REFERENCES issues (project_id, id) ON DELETE RESTRICT
);
INSERT INTO outbox_events_v35 (
    id, event_kind, payload_version, project_id, issue_id, actor_user_id,
    issue_version, occurred_at
)
SELECT id, event_kind, payload_version, project_id, issue_id, actor_user_id,
       issue_version, occurred_at
FROM outbox_events;
DROP TABLE outbox_events;
ALTER TABLE outbox_events_v35 RENAME TO outbox_events;
CREATE INDEX outbox_events_time_idx ON outbox_events (occurred_at, id);
CREATE TRIGGER outbox_event_payload_is_immutable
BEFORE UPDATE ON outbox_events
BEGIN
    SELECT RAISE(ABORT, 'outbox event envelopes are immutable');
END;

CREATE TABLE issue_priority_receipts (
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    operation TEXT NOT NULL CHECK (operation = 'issue.set_priority'),
    key_digest BLOB NOT NULL CHECK (
        typeof(key_digest) = 'blob' AND length(key_digest) = 32
    ),
    request_fingerprint BLOB NOT NULL CHECK (
        typeof(request_fingerprint) = 'blob' AND length(request_fingerprint) = 32
    ),
    master_key_id BLOB NOT NULL CHECK (
        typeof(master_key_id) = 'blob' AND length(master_key_id) = 16
    ),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    issue_id TEXT NOT NULL CHECK (length(issue_id) = 36),
    issue_number INTEGER NOT NULL CHECK (
        typeof(issue_number) = 'integer' AND issue_number > 0
    ),
    result_priority TEXT NOT NULL CHECK (
        result_priority IN ('none', 'low', 'medium', 'high', 'urgent')
    ),
    result_version INTEGER NOT NULL CHECK (
        typeof(result_version) = 'integer' AND result_version >= 1
    ),
    changed INTEGER NOT NULL CHECK (changed IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (actor_user_id, operation, key_digest)
);
CREATE TRIGGER issue_priority_receipt_is_immutable
BEFORE UPDATE ON issue_priority_receipts
BEGIN
    SELECT RAISE(ABORT, 'priority receipts are immutable');
END;
CREATE TRIGGER issue_priority_receipt_cannot_be_deleted
BEFORE DELETE ON issue_priority_receipts
BEGIN
    SELECT RAISE(ABORT, 'priority receipt slots cannot be reused');
END;
