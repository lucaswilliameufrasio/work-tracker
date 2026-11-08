CREATE INDEX sessions_user_id_keyset
    ON sessions (user_id, id DESC)
    WHERE revoked_at IS NULL;
