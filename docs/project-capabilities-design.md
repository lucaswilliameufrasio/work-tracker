# Session-bound project capability projection

Slice239 removes the read-only proof used as a creation form precheck. GET/HEAD `/api/v1/projects/{project_id}/capabilities` returns only `{project_id, can_create_issues}` for the bearer-derived user with current explicit ReadProjectContent access. Readable viewer, archived or managerless project returnsfalse; organization owner/admin/installation authority without membership and missing projects share generic404. Session/account/membership/archive/manager context and capability are read in one SQLite transaction, without activity touch in the domain reader. No project catalog traversal, free text, role lists or default-schema/assignee claims.

The flag is advisory UI state, not a bearer token or authorization grant. It can become stale immediately; SessionIssueWriter must independently revalidate in BEGINIMMEDIATE and remain unchanged. Mutation defaults/storage/capacity may fail even when the flag istrue. No caching/storage, query parameters or mutation methods. Existing browser opt-in/Host/Origin/Fetch/cookie/envelope/no-store/HEAD policies apply. OpenAPI and every-operation tests updated together.

The creation controller checks current-session identity then the strict bounded capability DTO instead of HEAD issue collection; false becomes a distinct read-only state without form/retry. Returning from suspension rechecks it before restoring an uncertain command/reference; retained commands are never automatically sent or assigned a new key. Missing/inaccessible remains generic unavailable. Links from project cards already use role/archive hints; direct URLs now refuse the form for known read-only access. Other work screens do not infer authority from this endpoint. Tests cover current roles/archive/session, outsider/missing equivalence, zero side effects, contract bounds, revoked capability still rejected by POST, and actual HTTPS mobile/desktop read-only form denial. More granular actions, roster and mutation fields remain separate.
# Text replacement capability (slice244)

The DTO also requires `can_replace_issue_text`, projected from the same current
MutateIssues policy/snapshot as creation. It does not authorize PUT. The editor
checks this explicit flag; older servers omitting it normalize to false in the
client, never to can_create_issues. Read-only and archived projects return false.
