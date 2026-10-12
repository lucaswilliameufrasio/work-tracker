CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    organization_id INTEGER NOT NULL DEFAULT 1
        REFERENCES organizations (id) ON DELETE RESTRICT,
    project_key TEXT NOT NULL CHECK (
        length(project_key) BETWEEN 2 AND 6
        AND project_key NOT GLOB '*[^A-Z]*'
    ),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    description TEXT,
    default_view TEXT NOT NULL DEFAULT 'board'
        CHECK (default_view IN ('board', 'list')),
    archived_at TEXT,
    next_issue_number INTEGER NOT NULL DEFAULT 1 CHECK (next_issue_number >= 1),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (organization_id, project_key),
    UNIQUE (id, organization_id)
);

CREATE TRIGGER project_identity_is_immutable
BEFORE UPDATE OF id, organization_id, project_key ON projects
WHEN NEW.id != OLD.id
  OR NEW.organization_id != OLD.organization_id
  OR NEW.project_key != OLD.project_key
BEGIN
    SELECT RAISE(ABORT, 'project identity is immutable');
END;

CREATE TRIGGER project_cannot_be_permanently_deleted
BEFORE DELETE ON projects
BEGIN
    SELECT RAISE(ABORT, 'projects must be archived instead of deleted');
END;

CREATE TABLE project_members (
    project_id TEXT NOT NULL,
    organization_id INTEGER NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('manager', 'member', 'viewer')),
    created_at TEXT NOT NULL,
    PRIMARY KEY (project_id, user_id),
    FOREIGN KEY (project_id, organization_id)
        REFERENCES projects (id, organization_id) ON DELETE RESTRICT,
    FOREIGN KEY (organization_id, user_id)
        REFERENCES organization_members (organization_id, user_id) ON DELETE RESTRICT
);

CREATE INDEX project_members_user_idx ON project_members (user_id, project_id);

CREATE TRIGGER project_manager_must_not_be_guest_on_insert
BEFORE INSERT ON project_members
WHEN NEW.role = 'manager'
 AND NOT EXISTS (
    SELECT 1
    FROM organization_members
    WHERE organization_id = NEW.organization_id
      AND user_id = NEW.user_id
      AND role != 'guest'
 )
BEGIN
    SELECT RAISE(ABORT, 'project manager must be a non-guest organization member');
END;

CREATE TRIGGER project_manager_must_not_be_guest_on_update
BEFORE UPDATE OF organization_id, user_id, role ON project_members
WHEN NEW.role = 'manager'
 AND NOT EXISTS (
    SELECT 1
    FROM organization_members
    WHERE organization_id = NEW.organization_id
      AND user_id = NEW.user_id
      AND role != 'guest'
 )
BEGIN
    SELECT RAISE(ABORT, 'project manager must be a non-guest organization member');
END;

CREATE TRIGGER organization_member_cannot_become_guest_while_manager
BEFORE UPDATE OF role ON organization_members
WHEN NEW.role = 'guest'
 AND EXISTS (
    SELECT 1
    FROM project_members
    WHERE organization_id = NEW.organization_id
      AND user_id = NEW.user_id
      AND role = 'manager'
 )
BEGIN
    SELECT RAISE(ABORT, 'project manager cannot become an organization guest');
END;

DROP TRIGGER audit_events_no_update;
DROP INDEX audit_events_occurred_idx;
DROP INDEX audit_events_actor_idx;
ALTER TABLE audit_events RENAME TO audit_events_v12;

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
        'project.created'
    )),
    result TEXT NOT NULL CHECK (result IN ('success', 'failure')),
    actor_user_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('user', 'email_address', 'session', 'project')
    ),
    target_id TEXT NOT NULL CHECK (length(target_id) = 36),
    occurred_at TEXT NOT NULL
);

INSERT INTO audit_events (
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
)
SELECT
    id, event_kind, result, actor_user_id, target_kind, target_id, occurred_at
FROM audit_events_v12;

DROP TABLE audit_events_v12;

CREATE INDEX audit_events_occurred_idx ON audit_events (occurred_at DESC, id DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, occurred_at DESC, id DESC);

CREATE TRIGGER audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;
