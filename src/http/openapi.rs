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
        page_size,
        json!({"name": "cursor", "in": "query", "schema": uuid.clone(), "description": "Exclusive canonical lowercase hyphenated UUID boundary, descending session-record ID order. Need not identify an existing record and never grants access."}),
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
    json!({
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
                "Error": object(json!({"code": {"type": "string"}, "message": {"type": "string"}, "request_id": {"type": "string", "format": "uuid"}, "field_errors": {"type": "object", "additionalProperties": {"type": "string"}}, "retryable": {"type": "boolean"}}), &["code", "message", "request_id", "field_errors", "retryable"])
            }
        }
    })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
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
            "not_found: browser API opt-in disabled, or individual target absent/foreign/already revoked",
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
    if parameters
        .iter()
        .any(|parameter| parameter["name"] == "page_size" || parameter["name"] == "session_id")
    {
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
