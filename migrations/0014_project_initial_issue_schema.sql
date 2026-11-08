CREATE TABLE issue_types (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    system_key TEXT CHECK (
        system_key IS NULL OR (
            length(system_key) BETWEEN 1 AND 48
            AND system_key NOT GLOB '*[^a-z0-9_-]*'
        )
    ),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    icon_key TEXT CHECK (
        icon_key IS NULL OR (
            length(icon_key) BETWEEN 1 AND 48
            AND icon_key NOT GLOB '*[^a-z0-9_-]*'
        )
    ),
    color_key TEXT CHECK (
        color_key IS NULL OR (
            length(color_key) BETWEEN 1 AND 48
            AND color_key NOT GLOB '*[^a-z0-9_-]*'
        )
    ),
    archived_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (project_id, id),
    UNIQUE (project_id, system_key)
);

CREATE INDEX issue_types_project_idx ON issue_types (project_id, archived_at, name, id);

CREATE TRIGGER issue_type_system_key_is_immutable
BEFORE UPDATE OF system_key ON issue_types
WHEN NEW.system_key IS NOT OLD.system_key
BEGIN
    SELECT RAISE(ABORT, 'issue type system key is immutable');
END;

CREATE TABLE statuses (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    system_key TEXT CHECK (
        system_key IS NULL OR (
            length(system_key) BETWEEN 1 AND 48
            AND system_key NOT GLOB '*[^a-z0-9_-]*'
        )
    ),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    category TEXT NOT NULL CHECK (category IN ('backlog', 'active', 'done', 'canceled')),
    position INTEGER NOT NULL CHECK (position >= 0),
    board_visible INTEGER NOT NULL CHECK (board_visible IN (0, 1)),
    archived_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (project_id, id),
    UNIQUE (project_id, system_key)
);

CREATE INDEX statuses_project_order_idx ON statuses (project_id, archived_at, position, id);

CREATE TRIGGER status_system_key_is_immutable
BEFORE UPDATE OF system_key ON statuses
WHEN NEW.system_key IS NOT OLD.system_key
BEGIN
    SELECT RAISE(ABORT, 'status system key is immutable');
END;

-- Backfill projects created before this migration. Migration-generated IDs are
-- opaque UUID-shaped values; new application rows use UUIDv7.
INSERT INTO issue_types (
    id, project_id, system_key, name, icon_key, color_key, archived_at,
    created_at, updated_at
)
SELECT
    lower(hex(randomblob(4))) || '-' ||
        lower(hex(randomblob(2))) || '-4' ||
        substr(lower(hex(randomblob(2))), 2, 3) || '-' ||
        substr('89ab', (random() & 3) + 1, 1) ||
        substr(lower(hex(randomblob(2))), 2, 3) || '-' ||
        lower(hex(randomblob(6))),
    projects.id,
    'task',
    CASE organizations.locale WHEN 'pt-BR' THEN 'Tarefa' ELSE 'Task' END,
    NULL,
    NULL,
    NULL,
    projects.created_at,
    projects.updated_at
FROM projects
JOIN organizations ON organizations.id = projects.organization_id
WHERE NOT EXISTS (
    SELECT 1 FROM issue_types
    WHERE issue_types.project_id = projects.id
      AND issue_types.system_key = 'task'
);

WITH default_statuses(system_key, name_pt_br, name_en, category, position, board_visible) AS (
    VALUES
        ('backlog', 'Backlog', 'Backlog', 'backlog', 0, 0),
        ('todo', 'A fazer', 'To do', 'backlog', 1, 1),
        ('in_progress', 'Em progresso', 'In progress', 'active', 2, 1),
        ('done', 'Concluído', 'Done', 'done', 3, 1),
        ('canceled', 'Cancelado', 'Canceled', 'canceled', 4, 0)
)
INSERT INTO statuses (
    id, project_id, system_key, name, category, position, board_visible,
    archived_at, created_at, updated_at
)
SELECT
    lower(hex(randomblob(4))) || '-' ||
        lower(hex(randomblob(2))) || '-4' ||
        substr(lower(hex(randomblob(2))), 2, 3) || '-' ||
        substr('89ab', (random() & 3) + 1, 1) ||
        substr(lower(hex(randomblob(2))), 2, 3) || '-' ||
        lower(hex(randomblob(6))),
    projects.id,
    default_statuses.system_key,
    CASE organizations.locale
        WHEN 'pt-BR' THEN default_statuses.name_pt_br
        ELSE default_statuses.name_en
    END,
    default_statuses.category,
    default_statuses.position,
    default_statuses.board_visible,
    NULL,
    projects.created_at,
    projects.updated_at
FROM projects
JOIN organizations ON organizations.id = projects.organization_id
CROSS JOIN default_statuses
WHERE NOT EXISTS (
    SELECT 1 FROM statuses
    WHERE statuses.project_id = projects.id
      AND statuses.system_key = default_statuses.system_key
);
