CREATE TABLE mail_sender (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    provider TEXT NOT NULL CHECK (provider IN ('sabia', 'resend', 'millionsend')),
    api_key_envelope BLOB NOT NULL CHECK (length(api_key_envelope) > 0),
    from_address TEXT,
    from_name TEXT,
    template_key TEXT,
    sabia_origin TEXT,
    updated_at TEXT NOT NULL,
    CHECK (
        (
            provider = 'sabia'
            AND template_key IS NOT NULL
            AND sabia_origin IS NOT NULL
            AND from_address IS NULL
            AND from_name IS NULL
        )
        OR (
            provider IN ('resend', 'millionsend')
            AND from_address IS NOT NULL
            AND template_key IS NULL
            AND sabia_origin IS NULL
        )
    )
);
