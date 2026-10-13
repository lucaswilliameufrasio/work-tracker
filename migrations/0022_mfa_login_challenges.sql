CREATE TABLE mfa_login_challenges (
    token_hash BLOB PRIMARY KEY NOT NULL CHECK (
        typeof(token_hash) = 'blob' AND length(token_hash) = 32
    ),
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    email_key TEXT NOT NULL,
    credential_fingerprint BLOB NOT NULL CHECK (
        typeof(credential_fingerprint) = 'blob' AND length(credential_fingerprint) = 32
    ),
    totp_credential_id TEXT NOT NULL
        REFERENCES mfa_credentials (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    CHECK (expires_at > created_at)
);

CREATE INDEX mfa_login_challenges_user_expiry_idx
    ON mfa_login_challenges (user_id, expires_at);
