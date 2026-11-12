//! Partial development contract for implemented opt-in browser routes only.
use serde_json::{Value, json};

/// Exportable metadata, not a new HTTP endpoint or an exposure approval.
pub fn browser_document() -> Value {
    let uuid = json!({"type": "string", "format": "uuid"});
    let timestamp = json!({"type": "string", "format": "date-time"});
    let page_size = json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}});
    let project_query = vec![
        page_size.clone(),
        json!({"name": "cursor", "in": "query", "schema": {"type": "string"}, "description": "Exclusive project-key boundary in ascending order; must be a valid project key."}),
        json!({"name": "include_archived", "in": "query", "schema": {"type": "boolean", "default": false}}),
    ];
    let session_query = vec![
        page_size.clone(),
        json!({"name": "cursor", "in": "query", "schema": uuid.clone(), "description": "Exclusive canonical lowercase hyphenated UUID boundary, descending session-record ID order. Need not identify an existing record and never grants access."}),
    ];
    let issue_query = vec![
        page_size,
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone(), "description": "Canonical UUID; explicit project membership is required, not organization/installation authority alone."}),
        json!({"name": "cursor", "in": "query", "schema": decimal_i64(), "description": "Exclusive positive canonical decimal issue-number boundary, ascending order. No signs/zero/leading zeroes; maximum 9223372036854775807."}),
    ];
    let mut paths = serde_json::Map::new();
    for (path, id, schema, query, description) in [
        (
            "/api/v1/auth/session",
            "currentSession",
            "CurrentSession",
            vec![],
            "Current identity and bearer-bound CSRF token, never the bearer. No query options.",
        ),
        (
            "/api/v1/auth/sessions",
            "listSessions",
            "SessionPage",
            session_query,
            "Own valid sessions only. At most page_size candidates plus one lookahead are read. Invalid candidates are omitted; short/empty pages can have has_more=true. Follow next_cursor until false. Order is record ID, not activity/original authentication time. A traversal across requests is not a frozen snapshot.",
        ),
        (
            "/api/v1/projects",
            "listProjects",
            "ProjectPage",
            project_query.clone(),
            "Explicit project memberships only, including eligible guests. Organization/admin authority does not grant project content access. Ascending immutable project-key order.",
        ),
        (
            "/api/v1/projects/{project_id}/issues",
            "listProjectIssues",
            "IssuePage",
            issue_query,
            "Session identity, explicit current project read access and summaries share one SQLite snapshot. Missing/inaccessible project gives generic404. Archived projects are readable; trash excluded. No descriptions/history/rank. Issue numbers and versions are decimal JSON strings preserving positive i64 precision. This collection route has no search/filter options; use the dedicated project search operation. Cross-request pagination is not a frozen snapshot.",
        ),
        (
            "/api/v1/admin/projects",
            "listOperationalProjects",
            "OperationalProjectPage",
            project_query,
            "Active organization owner/admin operational metadata only. Counts include retained memberships/issues. Does not grant issue/content access. Ascending project-key order.",
        ),
    ] {
        let get = operation(id, Some(schema), query.clone(), description);
        let head = operation(&format!("{id}Head"), None, query, description);
        paths.insert(path.into(), json!({"get": get, "head": head}));
    }
    let assigned_query = vec![
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}}),
        json!({"name": "cursor", "in": "query", "schema": {"type": "string"}, "description": "Exclusive keyset. Order is due date ascending, dates that are absent last, then updated_at descending, then issue id descending."}),
    ];
    let assigned = "Issues whose current assignee is the signed-in person and whose project that person can still read. Archived projects and trashed issues are omitted. Creating or commenting on an issue does not put it here.";
    paths.insert(
        "/api/v1/me/issues".into(),
        json!({
            "get": operation("listAssignedIssues", Some("IssuePage"), assigned_query.clone(), assigned),
            "head": operation("listAssignedIssuesHead", None, assigned_query, assigned),
        }),
    );
    for (path, id, description) in [
        (
            "/api/v1/auth/session",
            "logout",
            "Revoke only the exact bearer session. Clear cookie after commit; other sessions remain valid. No query options.",
        ),
        (
            "/api/v1/auth/sessions",
            "revokeAllSessions",
            "Revoke all sessions of the bearer-derived user, including current, and clear cookie after commit. Other users remain unaffected. No query options.",
        ),
        (
            "/api/v1/auth/sessions/{session_id}",
            "revokeSession",
            "Revoke only an owned target. Foreign, missing and already-revoked targets share generic 404. Clear cookie only when the target is current and commit succeeds. An expired but not revoked owned target can be revoked. No query options.",
        ),
    ] {
        let mut parameters = vec![
            json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}, "description": "Exact configured HTTPS origin, without path or trailing slash."}),
            json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}, "description": "CSRF token from currentSession, bound to the exact cookie bearer; not authentication or MFA assurance."}),
        ];
        if path.contains("{session_id}") {
            parameters.push(json!({"name": "session_id", "in": "path", "required": true, "schema": uuid.clone(), "description": "Canonical lowercase hyphenated UUID; ownership is revalidated transactionally."}));
        }
        let mut delete = operation(id, None, parameters, description);
        delete["responses"].as_object_mut().unwrap().remove("200");
        delete["responses"]["204"] = json!({
            "description": "Committed revocation; empty body. Cookie clearing is conditional as described by this operation.",
            "headers": {
                "Cache-Control": {"schema": {"type": "string", "const": "no-store"}},
                "Set-Cookie": {"schema": {"type": "string"}, "description": "Only when current was revoked, after commit: __Host-puxiri_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax"}
            }
        });
        paths.entry(path).or_insert_with(|| json!({}))["delete"] = delete;
    }
    let project_member_query = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}}),
        json!({"name": "cursor", "in": "query", "schema": uuid.clone(), "description": "Exclusive canonical UUID user_id boundary in ascending order; does not grant access. Traversal is not a frozen snapshot."}),
    ];
    let description = "Current session plus explicit project membership, or organization owner/admin operational authority, is checked in the same SQLite snapshot as the roster. Installation-admin status alone grants nothing. Archived project rosters remain readable; missing/inaccessible projects return generic404. UUID keyset order; page is bounded and not a cross-request snapshot. Confirmed primary email is omitted except for project managers and organization owners/admins. can_be_assigned is advisory only; assignment writes revalidate current eligibility.";
    paths.insert(
        "/api/v1/projects/{project_id}/members".into(),
        json!({
            "get": operation("listProjectMembers", Some("ProjectMemberPage"), project_member_query.clone(), description),
            "head": operation("listProjectMembersHead", None, project_member_query, description)
        }),
    );
    let member_parameters = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "user_id", "in": "path", "required": true, "schema": uuid.clone()}),
    ];
    let description = "One current explicit project member, including a member no longer eligible for new assignment so existing assignments can be displayed. Session and roster authorization share a SQLite snapshot. Missing/inaccessible project or nonmember returns generic404. Confirmed primary email is omitted except for project managers and organization owners/admins. No query parameters.";
    paths.insert(
        "/api/v1/projects/{project_id}/members/{user_id}".into(),
        json!({
            "get": operation("getProjectMember", Some("ProjectMember"), member_parameters.clone(), description),
            "head": operation("getProjectMemberHead", None, member_parameters, description)
        }),
    );
    let selectors = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    ];
    let mut history_query = selectors.clone();
    history_query.push(json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}}));
    history_query.push(json!({"name": "cursor", "in": "query", "schema": {"type": "string", "minLength": 90, "maxLength": 90}, "description": "Opaque canonical Base64URL timestamp/UUID keyset boundary; no padding. Does not grant access or require a surviving boundary row."}));
    for (path, id, schema, query, description) in [
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}",
            "getProjectIssue",
            "IssueDetail",
            selectors,
            "Authorized bounded issue record, including retained trash/archived projects. No embedded unbounded history; use history subresource. No query options. Markdown description is JSON data, not rendered HTML.",
        ),
        (
            "/api/v1/projects/{project_id}/issues/{issue_number}/history",
            "listIssueHistory",
            "IssueHistoryPage",
            history_query,
            "Content-free actor/event/time journal. Explicit project/session authorization in one snapshot. Ascending occurred_at/id keyset, at most page_size plus one lookahead. Cross-request traversal is not frozen.",
        ),
    ] {
        paths.insert(path.into(), json!({"get": operation(id, Some(schema), query.clone(), description), "head": operation(&format!("{id}Head"), None, query, description)}));
    }
    let issue_search_query = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "q", "in": "query", "required": false, "schema": {"type": "string", "minLength": 1, "maxLength": 256, "x-maxUtf8Bytes": 512}, "description": "Optional literal Unicode-whitespace-separated terms combined with AND; FTS operators are not syntax. A project-local issue key is matched exactly, case-insensitively. Blank/control-containing queries are rejected."}),
        json!({"name": "priority", "in": "query", "required": false, "schema": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]}, "description": "Exact priority filter. Combines with q, due_date, issue_type_id, status_id and assignee_user_id using AND; may also be used alone."}),
        json!({"name": "due_date", "in": "query", "required": false, "schema": {"type": "string", "format": "date", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"}, "description": "Exact civil due date (valid YYYY-MM-DD calendar date). Combines with q, priority, issue_type_id, status_id and assignee_user_id using AND; may also be used alone. Does not mean a range, overdue, or missing due date."}),
        json!({"name": "issue_type_id", "in": "query", "required": false, "schema": uuid.clone(), "description": "Exact issue type in this project, including an archived type. Combines with q, priority, due_date, status_id and assignee_user_id using AND; may also be used alone. A missing, foreign or malformed type is rejected and does not widen results. It does not mean every type or an untyped issue."}),
        json!({"name": "status_id", "in": "query", "required": false, "schema": uuid.clone(), "description": "Exact status in this project, including an archived status. Combines with q, priority, due_date, issue_type_id and assignee_user_id using AND; may also be used alone. A missing, foreign or malformed status is rejected and does not widen results. It does not mean every status."}),
        json!({"name": "assignee_user_id", "in": "query", "required": false, "schema": uuid.clone(), "description": "Exact project member, including a member who is no longer eligible for new assignments. Combines with q, priority, due_date, issue_type_id and status_id using AND; may also be used alone. A missing, foreign or malformed user is rejected and does not widen results. It does not mean every assignee or an unassigned issue."}),
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}}),
        json!({"name": "cursor", "in": "query", "schema": {"type": "string", "minLength": 51, "maxLength": 75, "pattern": "^[A-Za-z0-9_-]+$"}, "description": "Opaque canonical Base64URL issue_number/issue_id keyset boundary; no padding. Does not grant access or require a surviving row."}),
    ];
    let issue_search_description = "Search title/description with FTS5, match a project-local issue key exactly, or filter by exact priority, exact civil due_date, exact issue type, exact status and/or exact assignee. q, priority, due_date, issue_type_id, status_id and assignee_user_id combine with AND; at least one is required. An archived type or status in the same project remains filterable. A current project member remains filterable after losing assignment eligibility, because the retained assignment still references that membership. A missing, foreign or malformed value is rejected instead of returning a widened or empty success page. Session identity and explicit project membership are revalidated in the same SQLite snapshot as results. Missing/inaccessible project is generic404. Archived projects remain readable; trashed issues and comments are excluded. Summaries contain no description/snippet. Results are ordered by immutable issue_number/id and bounded with one lookahead. A cursor is not authorization and traversal is not frozen across requests. Date ranges, overdue/missing-date filters, unassigned-assignee filters, other structured filters, facets, relevance ranking or saved views are not provided.";
    paths.insert(
        "/api/v1/projects/{project_id}/issues/search".into(),
        json!({
            "get": operation("searchProjectIssues", Some("IssuePage"), issue_search_query.clone(), issue_search_description),
            "head": operation("searchProjectIssuesHead", None, issue_search_query, issue_search_description)
        }),
    );
    let catalog_query = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 50}}),
        json!({"name": "cursor", "in": "query", "schema": uuid.clone(), "description": "Exclusive canonical UUID ascending boundary, not board display order; does not grant access."}),
        json!({"name": "include_archived", "in": "query", "schema": {"type": "boolean", "default": false}}),
    ];
    for (path, id, schema) in [
        (
            "/api/v1/projects/{project_id}/statuses",
            "listProjectStatuses",
            "StatusPage",
        ),
        (
            "/api/v1/projects/{project_id}/issue-types",
            "listProjectIssueTypes",
            "IssueTypePage",
        ),
    ] {
        let description = "Explicit project membership and session identity revalidated with catalog data in one snapshot. Missing/inaccessible project returns generic404. Archived projects readable; archived catalog records excluded by default. UUID keyset traversal is not display order or a frozen cross-request snapshot. No editor or board query is enabled.";
        paths.insert(path.into(), json!({"get": operation(id, Some(schema), catalog_query.clone(), description), "head": operation(&format!("{id}Head"), None, catalog_query.clone(), description)}));
    }
    for (path, id, schema) in [
        (
            "/api/v1/projects/{project_id}/statuses/{record_id}",
            "getProjectStatus",
            "Status",
        ),
        (
            "/api/v1/projects/{project_id}/issue-types/{record_id}",
            "getProjectIssueType",
            "IssueType",
        ),
    ] {
        let query = vec![
            json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
            json!({"name": "record_id", "in": "path", "required": true, "schema": uuid.clone()}),
        ];
        let description = "One catalog record, including archived references, in the same authenticated explicit-membership snapshot. Missing/foreign project or record returns generic404. No query options, edits or catalog traversal. Name size is not capped by the existing storage schema.";
        paths.insert(path.into(), json!({"get": operation(id, Some(schema), query.clone(), description), "head": operation(&format!("{id}Head"), None, query, description)}));
    }
    let board_query = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "status_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}}),
        json!({"name": "cursor", "in": "query", "schema": {"type": "string", "minLength": 51, "maxLength": 76}, "description": "Exclusive canonical Base64URL rank/UUID boundary. Rank is canonical signed i64 decimal; no padding. Boundary need not survive and grants no access."}),
    ];
    let description = "One visible nonarchived status column. Session, explicit project membership, status eligibility, nontrash count and card page share one SQLite snapshot. Missing/foreign/hidden/archived status or inaccessible project returns generic404. Archived project remains readable. Ascending rank/id keyset with one lookahead; count/rank use lossless decimal strings. Read-only rank is not a mutation command. No cross-request frozen snapshot, movement, filter or SSE is implemented.";
    paths.insert("/api/v1/projects/{project_id}/board/{status_id}".into(), json!({"get": operation("readBoardColumn", Some("BoardColumnPage"), board_query.clone(), description), "head": operation("readBoardColumnHead", None, board_query, description)}));
    let status_query = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "page_size", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 100, "default": 4}}),
        json!({"name": "cursor", "in": "query", "schema": {"type": "string", "minLength": 51, "maxLength": 75}, "description": "Canonical Base64URL nonnegative i64 position/UUID boundary without padding. Does not grant access or freeze status order across requests."}),
    ];
    let description = "Visible nonarchived board statuses ordered globally by position/id, not UUID catalog order. Session, membership and metadata share a snapshot. Missing/inaccessible project returns generic404; archived projects remain readable. No hidden candidate rows, unbounded traversal or reordering command.";
    paths.insert("/api/v1/projects/{project_id}/board".into(), json!({"get": operation("listBoardStatuses", Some("BoardStatusPage"), status_query.clone(), description), "head": operation("listBoardStatusesHead", None, status_query, description)}));
    let parameters =
        vec![json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()})];
    let description = "Advisory current can_create_issues projection for explicit project readers. Session, account, membership, archive and manager context share one SQLite snapshot. Viewer/read-only project returns false; missing/private project generic404. Not an authorization token: every POST independently revalidates current mutation authority; defaults/capacity can still reject. No query parameters or free text.";
    paths.insert("/api/v1/projects/{project_id}/capabilities".into(), json!({"get": operation("getProjectCapabilities", Some("ProjectCapabilities"), parameters.clone(), description), "head": operation("getProjectCapabilitiesHead", None, parameters, description)}));
    let mut create = operation(
        "createProjectIssue",
        Some("IssueCreationReceipt"),
        vec![
            json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
            json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
            json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
            json!({"name": "Idempotency-Key", "in": "header", "required": true, "schema": uuid.clone()}),
        ],
        "Create with current explicit mutation membership in the same transaction as issue/history/outbox/receipt. Exact replay returns201 with stable references, never historical text. Preserve request/key after an uncertain response; no automatic retry or new-key recreation. No query options, explicit null, unknown or duplicate fields. No publication/worker or login enabled.",
    );
    create["requestBody"] = json!({"required": true, "description": "Uncompressed JSON, at most524288 bytes, collected within one second before handler/transaction. Optional fields must be omitted, not null. Description limit65536 UTF-8 bytes; title is trimmed and must contain1–240 characters.", "content": {"application/json": {"schema": object(json!({"title": {"type": "string"}, "description": {"type": "string"}, "assignee_user_id": uuid.clone()}), &["title"])}}});
    create["responses"].as_object_mut().unwrap().remove("200");
    create["responses"]["201"] = json!({"description": "Committed creation or exact replay; no Set-Cookie or text snapshot.", "headers": {
        "Cache-Control": {"schema": {"type": "string", "const": "no-store"}},
        "Location": {"schema": {"type": "string"}, "description": "Relative issue detail API URL."},
        "Idempotency-Replayed": {"schema": {"type": "string", "enum": ["true", "false"]}}
    }, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/IssueCreationReceipt"}}}});
    for (status, description) in [
        (
            "409",
            "idempotency_conflict/issue_schema_unavailable/issue_capacity_exhausted; retryable=false",
        ),
        (
            "415",
            "unsupported_media_type: uncompressed application/json with optional UTF-8 charset required",
        ),
        ("422", "invalid_params: invalid field or canonical UUID key"),
        (
            "413",
            "payload_too_large: creation JSON exceeds524288 bytes",
        ),
        (
            "503",
            "storage_unavailable/service_busy retryable=true; idempotency_unavailable retryable=false, never recreate with a new key",
        ),
    ] {
        create["responses"][status] = error_response(description, true);
    }
    let mut replace = create.clone();
    replace["operationId"] = json!("replaceIssueText");
    replace["description"] = json!(
        "Full title/description replacement in the session-authorized expected-version/idempotent transaction; omitted description clears it. Only content-free stable receipts, no text snapshot. Current rights/trash/archive revalidated even on replay.409 issue_version_conflict requires reload and draft preservation, never automatic overwrite/retry. No query/unknown/duplicate/null fields or arbitrary field editing."
    );
    replace["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    replace["requestBody"] = json!({"required": true, "description": "Uncompressed JSON <=524288 bytes/one-second pre-handler deadline. Required full title and expected_version positive i64 decimal string. Omit description to clear; never null.", "content": {"application/json": {"schema": object(json!({"title": {"type": "string"}, "description": {"type": "string"}, "expected_version": decimal_i64()}), &["title", "expected_version"])}}});
    replace["responses"]["413"] = error_response(
        "payload_too_large: replacement JSON exceeds524288 bytes",
        true,
    );
    let mut success = replace["responses"]["201"].clone();
    success["description"] = json!(
        "Committed replacement, effective no-op or exact replay. changed/result_version describe the original command, not subsequent state."
    );
    success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueTextReceipt"});
    replace["responses"].as_object_mut().unwrap().remove("201");
    replace["responses"]["200"] = success;
    replace["responses"]["409"] = error_response(
        "issue_version_conflict/idempotency_conflict/issue_capacity_exhausted/issue_clock_conflict; retryable=false",
        true,
    );
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/text".into(),
        json!({"put": replace}),
    );
    let mut set_priority = create.clone();
    set_priority["operationId"] = json!("setProjectIssuePriority");
    set_priority["description"] = json!(
        "Set one nontrashed issue's priority with the current expected version. Session, MutateIssues authority, issue eligibility, version, structured history, outbox envelope and immutable idempotency receipt share one transaction. Exact replay returns the original content-free receipt; preserve the same key and request after an uncertain result. A stale version returns409. No query options, explicit null, unknown or duplicate fields."
    );
    set_priority["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    set_priority["requestBody"] = json!({"required": true, "description": "Uncompressed JSON <=524288 bytes/one-second pre-handler deadline. Required expected_version positive i64 decimal string and priority enum. This updates exactly one field.", "content": {"application/json": {"schema": object(json!({"expected_version": decimal_i64(), "priority": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]}}), &["expected_version", "priority"])}}});
    let mut priority_success = set_priority["responses"]["201"].clone();
    priority_success["description"] = json!(
        "Committed priority change, effective no-op or exact replay; changed/priority/result_version describe the original command, not subsequent state."
    );
    priority_success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssuePriorityReceipt"});
    set_priority["responses"]
        .as_object_mut()
        .unwrap()
        .remove("201");
    set_priority["responses"]["200"] = priority_success;
    set_priority["responses"]["409"] = error_response(
        "issue_version_conflict/idempotency_conflict/issue_capacity_exhausted/issue_clock_conflict; retryable=false",
        true,
    );
    set_priority["responses"]["413"] =
        error_response("payload_too_large: priority JSON exceeds524288 bytes", true);
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/priority".into(),
        json!({"put": set_priority}),
    );
    let mut set_due_date = create.clone();
    set_due_date["operationId"] = json!("setProjectIssueDueDate");
    set_due_date["description"] = json!(
        "Set or clear one nontrashed issue's civil due date using its current expected version. The date remains YYYY-MM-DD; overdue is derived using the organization's configured IANA timezone. Session, MutateIssues authority, timezone validity, issue eligibility, version, structured history, content-free outbox envelope and immutable idempotency receipt share one transaction. Past dates are allowed. Exact replay returns the original receipt; preserve the same key and request after an uncertain result. A stale version returns409. No query options, omitted due_date, unknown or duplicate fields."
    );
    set_due_date["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    set_due_date["requestBody"] = json!({"required": true, "description": "Uncompressed JSON <=524288 bytes/one-second pre-handler deadline. Required expected_version positive i64 decimal string and due_date, which is a real canonical YYYY-MM-DD civil date or explicit null to clear. This updates exactly one field.", "content": {"application/json": {"schema": object(json!({"expected_version": decimal_i64(), "due_date": {"anyOf": [{"type": "string", "format": "date"}, {"type": "null"}]}}), &["expected_version", "due_date"])}}});
    let mut due_date_success = set_due_date["responses"]["201"].clone();
    due_date_success["description"] = json!(
        "Committed due-date change, effective no-op or exact replay; changed/due_date/result_version describe the original command, not subsequent state. due_date is null when the command cleared it."
    );
    due_date_success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueDueDateReceipt"});
    set_due_date["responses"]
        .as_object_mut()
        .unwrap()
        .remove("201");
    set_due_date["responses"]["200"] = due_date_success;
    set_due_date["responses"]["409"] = error_response(
        "issue_version_conflict/idempotency_conflict/issue_capacity_exhausted/issue_clock_conflict; retryable=false",
        true,
    );
    set_due_date["responses"]["413"] =
        error_response("payload_too_large: due-date JSON exceeds524288 bytes", true);
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/due-date".into(),
        json!({"put": set_due_date}),
    );
    let mut set_assignee = create.clone();
    set_assignee["operationId"] = json!("setProjectIssueAssignee");
    set_assignee["description"] = json!(
        "Set or clear one nontrashed issue's assignee with the current expected version. The selected user must be an active project manager/member at commit; eligible guests with explicit member role are included. The roster can_be_assigned flag is advisory only. Session, MutateIssues authority, target eligibility, issue version, structured history, content-free outbox envelope and immutable idempotency receipt are checked or written transactionally. Exact replay returns the original receipt and never reapplies the mutation. A stale version returns409. No query options, omitted assignee_user_id, unknown or duplicate fields."
    );
    set_assignee["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    set_assignee["requestBody"] = json!({"required": true, "description": "Uncompressed JSON <=524288 bytes/one-second pre-handler deadline. Required expected_version positive i64 decimal string and assignee_user_id, a canonical lowercase UUID or explicit null to clear. This updates exactly one field.", "content": {"application/json": {"schema": object(json!({"expected_version": decimal_i64(), "assignee_user_id": {"anyOf": [uuid.clone(), {"type": "null"}]}}), &["expected_version", "assignee_user_id"])}}});
    let mut assignee_success = set_assignee["responses"]["201"].clone();
    assignee_success["description"] = json!(
        "Committed assignment change, effective no-op or exact replay; changed/assignee_user_id/result_version describe the original command, and assignee_user_id is null when cleared."
    );
    assignee_success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueAssigneeReceipt"});
    set_assignee["responses"]
        .as_object_mut()
        .unwrap()
        .remove("201");
    set_assignee["responses"]["200"] = assignee_success;
    set_assignee["responses"]["409"] = error_response(
        "issue_version_conflict/idempotency_conflict/issue_capacity_exhausted/issue_clock_conflict; retryable=false",
        true,
    );
    set_assignee["responses"]["413"] =
        error_response("payload_too_large: assignee JSON exceeds524288 bytes", true);
    set_assignee["responses"]["422"] = error_response(
        "invalid_params: invalid assignee_user_id or target is not currently eligible",
        true,
    );
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/assignee".into(),
        json!({"put": set_assignee}),
    );
    let mut move_issue = create.clone();
    move_issue["operationId"] = json!("moveProjectIssue");
    move_issue["description"] = json!(
        "Move or reorder one nontrashed issue using its current expected version and adjacent target-column neighbors. Session, current MutateIssues access, project archive, issue, target visibility, neighbor adjacency, rank allocation/rebalance, issue version, history, outbox and receipt are checked or written in one transaction. Exact replay returns the original content-free receipt; preserve the same key and request after an uncertain result. A stale issue version or neighbor boundary returns409 and requires a board reload. The client never supplies rank. No query options, explicit null, unknown or duplicate fields."
    );
    move_issue["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    move_issue["requestBody"] = json!({"required": true, "description": "Uncompressed JSON <=524288 bytes/one-second pre-handler deadline. Required expected_version positive i64 decimal string and target_status_id UUID. previous_issue_id/next_issue_id are optional adjacent issue UUIDs; omit a missing boundary, never send null. The server derives rank and rebalances atomically when needed.", "content": {"application/json": {"schema": object(json!({"expected_version": decimal_i64(), "target_status_id": uuid.clone(), "previous_issue_id": uuid.clone(), "next_issue_id": uuid.clone()}), &["expected_version", "target_status_id"])}}});
    let mut move_success = move_issue["responses"]["201"].clone();
    move_success["description"] = json!(
        "Committed move, effective no-op or exact replay; changed/status_id/result_version describe the original command, not subsequent state."
    );
    move_success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueMoveReceipt"});
    move_issue["responses"]
        .as_object_mut()
        .unwrap()
        .remove("201");
    move_issue["responses"]["200"] = move_success;
    move_issue["responses"]["409"] = error_response(
        "issue_version_conflict/issue_order_conflict/idempotency_conflict/issue_capacity_exhausted/issue_clock_conflict; retryable=false",
        true,
    );
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/move".into(),
        json!({"post": move_issue}),
    );
    let comment_parameters = vec![
        json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    ];
    let comment_description = "Comments on one visible issue. Session and project read access share one snapshot. The body is Markdown stored as text. Events record created, edited, and removed without the previous body. Archived projects and trashed issues stay readable and reject changes. A viewer can read and cannot create. The author edits and deletes their own comment. A project manager can delete another person's comment and cannot rewrite it.";
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/comments".into(),
        json!({
            "get": operation("listIssueComments", Some("IssueCommentPage"), comment_parameters.clone(), comment_description),
            "head": operation("listIssueCommentsHead", None, comment_parameters, comment_description)
        }),
    );
    let mut create_comment = create.clone();
    create_comment["operationId"] = json!("createIssueComment");
    create_comment["description"] = json!(
        "Add a comment when the signed-in person can mutate issues. Exact replay returns the original content-free receipt. The same key with a different body, project, or issue returns 409. A viewer, an archived project, and a trashed issue are rejected."
    );
    create_comment["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    create_comment["requestBody"] = json!({"required": true, "content": {"application/json": {"schema": object(json!({"body": {"type": "string", "minLength": 1, "maxLength": 65536}}), &["body"])}}});
    create_comment["responses"]["201"]["description"] =
        json!("Committed comment or exact replay. The response does not repeat the body.");
    create_comment["responses"]["201"]["headers"]["Location"] =
        json!({"schema": {"type": "string"}});
    create_comment["responses"]["201"]["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueCommentReceipt"});
    create_comment["responses"]["409"] =
        error_response("idempotency_conflict; retryable=false", true);
    create_comment["responses"]["403"] = error_response(
        "permission_denied: viewer, archived project, or trashed issue",
        true,
    );
    let mut comments = paths
        .remove("/api/v1/projects/{project_id}/issues/{issue_number}/comments")
        .unwrap_or_else(|| json!({}));
    comments["post"] = create_comment;
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/comments".into(),
        comments,
    );
    let mut edit_comment = create.clone();
    edit_comment["operationId"] = json!("editIssueComment");
    edit_comment["description"] = json!(
        "Replace the author's own comment when expected_version matches. A manager cannot rewrite another person's text. Exact replay returns the original receipt. A stale version returns 409."
    );
    edit_comment["parameters"].as_array_mut().unwrap().push(
        json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
    );
    edit_comment["parameters"].as_array_mut().unwrap().push(
        json!({"name": "comment_id", "in": "path", "required": true, "schema": uuid.clone()}),
    );
    edit_comment["requestBody"] = json!({"required": true, "content": {"application/json": {"schema": object(json!({"body": {"type": "string", "minLength": 1, "maxLength": 65536}, "expected_version": decimal_i64()}), &["body", "expected_version"])}}});
    let mut edit_success = edit_comment["responses"]["201"].clone();
    edit_success["description"] = json!(
        "Committed edit, unchanged body, or exact replay. The response does not repeat the body."
    );
    edit_success["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/IssueCommentReceipt"});
    edit_comment["responses"]
        .as_object_mut()
        .unwrap()
        .remove("201");
    edit_comment["responses"]["200"] = edit_success;
    edit_comment["responses"]["409"] = error_response(
        "issue_version_conflict/idempotency_conflict; retryable=false",
        true,
    );
    edit_comment["responses"]["403"] = error_response(
        "permission_denied: only the author can edit, and the project must still accept changes",
        true,
    );
    let mut remove_comment = operation(
        "removeIssueComment",
        None,
        vec![
            json!({"name": "project_id", "in": "path", "required": true, "schema": uuid.clone()}),
            json!({"name": "issue_number", "in": "path", "required": true, "schema": decimal_i64()}),
            json!({"name": "comment_id", "in": "path", "required": true, "schema": uuid.clone()}),
            json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
            json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
            json!({"name": "Idempotency-Key", "in": "header", "required": true, "schema": uuid.clone()}),
        ],
        "Delete the author's own comment, or any comment when the actor is a project manager. History records the removal without the body. Exact replay returns 204 and does not delete again.",
    );
    remove_comment["responses"]
        .as_object_mut()
        .unwrap()
        .remove("200");
    remove_comment["responses"]["204"] = json!({
        "description": "Committed removal or exact replay. Empty body.",
        "headers": {
            "Cache-Control": {"schema": {"type": "string", "const": "no-store"}},
            "Idempotency-Replayed": {"schema": {"type": "string", "enum": ["true", "false"]}}
        }
    });
    paths.insert(
        "/api/v1/projects/{project_id}/issues/{issue_number}/comments/{comment_id}".into(),
        json!({"put": edit_comment, "delete": remove_comment}),
    );
    paths
        .get_mut("/api/v1/projects/{project_id}/issues")
        .unwrap()["post"] = create;
    insert_admin_contract(&mut paths);
    insert_entry_contract(&mut paths);
    let mut document = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Puxiri opt-in browser API — partial development contract",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Disabled (404) unless --browser-origin/PUXIRI_BROWSER_ORIGIN is configured. Listener does not terminate TLS; use an explicitly configured HTTPS origin. Cookie-only authentication; no Authorization fallback. Host must match; Origin and Fetch Metadata are checked when supplied, and mutations require exact Origin plus CSRF. Unknown/duplicate query fields are rejected. Every operation has no-store responses; HEAD has no body, including errors. Browser admission: eight in-flight requests per router instance, no admission queue; path+query <=2048 bytes, headers <=64 entries/16384 aggregate name+value bytes. Matched anonymous setup and login requests, project creation, POST issue creation/move, PUT issue text/priority/due-date/assignee, POST issue comments, PUT comment edits, POST invitations, PUT the mail sender, PUT the MCP grant, POST installation ban and unban, and POST replication capture, local restore, and remote restore accept uncompressed JSON up to 524288 bytes. Setup and login start do not require a session. Project and issue mutations require the session cookie and CSRF. Every other operation requires an empty body. Bodies must finish within one second before any handler/transaction starts. There is no whole-handler timeout or per-client requests-per-second limit. Setup issues the initial administrator session. Later Muira login issues a Puxiri session only when that person already belongs to the installation or consumes a pending invitation; otherwise the callback stores no account and sends the browser to the invitation-required page. A Muira token never authorizes a Puxiri call. MCP accepts that token only as identity after an invitation and a separate Puxiri grant. Installation administration is separate from organization ownership. This contract issues no API token. Do not store bearer tokens in localStorage. This document is generated locally, not served as an endpoint or a production readiness claim."
        },
        "security": [{"browserSession": []}],
        "paths": paths,
        "components": {
            "securitySchemes": {"browserSession": {"type": "apiKey", "in": "cookie", "name": "__Host-puxiri_session", "description": "Opaque bearer cookie: Secure, HttpOnly, SameSite=Lax, Path=/, no Domain. Ambiguous/duplicate credentials are refused."}},
            "schemas": {
                "CurrentSession": object(json!({"user_id": uuid.clone(), "session_id": uuid.clone(), "csrf_token": {"type": "string", "minLength": 43, "maxLength": 43}}), &["user_id", "session_id", "csrf_token"]),
                "Session": object(json!({"id": uuid.clone(), "created_at": timestamp.clone(), "last_activity_at": timestamp.clone(), "expires_at": timestamp.clone(), "is_current": {"type": "boolean"}}), &["id", "created_at", "last_activity_at", "expires_at", "is_current"]),
                "Project": object(json!({"id": uuid.clone(), "project_key": {"type": "string"}, "name": {"type": "string"}, "role": {"type": "string", "enum": ["manager", "member", "viewer"]}, "archived_at": timestamp.clone()}), &["id", "project_key", "name", "role"]),
                "OperationalProject": object(json!({"id": uuid, "project_key": {"type": "string"}, "name": {"type": "string"}, "archived_at": timestamp, "membership_count": {"type": "integer", "minimum": 0}, "issue_count": {"type": "integer", "minimum": 0}}), &["id", "project_key", "name", "membership_count", "issue_count"]),
                "ProjectMember": object(json!({"user_id": uuid.clone(), "display_name": {"type": "string", "minLength": 1}, "role": {"type": "string", "enum": ["manager", "member", "viewer"]}, "primary_email": {"type": "string"}, "can_be_assigned": {"type": "boolean"}}), &["user_id", "display_name", "role", "can_be_assigned"]),
                "SessionPage": page("Session"),
                "ProjectPage": page("Project"),
                "OperationalProjectPage": page("OperationalProject"),
                "ProjectMemberPage": page("ProjectMember"),
                "Issue": object(json!({
                    "id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(),
                    "issue_key": {"type": "string"}, "issue_type_id": uuid.clone(), "status_id": uuid.clone(),
                    "title": {"type": "string", "minLength": 1, "maxLength": 240},
                    "assignee_user_id": uuid.clone(), "creator_user_id": uuid.clone(),
                    "priority": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]},
                    "due_date": {"type": "string", "format": "date"}, "is_overdue": {"type": "boolean"}, "version": decimal_i64(),
                    "created_at": timestamp.clone(), "updated_at": timestamp.clone()
                }), &["id", "project_id", "issue_number", "issue_key", "issue_type_id", "status_id", "title", "creator_user_id", "priority", "is_overdue", "version", "created_at", "updated_at"]),
                "IssuePage": page("Issue"),
                "IssueTextReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "result_version": decimal_i64(), "changed": {"type": "boolean"}}), &["id", "project_id", "issue_number", "issue_key", "result_version", "changed"]),
                "IssuePriorityReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "priority": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]}, "result_version": decimal_i64(), "changed": {"type": "boolean"}}), &["id", "project_id", "issue_number", "issue_key", "priority", "result_version", "changed"]),
                "IssueDueDateReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "due_date": {"anyOf": [{"type": "string", "format": "date"}, {"type": "null"}]}, "result_version": decimal_i64(), "changed": {"type": "boolean"}}), &["id", "project_id", "issue_number", "issue_key", "due_date", "result_version", "changed"]),
                "IssueAssigneeReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "assignee_user_id": {"anyOf": [uuid.clone(), {"type": "null"}]}, "result_version": decimal_i64(), "changed": {"type": "boolean"}}), &["id", "project_id", "issue_number", "issue_key", "assignee_user_id", "result_version", "changed"]),
                "ProjectCapabilities": object(json!({"project_id": uuid.clone(), "can_create_issues": {"type": "boolean"}, "can_update_issue_fields": {"type": "boolean"}, "can_replace_issue_text": {"type": "boolean"}, "can_move_issues": {"type": "boolean"}}), &["project_id", "can_create_issues", "can_update_issue_fields", "can_replace_issue_text", "can_move_issues"]),
                "IssueCreationReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "created_version": {"type": "string", "enum": ["1"]}}), &["id", "project_id", "issue_number", "issue_key", "created_version"]),
                "IssueMoveReceipt": object(json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "issue_key": {"type": "string"}, "status_id": uuid.clone(), "result_version": decimal_i64(), "changed": {"type": "boolean"}}), &["id", "project_id", "issue_number", "issue_key", "status_id", "result_version", "changed"]),
                "Error": object(json!({"code": {"type": "string"}, "message": {"type": "string"}, "request_id": {"type": "string", "format": "uuid"}, "field_errors": {"type": "object", "additionalProperties": {"type": "string"}}, "retryable": {"type": "boolean"}}), &["code", "message", "request_id", "field_errors", "retryable"])
            }
        }
    });
    {
        let schemas = document["components"]["schemas"]
            .as_object_mut()
            .expect("openapi schemas");
        schemas.insert(
            "InstallationOverview".into(),
            object(
                json!({"login_source": {"type": "string", "enum": ["puxiri", "muira"]}, "mail_configured": {"type": "boolean"}, "replication": {"$ref": "#/components/schemas/ReplicationStatus"}}),
                &["mail_configured", "replication"],
            ),
        );
        schemas.insert(
            "ReplicationStatus".into(),
            object(
                json!({"snapshot": {"type": "boolean"}, "txid": {"type": "string", "minLength": 1}, "same_filesystem": {"type": "boolean"}, "generation_started_at": {"type": "string", "format": "date-time", "description": "When the current generation began from a fresh snapshot. Omitted before the first capture."}, "capture_interval_seconds": {"type": "integer", "minimum": 1, "description": "Seconds between automatic captures. Omitted when capture is left to an administrator."}, "generation_hours": {"type": "integer", "minimum": 1}, "retained_generations": {"type": "integer", "minimum": 1, "description": "Generations kept at the S3 destination, the current one included. Locally only the current generation is kept."}, "remote": {"$ref": "#/components/schemas/ReplicationRemoteStatus"}}),
                &["snapshot", "same_filesystem", "generation_hours", "retained_generations"],
            ),
        );
        schemas.insert(
            "ReplicationRemoteStatus".into(),
            object(
                json!({"location": {"type": "string", "minLength": 1, "description": "s3://bucket/prefix. Never includes credentials."}, "acknowledged_txid": {"type": "string", "minLength": 1, "description": "Newest point this destination acknowledged after confirmed uploads. Omitted until the first successful upload."}}),
                &["location"],
            ),
        );
        schemas.insert(
            "InstallationPerson".into(),
            object(
                json!({"user_id": uuid.clone(), "display_name": {"type": "string", "minLength": 1}, "organization_role": {"type": "string", "enum": ["owner", "admin", "member", "guest"]}, "state": {"type": "string", "enum": ["active", "banned", "disabled"]}, "installation_admin": {"type": "boolean"}, "primary_email": {"type": "string", "minLength": 1}}),
                &[
                    "user_id",
                    "display_name",
                    "organization_role",
                    "state",
                    "installation_admin",
                ],
            ),
        );
        schemas.insert(
            "InstallationPeople".into(),
            object(
                json!({"data": {"type": "array", "maxItems": 200, "items": {"$ref": "#/components/schemas/InstallationPerson"}}, "has_more": {"type": "boolean"}}),
                &["data", "has_more"],
            ),
        );
        schemas.insert(
            "AuditRecord".into(),
            object(
                json!({"id": uuid.clone(), "event_kind": {"type": "string", "minLength": 1}, "result": {"type": "string", "enum": ["success", "failure"]}, "actor_user_id": uuid.clone(), "target_kind": {"type": "string", "minLength": 1}, "target_id": uuid.clone(), "reason": {"type": "string", "minLength": 1}, "occurred_at": timestamp.clone()}),
                &[
                    "id",
                    "event_kind",
                    "result",
                    "target_kind",
                    "target_id",
                    "occurred_at",
                ],
            ),
        );
        schemas.insert(
            "AuditPage".into(),
            object(
                json!({"data": {"type": "array", "maxItems": 50, "items": {"$ref": "#/components/schemas/AuditRecord"}}, "has_more": {"type": "boolean"}}),
                &["data", "has_more"],
            ),
        );
        schemas.insert(
            "AccountChange".into(),
            object(json!({"changed": {"type": "boolean"}}), &["changed"]),
        );
        schemas.insert(
            "ReplicationCapture".into(),
            object(
                json!({"txid": {"type": "string", "minLength": 1}, "transactions": {"type": "integer", "minimum": 0}, "wal_frames": {"type": "integer", "minimum": 0}, "snapshot_created": {"type": "boolean"}, "generation_started": {"type": "boolean"}, "remote": {"$ref": "#/components/schemas/ReplicationRemoteSync"}}),
                &["txid", "transactions", "wal_frames", "snapshot_created", "generation_started"],
            ),
        );
        schemas.insert(
            "ReplicationRemoteSync".into(),
            object(
                json!({"status": {"type": "string", "enum": ["synced", "failed"]}, "txid": {"type": "string", "minLength": 1}, "objects_confirmed": {"type": "integer", "minimum": 0}, "bytes_confirmed": {"type": "integer", "minimum": 0}}),
                &["status"],
            ),
        );
        schemas.insert(
            "ReplicationRestore".into(),
            object(
                json!({"file_name": {"type": "string", "minLength": 1, "maxLength": 80}}),
                &["file_name"],
            ),
        );
    }
    let mut detail = document["components"]["schemas"]["Issue"].clone();
    let mut card = detail.clone();
    card["properties"]["rank"] = json!({"type": "string", "pattern": "^(0|-?[1-9][0-9]{0,18})$", "maxLength": 20, "description": "Read-only canonical signed i64 decimal, from -9223372036854775808 through 9223372036854775807."});
    card["required"].as_array_mut().unwrap().push(json!("rank"));
    document["components"]["schemas"]["BoardCard"] = card;
    let mut column = page("BoardCard");
    column["properties"]["total_count"] = json!({"type": "string", "pattern": "^(0|[1-9][0-9]{0,18})$", "maxLength": 19, "description": "All nontrashed issues in this selected column, as a nonnegative i64 decimal string."});
    column["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("total_count"));
    document["components"]["schemas"]["BoardColumnPage"] = column;
    detail["properties"]["description"] = json!({"type": "string", "maxLength": 65536, "description": "Optional Markdown data, capped at 65536 UTF-8 bytes by the domain; never HTML-rendered by this endpoint."});
    detail["properties"]["completed_at"] = timestamp.clone();
    detail["properties"]["trashed_at"] = timestamp.clone();
    document["components"]["schemas"]["IssueDetail"] = detail;
    let priority_change = object(
        json!({
            "field": {"type": "string", "const": "priority"},
            "before": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]},
            "after": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]}
        }),
        &["field", "before", "after"],
    );
    let due_date_change = object(
        json!({
            "field": {"type": "string", "const": "due_date"},
            "before": {"anyOf": [{"type": "string", "format": "date"}, {"type": "null"}]},
            "after": {"anyOf": [{"type": "string", "format": "date"}, {"type": "null"}]}
        }),
        &["field", "before", "after"],
    );
    let assignee_change = object(
        json!({
            "field": {"type": "string", "const": "assignee_user_id"},
            "before": {"anyOf": [uuid.clone(), {"type": "null"}]},
            "after": {"anyOf": [uuid.clone(), {"type": "null"}]}
        }),
        &["field", "before", "after"],
    );
    let mut history_event = object(
        json!({
            "id": uuid.clone(),
            "project_id": uuid.clone(),
            "issue_id": uuid.clone(),
            "actor_user_id": uuid,
            "event_kind": {"type": "string", "enum": ["issue.created", "issue.title_updated", "issue.description_updated", "issue.text_updated", "issue.moved", "issue.field_updated"]},
            "change": {"oneOf": [priority_change, due_date_change, assignee_change]},
            "occurred_at": {"type": "string", "format": "date-time"}
        }),
        &[
            "id",
            "project_id",
            "issue_id",
            "actor_user_id",
            "event_kind",
            "occurred_at",
        ],
    );
    history_event["allOf"] = json!([
        {
            "if": {
                "properties": {"event_kind": {"const": "issue.field_updated"}},
                "required": ["event_kind"]
            },
            "then": {"required": ["change"]}
        },
        {
            "if": {
                "properties": {"event_kind": {"not": {"const": "issue.field_updated"}}},
                "required": ["event_kind"]
            },
            "then": {"not": {"required": ["change"]}}
        }
    ]);
    document["components"]["schemas"]["IssueHistoryEvent"] = history_event;
    document["components"]["schemas"]["IssueHistoryPage"] = page("IssueHistoryEvent");
    let mut properties = json!({
        "id": {"type": "string", "format": "uuid"},
        "project_id": {"type": "string", "format": "uuid"},
        "name": {"type": "string", "minLength": 1},
        "system_key": {"type": "string", "pattern": "^[a-z0-9_-]{1,48}$"},
        "archived_at": {"type": "string", "format": "date-time"}
    });
    properties["icon_key"] = json!({"type": "string", "pattern": "^[a-z0-9_-]{1,48}$", "description": "Opaque existing token; no visual palette is defined by this read route."});
    properties["color_key"] = properties["icon_key"].clone();
    document["components"]["schemas"]["IssueType"] =
        object(properties.clone(), &["id", "project_id", "name"]);
    properties.as_object_mut().unwrap().remove("icon_key");
    properties.as_object_mut().unwrap().remove("color_key");
    properties["category"] =
        json!({"type": "string", "enum": ["backlog", "active", "done", "canceled"]});
    properties["position"] = json!({"type": "string", "pattern": "^(0|[1-9][0-9]{0,18})$", "maxLength": 19, "description": "Nonnegative i64 as lossless decimal string, maximum9223372036854775807. Board display order, distinct from UUID pagination order."});
    properties["board_visible"] = json!({"type": "boolean"});
    document["components"]["schemas"]["Status"] = object(
        properties,
        &[
            "id",
            "project_id",
            "name",
            "category",
            "position",
            "board_visible",
        ],
    );
    document["components"]["schemas"]["StatusPage"] = page("Status");
    let mut board_status = document["components"]["schemas"]["Status"].clone();
    board_status["properties"]
        .as_object_mut()
        .unwrap()
        .remove("archived_at");
    board_status["properties"]["board_visible"] = json!({"type": "boolean", "const": true});
    document["components"]["schemas"]["BoardStatus"] = board_status;
    document["components"]["schemas"]["BoardStatusPage"] = page("BoardStatus");
    document["components"]["schemas"]["IssueTypePage"] = page("IssueType");
    document["components"]["schemas"]["InstallationStatus"] = object(
        json!({"setup_required": {"type": "boolean"}, "login": {"type": "string", "enum": ["puxiri", "muira"]}}),
        &["setup_required"],
    );
    document["components"]["schemas"]["MuiraAuthorization"] = object(
        json!({"authorization_url": {"type": "string", "format": "uri"}}),
        &["authorization_url"],
    );
    document["components"]["schemas"]["IssueComment"] = object(
        json!({"id": uuid.clone(), "author_user_id": uuid.clone(), "author_display_name": {"type": "string"}, "body": {"type": "string"}, "version": decimal_i64(), "can_edit": {"type": "boolean"}, "can_delete": {"type": "boolean"}, "created_at": timestamp.clone(), "updated_at": timestamp.clone()}),
        &[
            "id",
            "author_user_id",
            "author_display_name",
            "body",
            "version",
            "can_edit",
            "can_delete",
            "created_at",
            "updated_at",
        ],
    );
    document["components"]["schemas"]["IssueCommentEvent"] = object(
        json!({"id": uuid.clone(), "comment_id": uuid.clone(), "actor_user_id": uuid.clone(), "event_kind": {"type": "string", "enum": ["comment.created", "comment.edited", "comment.removed"]}, "occurred_at": timestamp.clone()}),
        &[
            "id",
            "comment_id",
            "actor_user_id",
            "event_kind",
            "occurred_at",
        ],
    );
    document["components"]["schemas"]["IssueCommentPage"] = object(
        json!({"comments": {"type": "array", "items": {"$ref": "#/components/schemas/IssueComment"}}, "events": {"type": "array", "items": {"$ref": "#/components/schemas/IssueCommentEvent"}}, "has_more": {"type": "boolean"}, "next_cursor": uuid.clone()}),
        &["comments", "events", "has_more"],
    );
    document["components"]["schemas"]["IssueCommentReceipt"] = object(
        json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(), "result_version": decimal_i64(), "changed": {"type": "boolean"}}),
        &["id", "project_id", "issue_number", "changed"],
    );
    document["components"]["schemas"]["McpGrant"] = object(
        json!({"configured": {"type": "boolean"}, "scope": {"type": "string", "enum": ["read", "write"]}, "all_projects": {"type": "boolean"}, "projects": {"type": "array", "items": {"type": "string"}}}),
        &["configured"],
    );
    document["components"]["schemas"]["IssuedInvitation"] = object(
        json!({"id": uuid.clone(), "expires_at": timestamp.clone(), "delivery": {"type": "string", "enum": ["queued", "unconfigured", "rejected", "unknown"]}, "invite_url": {"type": "string"}, "token": {"type": "string", "minLength": 43, "maxLength": 43}, "provider_message_id": {"type": "string"}}),
        &["id", "expires_at", "delivery", "invite_url", "token"],
    );
    document["components"]["schemas"]["MailSender"] = object(
        json!({"configured": {"type": "boolean"}, "provider": {"type": "string", "enum": ["sabia", "resend", "millionsend"]}, "from_address": {"type": "string"}, "from_name": {"type": "string"}, "template": {"type": "string"}, "origin": {"type": "string"}}),
        &["configured"],
    );
    document["components"]["schemas"]["CreatedProject"] = object(
        json!({
            "id": {"type": "string", "format": "uuid"},
            "project_key": {"type": "string", "pattern": "^[A-Z]{2,6}$"},
            "name": {"type": "string", "minLength": 1},
            "description": {"type": "string"},
            "default_view": {"type": "string", "enum": ["board", "list"]}
        }),
        &["id", "project_key", "name", "default_view"],
    );
    document
}

