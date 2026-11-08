CREATE TABLE issue_events_v37 (
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
        field_key IS NULL OR field_key IN (
            'priority', 'due_date', 'assignee_user_id'
        )
    ),
    before_value_json TEXT CHECK (
        before_value_json IS NULL OR (
            length(CAST(before_value_json AS BLOB)) <= 4096
            AND json_valid(before_value_json)
            AND CASE WHEN json_valid(before_value_json)
                THEN CASE field_key
                    WHEN 'priority' THEN json_type(before_value_json) = 'text'
                        AND json_extract(before_value_json, '$') IN (
                            'none', 'low', 'medium', 'high', 'urgent'
                        )
                    WHEN 'due_date' THEN json_type(before_value_json) = 'null'
                        OR (
                            json_type(before_value_json) = 'text'
                            AND length(json_extract(before_value_json, '$')) = 10
                            AND json_extract(before_value_json, '$') GLOB
                                '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
                        )
                    WHEN 'assignee_user_id' THEN json_type(before_value_json) = 'null'
                        OR (
                            json_type(before_value_json) = 'text'
                            AND length(json_extract(before_value_json, '$')) = 36
                            AND substr(json_extract(before_value_json, '$'), 9, 1) = '-'
                            AND substr(json_extract(before_value_json, '$'), 14, 1) = '-'
                            AND substr(json_extract(before_value_json, '$'), 19, 1) = '-'
                            AND substr(json_extract(before_value_json, '$'), 24, 1) = '-'
                            AND length(replace(json_extract(before_value_json, '$'), '-', '')) = 32
                            AND replace(json_extract(before_value_json, '$'), '-', '')
                                NOT GLOB '*[^0-9a-f]*'
                        )
                    ELSE 0
                END
                ELSE 0
            END
        )
    ),
    after_value_json TEXT CHECK (
        after_value_json IS NULL OR (
            length(CAST(after_value_json AS BLOB)) <= 4096
            AND json_valid(after_value_json)
            AND CASE WHEN json_valid(after_value_json)
                THEN CASE field_key
                    WHEN 'priority' THEN json_type(after_value_json) = 'text'
                        AND json_extract(after_value_json, '$') IN (
                            'none', 'low', 'medium', 'high', 'urgent'
                        )
                    WHEN 'due_date' THEN json_type(after_value_json) = 'null'
                        OR (
                            json_type(after_value_json) = 'text'
                            AND length(json_extract(after_value_json, '$')) = 10
                            AND json_extract(after_value_json, '$') GLOB
                                '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
                        )
                    WHEN 'assignee_user_id' THEN json_type(after_value_json) = 'null'
                        OR (
                            json_type(after_value_json) = 'text'
                            AND length(json_extract(after_value_json, '$')) = 36
                            AND substr(json_extract(after_value_json, '$'), 9, 1) = '-'
                            AND substr(json_extract(after_value_json, '$'), 14, 1) = '-'
                            AND substr(json_extract(after_value_json, '$'), 19, 1) = '-'
                            AND substr(json_extract(after_value_json, '$'), 24, 1) = '-'
                            AND length(replace(json_extract(after_value_json, '$'), '-', '')) = 32
                            AND replace(json_extract(after_value_json, '$'), '-', '')
                                NOT GLOB '*[^0-9a-f]*'
                        )
                    ELSE 0
                END
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
INSERT INTO issue_events_v37 (
    id, project_id, issue_id, actor_user_id, event_kind, occurred_at,
    field_key, before_value_json, after_value_json
)
SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at,
       field_key, before_value_json, after_value_json
FROM issue_events;
DROP TABLE issue_events;
ALTER TABLE issue_events_v37 RENAME TO issue_events;
CREATE UNIQUE INDEX issue_events_one_creation_idx
    ON issue_events (issue_id) WHERE event_kind = 'issue.created';
CREATE INDEX issue_events_issue_time_idx
    ON issue_events (project_id, issue_id, occurred_at DESC, id DESC);

CREATE TABLE issue_assignee_receipts (
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    operation TEXT NOT NULL CHECK (operation = 'issue.set_assignee'),
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
    result_assignee_user_id TEXT CHECK (
        result_assignee_user_id IS NULL OR (
            length(result_assignee_user_id) = 36
            AND substr(result_assignee_user_id, 9, 1) = '-'
            AND substr(result_assignee_user_id, 14, 1) = '-'
            AND substr(result_assignee_user_id, 19, 1) = '-'
            AND substr(result_assignee_user_id, 24, 1) = '-'
            AND length(replace(result_assignee_user_id, '-', '')) = 32
            AND replace(result_assignee_user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
        )
    ),
    result_version INTEGER NOT NULL CHECK (
        typeof(result_version) = 'integer' AND result_version >= 1
    ),
    changed INTEGER NOT NULL CHECK (changed IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (actor_user_id, operation, key_digest)
);
CREATE TRIGGER issue_assignee_receipt_is_immutable
BEFORE UPDATE ON issue_assignee_receipts
BEGIN
    SELECT RAISE(ABORT, 'assignee receipts are immutable');
END;
CREATE TRIGGER issue_assignee_receipt_cannot_be_deleted
BEFORE DELETE ON issue_assignee_receipts
BEGIN
    SELECT RAISE(ABORT, 'assignee receipt slots cannot be reused');
END;
