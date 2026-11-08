CREATE INDEX issues_project_status_search_idx
    ON issues (project_id, status_id, trashed_at, issue_number, id);