fn insert_admin_contract(paths: &mut serde_json::Map<String, Value>) {
    let headers = vec![
        json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
        json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
    ];
    let mut person_parameters = vec![
        json!({"name": "person_id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}),
    ];
    person_parameters.extend(headers.clone());
    let overview = "Installation administration only. Organization ownership is not enough. Shows the login source when setup has chosen one, whether an email sender is configured, the local replication watermark, and, when an S3-compatible destination is configured, its location and the newest point it acknowledged. A local replica on the same filesystem does not survive loss of that disk. This response has no issue text.";
    paths.insert(
        "/api/v1/admin/overview".into(),
        json!({
            "get": operation("installationOverview", Some("InstallationOverview"), vec![], overview),
            "head": operation("installationOverviewHead", None, vec![], overview),
        }),
    );
    let people = "People in the organization: display name, organization role, account state, installation-admin flag, and primary email when one exists. Installation administration is required. This is not an issue reader.";
    paths.insert(
        "/api/v1/admin/people".into(),
        json!({
            "get": operation("listInstallationPeople", Some("InstallationPeople"), vec![], people),
            "head": operation("listInstallationPeopleHead", None, vec![], people),
        }),
    );
    paths.insert(
        "/api/v1/admin/people/{person_id}/ban".into(),
        json!({"post": authenticated_mutation(
            "banInstallationPerson",
            "AccountChange",
            "200",
            "Ban an account. Memberships stay. Credentials that could become valid again are revoked. The reason is stored on the audit event. The last installation administrator cannot be removed this way.",
            object(json!({"reason": {"type": "string", "minLength": 1, "maxLength": 500}}), &["reason"]),
            person_parameters.clone(),
        )}),
    );
    paths.insert(
        "/api/v1/admin/people/{person_id}/unban".into(),
        json!({"post": authenticated_mutation(
            "unbanInstallationPerson",
            "AccountChange",
            "200",
            "Restore a banned account to active. Revoked sessions are not recreated.",
            object(json!({}), &[]),
            person_parameters,
        )}),
    );
    let catalog = "Project metadata for an installation administrator: key, name, archive time, membership count, and issue count. The first 100 projects are returned in project-key order. This does not include issue text. Organization ownership alone is not this permission.";
    paths.insert(
        "/api/v1/admin/catalog".into(),
        json!({
            "get": operation("listInstallationCatalog", Some("OperationalProjectPage"), vec![], catalog),
            "head": operation("listInstallationCatalogHead", None, vec![], catalog),
        }),
    );
    let audit_text = "The 50 newest audit events: kind, result, actor, target, and the reason when the administrator recorded one. Issue text is not included.";
    paths.insert(
        "/api/v1/admin/audit".into(),
        json!({
            "get": operation("listInstallationAudit", Some("AuditPage"), vec![], audit_text),
            "head": operation("listInstallationAuditHead", None, vec![], audit_text),
        }),
    );
    paths.insert(
        "/api/v1/admin/replication/capture".into(),
        json!({"post": authenticated_mutation(
            "captureReplication",
            "ReplicationCapture",
            "200",
            "Create the initial snapshot when needed, then capture the WAL through sqlite-replication while application writers are paused. When the current generation is older than the configured age, the capture starts a new generation from a fresh snapshot and removes the previous local one. When an S3-compatible destination is configured, the generation is then uploaded with immutable keys and remote is reported; an upload failure leaves the local capture in place and the next capture retries it. After an acknowledged upload, remote generations beyond the retention count are deleted; the current one never is. 409 replication_busy while a scheduled capture or a restore is running. Only a local RustFS endpoint has been tested; AWS S3, R2, and other providers are not certified. This does not promise survival of power loss.",
            object(json!({}), &[]),
            headers.clone(),
        )}),
    );
    paths.insert(
        "/api/v1/admin/replication/restore".into(),
        json!({"post": authenticated_mutation(
            "restoreReplication",
            "ReplicationRestore",
            "200",
            "Restore the newest verified local point into a new file under the data directory. The live database is not replaced. Stop Puxiri before substituting that file by hand.",
            object(json!({}), &[]),
            headers.clone(),
        )}),
    );
    paths.insert(
        "/api/v1/admin/replication/remote/restore".into(),
        json!({"post": authenticated_mutation(
            "restoreRemoteReplication",
            "ReplicationRestore",
            "200",
            "Download and verify the newest complete generation from the configured S3-compatible destination in temporary space, then restore its newest point into a new file under the data directory. 409 replication_remote_not_configured when no destination is configured; 503 replication_remote_unavailable when the destination cannot be read. The live database is not replaced.",
            object(json!({}), &[]),
            headers,
        )}),
    );
}

fn insert_entry_contract(paths: &mut serde_json::Map<String, Value>) {
    let status = "Whether this installation still needs its initial administrator, and which login source is active. No session, query, or body. login is omitted until setup completes.";
    let mut status_get = operation(
        "installationStatus",
        Some("InstallationStatus"),
        vec![],
        status,
    );
    let mut status_head = operation("installationStatusHead", None, vec![], status);
    status_get["security"] = json!([]);
    status_head["security"] = json!([]);
    paths.insert(
        "/api/v1/setup/status".into(),
        json!({"get": status_get, "head": status_head}),
    );
    paths.insert(
        "/api/v1/setup/puxiri".into(),
        json!({"post": anonymous_mutation("completePuxiriSetup", "CurrentSession", "Create the installation, its owner, and the built-in login source. The setup token is consumed only when the owner and session commit. Password policy failures are 422. A consumed or unknown token is 409.", object(json!({"setup_token": {"type": "string", "minLength": 43, "maxLength": 43}, "organization_name": {"type": "string", "minLength": 1}, "locale": {"type": "string", "enum": ["pt-BR", "en"]}, "timezone": {"type": "string", "minLength": 1}, "display_name": {"type": "string", "minLength": 1}, "email": {"type": "string"}, "password": {"type": "string", "minLength": 10}}), &["setup_token", "organization_name", "locale", "timezone", "display_name", "email", "password"]))}),
    );
    paths.insert(
        "/api/v1/setup/muira".into(),
        json!({"post": anonymous_mutation("beginMuiraSetup", "MuiraAuthorization", "Store the Muira client secret in the installation envelope and return the authorize URL. No session is issued until the callback verifies the same email. The setup token stays usable until that callback commits.", object(json!({"setup_token": {"type": "string", "minLength": 43, "maxLength": 43}, "organization_name": {"type": "string", "minLength": 1}, "locale": {"type": "string", "enum": ["pt-BR", "en"]}, "timezone": {"type": "string", "minLength": 1}, "display_name": {"type": "string", "minLength": 1}, "email": {"type": "string"}, "issuer": {"type": "string", "format": "uri"}, "client_id": {"type": "string", "minLength": 1}, "client_secret": {"type": "string", "minLength": 1, "maxLength": 512}}), &["setup_token", "organization_name", "locale", "timezone", "display_name", "email", "issuer", "client_id", "client_secret"]))}),
    );
    paths.insert(
        "/api/v1/auth/muira/login".into(),
        json!({"post": anonymous_mutation("beginMuiraLogin", "MuiraAuthorization", "Start Muira login for an installation that already chose Muira. An empty JSON object is required. Membership is checked only on the callback.", object(json!({}), &[]))}),
    );
    paths.insert(
        "/api/v1/auth/puxiri/login".into(),
        json!({"post": anonymous_mutation("loginPuxiri", "CurrentSession", "Check the built-in password credential. Unknown email, a wrong password, and a password submitted to a Muira installation share one response. The message does not reveal which login source is configured.", object(json!({"email": {"type": "string"}, "password": {"type": "string"}}), &["email", "password"]))}),
    );
    let mut callback = json!({
        "operationId": "completeMuiraLogin",
        "description": "Top-level return from Muira. The state query must match the __Host-puxiri_muira cookie. HEAD is rejected before the authorization code is exchanged. Success sets the Puxiri session cookie and redirects to /projects. A verified person without a membership or a matching invitation is redirected to /convite and no account or session is stored. A Muira token is not accepted on any other Puxiri route.",
        "security": [],
        "parameters": [
            {"name": "code", "in": "query", "required": true, "schema": {"type": "string", "minLength": 1, "maxLength": 1024}},
            {"name": "state", "in": "query", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}
        ],
        "responses": {
            "303": {"description": "Browser redirect. Location is /projects, /convite, or /?erro=identidade|setup|login|indisponivel.", "headers": {"Cache-Control": {"schema": {"type": "string", "const": "no-store"}}, "Location": {"schema": {"type": "string"}}}},
            "400": error_response("malformed_request: duplicate, unknown, or malformed callback query", true),
            "403": error_response("request_origin_denied: host or fetch metadata rejected. HEAD is method_not_allowed before exchange.", true),
            "405": error_response("method_not_allowed: HEAD does not exchange the code", true)
        }
    });
    callback["responses"]["401"] = error_response(
        "unauthenticated: not used; identity failure is a redirect",
        true,
    );
    paths.insert(
        "/api/v1/auth/muira/callback".into(),
        json!({"get": callback}),
    );
    let mut project = paths
        .remove("/api/v1/projects")
        .unwrap_or_else(|| json!({}));
    project["post"] = authenticated_mutation(
        "createProject",
        "CreatedProject",
        "201",
        "Create a project when the signed-in person is an organization owner or admin. The creator becomes the project manager. Members and guests receive 403.",
        object(
            json!({"project_key": {"type": "string", "pattern": "^[A-Z]{2,6}$"}, "name": {"type": "string", "minLength": 1}, "description": {"type": "string"}}),
            &["project_key", "name"],
        ),
        vec![
            json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
            json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
        ],
    );
    paths.insert("/api/v1/projects".into(), project);
    paths.insert(
        "/api/v1/invitations".into(),
        json!({"post": authenticated_mutation(
            "createInvitation",
            "IssuedInvitation",
            "201",
            "Issue or reissue an organization invitation. Email is attempted only when one sender is configured. Acceptance by Sabiá, Resend, or MillionSend means queued, not delivered. An unconfigured sender still returns the one-time token so the operator can copy the invitation. A Muira login does not create this invitation.",
            object(json!({"email": {"type": "string"}, "role": {"type": "string", "enum": ["member", "guest"]}, "projects": {"type": "array", "items": {"type": "object"}}}), &["email", "role"]),
            vec![
                json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
                json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
            ],
        )}),
    );
    let mail_get = operation(
        "mailSender",
        Some("MailSender"),
        vec![],
        "The active email sender without its API key. configured is false when the installation has no sender. One sender is active; there is no fallback.",
    );
    let mail_head = operation(
        "mailSenderHead",
        None,
        vec![],
        "The active email sender without its API key.",
    );
    paths.insert(
        "/api/v1/mail/sender".into(),
        json!({
            "get": mail_get,
            "head": mail_head,
            "put": authenticated_mutation(
                "replaceMailSender",
                "MailSender",
                "200",
                "Replace the only email sender. The API key is stored encrypted and is not returned. Sabiá uses a published template and origin. Resend and MillionSend are separate direct clients.",
                object(json!({"provider": {"type": "string", "enum": ["sabia", "resend", "millionsend"]}, "api_key": {"type": "string", "minLength": 1, "maxLength": 512}, "from_address": {"type": "string"}, "from_name": {"type": "string"}, "template": {"type": "string"}, "origin": {"type": "string"}}), &["provider", "api_key"]),
                vec![
                    json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
                    json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
                ],
            )
        }),
    );
    let grant_parameters = vec![
        json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}),
        json!({"name": "x-csrf-token", "in": "header", "required": true, "schema": {"type": "string", "minLength": 43, "maxLength": 43}}),
    ];
    let grant_get = operation(
        "mcpGrant",
        Some("McpGrant"),
        vec![],
        "The signed-in person's MCP grant. configured is false when no grant exists. A Muira token is not this grant.",
    );
    let grant_head = operation(
        "mcpGrantHead",
        None,
        vec![],
        "The signed-in person's MCP grant.",
    );
    let mut grant_delete = authenticated_mutation(
        "revokeMcpGrant",
        "McpGrant",
        "200",
        "Remove the signed-in person's MCP grant. Later Muira tokens no longer open MCP.",
        object(json!({}), &[]),
        grant_parameters.clone(),
    );
    grant_delete.as_object_mut().unwrap().remove("requestBody");
    grant_delete["responses"]
        .as_object_mut()
        .unwrap()
        .remove("200");
    grant_delete["responses"]["204"] = json!({
        "description": "The grant is absent.",
        "headers": {"Cache-Control": {"schema": {"type": "string", "const": "no-store"}}}
    });
    paths.insert(
        "/api/v1/mcp/grant".into(),
        json!({
            "get": grant_get,
            "head": grant_head,
            "put": authenticated_mutation(
                "replaceMcpGrant",
                "McpGrant",
                "200",
                "Replace the signed-in person's MCP grant. all_projects covers current and future memberships. Otherwise every listed project must already include this person. Read cannot call write tools.",
                object(json!({"scope": {"type": "string", "enum": ["read", "write"]}, "all_projects": {"type": "boolean"}, "projects": {"type": "array", "items": {"type": "string"}}}), &["scope", "all_projects"]),
                grant_parameters,
            ),
            "delete": grant_delete,
        }),
    );
}

