CREATE TABLE issue_events_v32 (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'issue.created', 'issue.title_updated', 'issue.description_updated', 'issue.text_updated'
    )),
    occurred_at TEXT NOT NULL,
    FOREIGN KEY (project_id, issue_id) REFERENCES issues (project_id, id) ON DELETE RESTRICT
);
INSERT INTO issue_events_v32 SELECT id, project_id, issue_id, actor_user_id, event_kind, occurred_at FROM issue_events;
DROP TABLE issue_events;
ALTER TABLE issue_events_v32 RENAME TO issue_events;
CREATE UNIQUE INDEX issue_events_one_creation_idx ON issue_events (issue_id) WHERE event_kind = 'issue.created';
CREATE INDEX issue_events_issue_time_idx ON issue_events (project_id, issue_id, occurred_at DESC, id DESC);

CREATE TABLE outbox_events_v32 (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'issue.created', 'issue.title_updated', 'issue.description_updated', 'issue.text_updated'
    )),
    payload_version INTEGER NOT NULL CHECK (payload_version = 1),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    issue_version INTEGER NOT NULL CHECK (typeof(issue_version) = 'integer' AND (
        (event_kind = 'issue.created' AND issue_version = 1)
        OR (event_kind != 'issue.created' AND issue_version >= 2)
    )),
    occurred_at TEXT NOT NULL,
    UNIQUE (issue_id, issue_version, event_kind),
    FOREIGN KEY (project_id, issue_id) REFERENCES issues (project_id, id) ON DELETE RESTRICT
);
INSERT INTO outbox_events_v32 SELECT id, event_kind, payload_version, project_id, issue_id, actor_user_id, issue_version, occurred_at FROM outbox_events;
DROP TABLE outbox_events;
ALTER TABLE outbox_events_v32 RENAME TO outbox_events;
CREATE INDEX outbox_events_time_idx ON outbox_events (occurred_at, id);
CREATE TRIGGER outbox_event_payload_is_immutable BEFORE UPDATE ON outbox_events
BEGIN
    SELECT RAISE(ABORT, 'outbox event envelopes are immutable');
END;
