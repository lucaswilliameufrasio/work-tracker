CREATE INDEX issues_project_due_date_search_idx
    ON issues (project_id, due_date, trashed_at, issue_number, id)
    WHERE due_date IS NOT NULL;
