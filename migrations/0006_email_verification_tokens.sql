CREATE TABLE email_verification_tokens (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    email_id TEXT NOT NULL REFERENCES user_emails (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT
);

CREATE UNIQUE INDEX email_verification_one_pending_per_email_idx
    ON email_verification_tokens (email_id)
    WHERE consumed_at IS NULL;
