CREATE TABLE auth_identities (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK (provider IN ('google', 'apple', 'github', 'gitlab')),
    issuer TEXT NOT NULL CHECK (length(trim(issuer)) > 0),
    subject TEXT NOT NULL CHECK (length(trim(subject)) > 0),
    created_at TEXT NOT NULL,
    UNIQUE (provider, issuer, subject)
);

CREATE INDEX auth_identities_user_idx ON auth_identities (user_id);
