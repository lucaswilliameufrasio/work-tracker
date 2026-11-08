CREATE TABLE issue_text_receipts (
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    operation TEXT NOT NULL CHECK (operation = 'issue.replace_text'),
    key_digest BLOB NOT NULL CHECK (typeof(key_digest) = 'blob' AND length(key_digest) = 32),
    request_fingerprint BLOB NOT NULL CHECK (typeof(request_fingerprint) = 'blob' AND length(request_fingerprint) = 32),
    master_key_id BLOB NOT NULL CHECK (typeof(master_key_id) = 'blob' AND length(master_key_id) = 16),
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    issue_id TEXT NOT NULL CHECK (length(issue_id) = 36),
    issue_number INTEGER NOT NULL CHECK (typeof(issue_number) = 'integer' AND issue_number > 0),
    result_version INTEGER NOT NULL CHECK (typeof(result_version) = 'integer' AND result_version >= 1),
    changed INTEGER NOT NULL CHECK (changed IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (actor_user_id, operation, key_digest)
);
CREATE TRIGGER issue_text_receipt_is_immutable BEFORE UPDATE ON issue_text_receipts
BEGIN
    SELECT RAISE(ABORT, 'text receipts are immutable');
END;
CREATE TRIGGER issue_text_receipt_cannot_be_deleted BEFORE DELETE ON issue_text_receipts
BEGIN
    SELECT RAISE(ABORT, 'text receipt slots cannot be reused');
END;
