CREATE INDEX user_emails_confirmed_user_idx
    ON user_emails (user_id, confirmed_at, is_primary);
