CREATE TABLE user_emails (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    address TEXT NOT NULL CHECK (address = trim(address)),
    address_key TEXT NOT NULL UNIQUE CHECK (address_key = lower(trim(address))),
    is_primary INTEGER NOT NULL DEFAULT 0 CHECK (is_primary IN (0, 1)),
    confirmed_at TEXT,
    created_at TEXT NOT NULL,
    CHECK (is_primary = 0 OR confirmed_at IS NOT NULL)
);

CREATE UNIQUE INDEX user_emails_one_confirmed_primary_idx
    ON user_emails (user_id)
    WHERE is_primary = 1 AND confirmed_at IS NOT NULL;

CREATE INDEX user_emails_user_idx ON user_emails (user_id);
