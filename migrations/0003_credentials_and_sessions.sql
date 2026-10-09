CREATE TABLE password_credentials (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    password_hash TEXT NOT NULL CHECK (substr(password_hash, 1, 10) = '$argon2id$'),
    changed_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    reauthenticated_at TEXT,
    revoked_at TEXT
);

CREATE INDEX sessions_user_expiry_idx
    ON sessions (user_id, expires_at)
    WHERE revoked_at IS NULL;
