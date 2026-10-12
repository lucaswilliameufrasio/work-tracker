CREATE TABLE issues (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    issue_number INTEGER NOT NULL CHECK (
        typeof(issue_number) = 'integer' AND issue_number > 0
    ),
    issue_type_id TEXT NOT NULL,
    status_id TEXT NOT NULL,
    title TEXT NOT NULL CHECK (
        length(trim(title)) > 0 AND length(title) <= 240
    ),
    description TEXT CHECK (
        description IS NULL OR length(CAST(description AS BLOB)) <= 65536
    ),
    assignee_user_id TEXT,
    creator_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    priority TEXT NOT NULL DEFAULT 'none'
        CHECK (priority IN ('none', 'low', 'medium', 'high', 'urgent')),
    due_date TEXT CHECK (
        due_date IS NULL OR (
            length(due_date) = 10
            AND due_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
        )
    ),
    rank INTEGER NOT NULL CHECK (typeof(rank) = 'integer'),
    version INTEGER NOT NULL DEFAULT 1 CHECK (
        typeof(version) = 'integer' AND version >= 1
    ),
    completed_at TEXT,
    trashed_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (project_id, issue_number),
    UNIQUE (project_id, id),
    FOREIGN KEY (project_id, issue_type_id)
        REFERENCES issue_types (project_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (project_id, status_id)
        REFERENCES statuses (project_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (project_id, assignee_user_id)
        REFERENCES project_members (project_id, user_id) ON DELETE RESTRICT
);

CREATE INDEX issues_project_status_rank_idx
    ON issues (project_id, status_id, trashed_at, rank, id);
CREATE INDEX issues_project_assignee_idx
    ON issues (project_id, assignee_user_id, status_id, trashed_at, rank, id);
CREATE INDEX issues_project_created_idx
    ON issues (project_id, created_at DESC, id DESC);

CREATE TRIGGER issue_identity_and_authorship_are_immutable
BEFORE UPDATE OF id, project_id, issue_number, creator_user_id ON issues
WHEN NEW.id != OLD.id
  OR NEW.project_id != OLD.project_id
  OR NEW.issue_number != OLD.issue_number
  OR NEW.creator_user_id != OLD.creator_user_id
BEGIN
    SELECT RAISE(ABORT, 'issue identity and authorship are immutable');
END;

CREATE TRIGGER project_issue_counter_never_decreases
BEFORE UPDATE OF next_issue_number ON projects
WHEN NEW.next_issue_number < OLD.next_issue_number
BEGIN
    SELECT RAISE(ABORT, 'project issue number counter cannot decrease');
END;

CREATE TABLE issue_events (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (event_kind IN ('issue.created')),
    occurred_at TEXT NOT NULL,
    UNIQUE (issue_id, event_kind),
    FOREIGN KEY (project_id, issue_id)
        REFERENCES issues (project_id, id) ON DELETE RESTRICT
);

CREATE INDEX issue_events_issue_time_idx
    ON issue_events (project_id, issue_id, occurred_at DESC, id DESC);
