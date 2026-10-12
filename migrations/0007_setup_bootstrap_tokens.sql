CREATE TABLE setup_bootstrap_tokens (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    issued_at TEXT NOT NULL,
    consumed_at TEXT
);
