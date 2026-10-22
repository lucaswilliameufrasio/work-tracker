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
            "Session identity, explicit current project read access and summaries share one SQLite snapshot. Missing/inaccessible project gives generic404. Archived projects are readable; trash excluded. No descriptions/history/rank. Issue numbers and versions are decimal JSON strings preserving positive i64 precision. No issue mutation/detail/search or MCP access is provided. Cross-request pagination is not a frozen snapshot.",
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
    let mut document = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Puxiri opt-in browser API — partial development contract",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Disabled (404) unless --browser-origin/PUXIRI_BROWSER_ORIGIN is configured. Listener does not terminate TLS; use an explicitly configured HTTPS origin. Cookie-only authentication; no Authorization fallback. Host must match; Origin and Fetch Metadata are checked when supplied, and mutations require exact Origin plus CSRF. Unknown/duplicate query fields are rejected. Every operation has no-store responses; HEAD has no body, including errors. Browser admission: eight in-flight requests per router instance, no admission queue; path+query <=2048 bytes, headers <=64 entries/16384 aggregate name+value bytes, no payload bytes. Empty unknown-length bodies must finish within one second before any handler/transaction starts. There is no whole-handler timeout or per-client requests-per-second limit. No login/signup/setup/session issuance, API tokens or OAuth are provided by this contract. Do not store bearer tokens in localStorage. This document is generated locally, not served as an endpoint or a production readiness claim."
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
                "SessionPage": page("Session"),
                "ProjectPage": page("Project"),
                "OperationalProjectPage": page("OperationalProject"),
                "Issue": object(json!({
                    "id": uuid.clone(), "project_id": uuid.clone(), "issue_number": decimal_i64(),
                    "issue_key": {"type": "string"}, "issue_type_id": uuid.clone(), "status_id": uuid.clone(),
                    "title": {"type": "string", "minLength": 1, "maxLength": 240},
                    "assignee_user_id": uuid.clone(), "creator_user_id": uuid.clone(),
                    "priority": {"type": "string", "enum": ["none", "low", "medium", "high", "urgent"]},
                    "due_date": {"type": "string", "format": "date"}, "version": decimal_i64(),
                    "created_at": timestamp.clone(), "updated_at": timestamp.clone()
                }), &["id", "project_id", "issue_number", "issue_key", "issue_type_id", "status_id", "title", "creator_user_id", "priority", "version", "created_at", "updated_at"]),
                "IssuePage": page("Issue"),
                "Error": object(json!({"code": {"type": "string"}, "message": {"type": "string"}, "request_id": {"type": "string", "format": "uuid"}, "field_errors": {"type": "object", "additionalProperties": {"type": "string"}}, "retryable": {"type": "boolean"}}), &["code", "message", "request_id", "field_errors", "retryable"])
            }
        }
    });
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
    detail["properties"]["trashed_at"] = timestamp;
    document["components"]["schemas"]["IssueDetail"] = detail;
    document["components"]["schemas"]["IssueHistoryEvent"] = object(
        json!({"id": uuid.clone(), "project_id": uuid.clone(), "issue_id": uuid.clone(), "actor_user_id": uuid, "event_kind": {"type": "string", "enum": ["issue.created"]}, "occurred_at": {"type": "string", "format": "date-time"}}),
        &[
            "id",
            "project_id",
            "issue_id",
            "actor_user_id",
            "event_kind",
            "occurred_at",
        ],
    );
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
    document["components"]["schemas"]["IssueTypePage"] = page("IssueType");
    document
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
            || parameter["name"] == "record_id"
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
