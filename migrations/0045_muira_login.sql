CREATE TABLE auth_identities_muira (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK (provider IN ('google', 'apple', 'github', 'gitlab', 'muira')),
    issuer TEXT NOT NULL CHECK (length(trim(issuer)) > 0),
    subject TEXT NOT NULL CHECK (length(trim(subject)) > 0),
    created_at TEXT NOT NULL,
    UNIQUE (provider, issuer, subject)
);

INSERT INTO auth_identities_muira (
    id, user_id, provider, issuer, subject, created_at
)
SELECT id, user_id, provider, issuer, subject, created_at
FROM auth_identities;

DROP TABLE auth_identities;

ALTER TABLE auth_identities_muira RENAME TO auth_identities;

CREATE INDEX auth_identities_user_idx ON auth_identities (user_id);

CREATE TABLE installation_login (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    source TEXT NOT NULL CHECK (source IN ('puxiri', 'muira')),
    muira_issuer TEXT,
    muira_client_id TEXT,
    muira_client_secret_envelope BLOB,
    CHECK (
        (
            source = 'puxiri'
            AND muira_issuer IS NULL
            AND muira_client_id IS NULL
            AND muira_client_secret_envelope IS NULL
        )
        OR (
            source = 'muira'
            AND length(trim(muira_issuer)) > 0
            AND length(trim(muira_client_id)) > 0
            AND muira_client_secret_envelope IS NOT NULL
        )
    )
);

CREATE TABLE muira_login_attempts (
    state_hash BLOB PRIMARY KEY NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('setup', 'login')),
    nonce TEXT NOT NULL,
    verifier TEXT NOT NULL,
    organization_name TEXT,
    locale TEXT,
    timezone_name TEXT,
    display_name TEXT,
    owner_email TEXT,
    owner_email_key TEXT,
    setup_token_hash BLOB,
    client_secret_envelope BLOB,
    issuer TEXT,
    client_id TEXT,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    CHECK (
        (
            purpose = 'login'
            AND organization_name IS NULL
            AND locale IS NULL
            AND timezone_name IS NULL
            AND display_name IS NULL
            AND owner_email IS NULL
            AND owner_email_key IS NULL
            AND setup_token_hash IS NULL
            AND client_secret_envelope IS NULL
            AND issuer IS NULL
            AND client_id IS NULL
        )
        OR (
            purpose = 'setup'
            AND length(trim(organization_name)) > 0
            AND locale IN ('pt-BR', 'en')
            AND length(trim(timezone_name)) > 0
            AND length(trim(display_name)) > 0
            AND length(trim(owner_email)) > 0
            AND length(trim(owner_email_key)) > 0
            AND setup_token_hash IS NOT NULL
            AND client_secret_envelope IS NOT NULL
            AND length(trim(issuer)) > 0
            AND length(trim(client_id)) > 0
        )
    )
);
