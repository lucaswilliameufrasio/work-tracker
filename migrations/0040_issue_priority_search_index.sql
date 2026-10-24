CREATE INDEX issues_project_priority_search_idx
    ON issues (project_id, priority, trashed_at, issue_number, id);
