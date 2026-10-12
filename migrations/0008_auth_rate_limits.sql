CREATE TABLE auth_rate_limit_buckets (
    subject_hash BLOB PRIMARY KEY NOT NULL CHECK (length(subject_hash) = 32),
    failure_count INTEGER NOT NULL CHECK (failure_count > 0),
    window_started_at TEXT NOT NULL,
    blocked_until TEXT
);

CREATE INDEX auth_rate_limit_window_idx
    ON auth_rate_limit_buckets (window_started_at);

CREATE TABLE auth_rate_limit_key_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    master_key_id BLOB NOT NULL CHECK (length(master_key_id) = 16)
);