fn anonymous_mutation(id: &str, schema: &str, description: &str, body: Value) -> Value {
    let mut operation = authenticated_mutation(id, schema, "200", description, body, vec![]);
    operation["security"] = json!([]);
    operation
}

fn authenticated_mutation(
    id: &str,
    schema: &str,
    success_status: &str,
    description: &str,
    body: Value,
    mut parameters: Vec<Value>,
) -> Value {
    if parameters.is_empty() {
        parameters.push(json!({"name": "Origin", "in": "header", "required": true, "schema": {"type": "string"}}));
    }
    let success = json!({
        "description": "Committed result. Setup and built-in login also set the session cookie. Muira start sets only the short-lived state cookie.",
        "headers": {"Cache-Control": {"schema": {"type": "string", "const": "no-store"}}},
        "content": {"application/json": {"schema": {"$ref": format!("#/components/schemas/{schema}")}}}
    });
    let mut responses = serde_json::Map::new();
    responses.insert(success_status.to_owned(), success);
    for (status, message) in [
        (
            "400",
            "malformed_request: malformed or unsupported JSON, or an unexpected query",
        ),
        (
            "401",
            "unauthenticated: credentials were not accepted, or no active session",
        ),
        (
            "403",
            "request_origin_denied/csrf_required/permission_denied",
        ),
        (
            "408",
            "request_timeout: body verification did not finish before any handler started; retryable=true",
        ),
        (
            "409",
            "setup_unavailable/project_key_unavailable/replication_busy/replication_remote_not_configured",
        ),
        ("413", "payload_too_large: JSON exceeds 524288 bytes"),
        ("414", "uri_too_long"),
        (
            "415",
            "unsupported_media_type: uncompressed application/json required",
        ),
        (
            "422",
            "invalid_params: field_errors identifies the invalid field",
        ),
        ("431", "headers_too_large"),
        ("500", "unexpected_error"),
        (
            "503",
            "storage_unavailable/service_busy/replication_remote_unavailable; retryable=true",
        ),
    ] {
        responses.insert(status.into(), error_response(message, true));
    }
    json!({
        "operationId": id,
        "description": description,
        "parameters": parameters,
        "requestBody": {"required": true, "content": {"application/json": {"schema": body}}},
        "responses": responses
    })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

fn decimal_i64() -> Value {
    json!({"type": "string", "pattern": "^[1-9][0-9]{0,18}$", "maxLength": 19, "description": "Canonical decimal integer from 1 through 9223372036854775807, encoded as a string to preserve i64 precision in JavaScript."})
}

fn page(item: &str) -> Value {
    object(
        json!({
            "data": {"type": "array", "maxItems": 100, "items": {"$ref": format!("#/components/schemas/{item}")}},
            "has_more": {"type": "boolean"},
            "next_cursor": {"type": "string", "description": "Omitted when has_more=false; never null."}
        }),
        &["data", "has_more"],
    )
}

fn operation(id: &str, schema: Option<&str>, parameters: Vec<Value>, description: &str) -> Value {
    let mut success = json!({"description": "Successful read; HEAD has an empty body.", "headers": {"Cache-Control": {"schema": {"type": "string", "const": "no-store"}}}});
    if let Some(schema) = schema {
        success["content"] = json!({"application/json": {"schema": {"$ref": format!("#/components/schemas/{schema}")}}});
    }
    let mut responses = serde_json::Map::new();
    responses.insert("200".into(), success);
    for (status, message) in [
        (
            "400",
            "malformed_request: unsupported, duplicate or malformed query fields",
        ),
        ("401", "unauthenticated: exact active bearer required"),
        (
            "403",
            "request_origin_denied/csrf_required/permission_denied: transport, CSRF or operational catalog authority denied",
        ),
        (
            "408",
            "request_timeout: body verification did not finish before any handler started; retryable=true",
        ),
        (
            "413",
            "payload_too_large: these routes accept no body bytes",
        ),
        (
            "414",
            "uri_too_long: path and query exceed the browser URI budget",
        ),
        (
            "431",
            "headers_too_large: browser header count or byte budget exceeded",
        ),
        (
            "404",
            "not_found: browser API opt-in disabled, or selected private resource absent/inaccessible/already revoked",
        ),
        (
            "500",
            "unexpected_error: generic internal or corrupt stored-data failure",
        ),
        (
            "503",
            "storage_unavailable/service_busy: persistence/audit unavailable or browser admission saturated; retryable=true. service_busy includes Retry-After: 1 as minimum backoff, not a readiness promise",
        ),
    ] {
        responses.insert(status.into(), error_response(message, schema.is_some()));
    }
    responses.get_mut("503").unwrap()["headers"]["Retry-After"] = json!({"schema": {"type": "string", "const": "1"}, "description": "Present only for service_busy admission rejection."});
    if parameters.iter().any(|parameter| {
        parameter["name"] == "page_size"
            || parameter["name"] == "session_id"
            || parameter["name"] == "issue_number"
            || parameter["name"] == "q"
            || parameter["name"] == "record_id"
            || parameter["name"] == "project_id"
    }) {
        responses.insert(
            "422".into(),
            error_response(
                "invalid_params: field_errors identifies the invalid field",
                schema.is_some(),
            ),
        );
    }
    // DELETE uses the JSON error envelope too, unlike HEAD.
    if parameters
        .iter()
        .any(|parameter| parameter["name"] == "x-csrf-token")
    {
        for response in responses.values_mut() {
            response["content"] =
                json!({"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}});
        }
    }
    json!({"operationId": id, "description": description, "parameters": parameters, "responses": responses})
}

fn error_response(description: &str, body: bool) -> Value {
    let mut response = json!({"description": description, "headers": {"Cache-Control": {"schema": {"type": "string", "const": "no-store"}}}});
    if body {
        response["content"] =
            json!({"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}});
    }
    response
}
