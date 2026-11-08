CREATE TABLE organizations (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    locale TEXT NOT NULL CHECK (locale IN ('pt-BR', 'en')),
    timezone TEXT NOT NULL CHECK (length(trim(timezone)) > 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE users (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    display_name TEXT NOT NULL CHECK (length(trim(display_name)) > 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'banned', 'disabled')),
    anonymized_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE organization_members (
    organization_id INTEGER NOT NULL DEFAULT 1
        REFERENCES organizations (id) ON DELETE RESTRICT,
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'guest')),
    created_at TEXT NOT NULL,
    PRIMARY KEY (organization_id, user_id)
);

CREATE TABLE installation_admins (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL
);

CREATE TRIGGER installation_admin_requires_organization_membership
BEFORE INSERT ON installation_admins
WHEN NOT EXISTS (
    SELECT 1
    FROM organization_members
    WHERE user_id = NEW.user_id
      AND role != 'guest'
)
BEGIN
    SELECT RAISE(ABORT, 'installation admin must be an organization member');
END;

CREATE TRIGGER installation_admin_keeps_organization_membership
BEFORE UPDATE OF role ON organization_members
WHEN NEW.role = 'guest'
 AND EXISTS (
    SELECT 1
    FROM installation_admins
    WHERE user_id = NEW.user_id
 )
BEGIN
    SELECT RAISE(ABORT, 'installation admin cannot be a guest');
END;
