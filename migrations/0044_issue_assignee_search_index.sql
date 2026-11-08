CREATE INDEX issues_project_assignee_search_idx
    ON issues (project_id, assignee_user_id, trashed_at, issue_number, id)
    WHERE assignee_user_id IS NOT NULL;
