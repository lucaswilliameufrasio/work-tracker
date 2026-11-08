CREATE TABLE recovery_codes (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    code_hash BLOB NOT NULL UNIQUE CHECK (length(code_hash) = 32),
    created_at TEXT NOT NULL,
    consumed_at TEXT,
    invalidated_at TEXT,
    CHECK (consumed_at IS NULL OR invalidated_at IS NULL)
);

CREATE INDEX recovery_codes_user_state_idx
    ON recovery_codes (user_id, consumed_at, invalidated_at);
