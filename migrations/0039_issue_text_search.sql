CREATE TABLE issue_search_documents (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL UNIQUE REFERENCES issues (id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    description TEXT
);

CREATE VIRTUAL TABLE issues_fts USING fts5(
    title,
    description,
    content = 'issue_search_documents',
    content_rowid = 'id',
    tokenize = 'unicode61'
);

CREATE TRIGGER issue_search_document_insert_fts
AFTER INSERT ON issue_search_documents
BEGIN
    INSERT INTO issues_fts (rowid, title, description)
    VALUES (new.id, new.title, new.description);
END;

CREATE TRIGGER issue_search_document_delete_fts
AFTER DELETE ON issue_search_documents
BEGIN
    INSERT INTO issues_fts (issues_fts, rowid, title, description)
    VALUES ('delete', old.id, old.title, old.description);
END;

CREATE TRIGGER issue_search_document_update_fts
AFTER UPDATE OF title, description ON issue_search_documents
BEGIN
    INSERT INTO issues_fts (issues_fts, rowid, title, description)
    VALUES ('delete', old.id, old.title, old.description);
    INSERT INTO issues_fts (rowid, title, description)
    VALUES (new.id, new.title, new.description);
END;

CREATE TRIGGER issue_search_issue_insert
AFTER INSERT ON issues
BEGIN
    INSERT INTO issue_search_documents (issue_id, title, description)
    VALUES (new.id, new.title, new.description);
END;

CREATE TRIGGER issue_search_issue_text_update
AFTER UPDATE OF title, description ON issues
BEGIN
    INSERT INTO issue_search_documents (issue_id, title, description)
    VALUES (new.id, new.title, new.description)
    ON CONFLICT (issue_id) DO UPDATE SET
        title = excluded.title,
        description = excluded.description;
END;

CREATE TRIGGER issue_search_issue_delete
BEFORE DELETE ON issues
BEGIN
    DELETE FROM issue_search_documents WHERE issue_id = old.id;
END;

INSERT INTO issue_search_documents (issue_id, title, description)
SELECT id, title, description FROM issues;
