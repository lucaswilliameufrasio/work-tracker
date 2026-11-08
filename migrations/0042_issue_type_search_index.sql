CREATE INDEX issues_project_type_search_idx
    ON issues (project_id, issue_type_id, trashed_at, issue_number, id);
