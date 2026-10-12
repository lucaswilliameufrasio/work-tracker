CREATE TABLE password_reset_tokens (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    email_id TEXT NOT NULL REFERENCES user_emails (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT,
    invalidated_at TEXT,
    CHECK (consumed_at IS NULL OR invalidated_at IS NULL)
);

CREATE INDEX password_reset_tokens_user_state_idx
    ON password_reset_tokens (user_id, consumed_at, invalidated_at);
