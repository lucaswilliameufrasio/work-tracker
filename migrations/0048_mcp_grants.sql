CREATE TABLE mcp_grants (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'write')),
    all_projects INTEGER NOT NULL CHECK (all_projects IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mcp_grant_projects (
    user_id TEXT NOT NULL REFERENCES mcp_grants (user_id) ON DELETE CASCADE,
    project_id TEXT NOT NULL,
    PRIMARY KEY (user_id, project_id)
);
