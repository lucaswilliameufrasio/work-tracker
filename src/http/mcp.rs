use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Map, Value, json};
use sqlx::Row;

use crate::mcp::{McpAccessError, McpCaller, McpRuntime, ToolFailure};

pub(super) fn routes(runtime: McpRuntime) -> Router {
    let runtime = Arc::new(runtime);
    let service: StreamableHttpService<WorkServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let runtime = Arc::clone(&runtime);
                move || {
                    Ok(WorkServer {
                        runtime: Arc::clone(&runtime),
                    })
                }
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None),
        );
    let metadata = Arc::clone(&runtime);
    let protected =
        Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                Arc::clone(&runtime),
                authorize,
            ));
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(move || {
                let runtime = Arc::clone(&metadata);
                async move { metadata_document(&runtime).await }
            }),
        )
        .merge(protected)
}

async fn metadata_document(runtime: &McpRuntime) -> Response {
    let issuer = configured_issuer(&runtime.pool).await;
    let mut servers = Vec::new();
    if let Some(issuer) = issuer {
        servers.push(issuer);
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "resource": format!("{}/mcp", runtime.origin.trim_end_matches('/')),
            "authorization_servers": servers,
            "scopes_supported": ["mcp:read", "mcp:write"],
            "bearer_methods_supported": ["header"],
        })),
    )
        .into_response()
}

async fn configured_issuer(pool: &sqlx::SqlitePool) -> Option<String> {
    let row =
        sqlx::query("SELECT source, muira_issuer FROM installation_login WHERE singleton = 1")
            .fetch_optional(pool)
            .await
            .ok()??;
    let source: String = row.try_get("source").ok()?;
    let issuer: Option<String> = row.try_get("muira_issuer").ok()?;
    if source == "muira" {
        issuer.filter(|value| !value.is_empty())
    } else {
        None
    }
}

async fn authorize(
    State(runtime): State<Arc<McpRuntime>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().query().is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let headers = request.headers();
    let presented: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
    if presented.len() != 1 {
        return challenge(&runtime, StatusCode::UNAUTHORIZED, None);
    }
    let Some(token) = presented[0]
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
    else {
        return challenge(&runtime, StatusCode::UNAUTHORIZED, Some("invalid_token"));
    };
    match crate::mcp::authenticate_bearer(&runtime.pool, token).await {
        Ok(caller) => {
            let mut request = request;
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(McpAccessError::MalformedToken) | Err(McpAccessError::IdentityRejected) => {
            challenge(&runtime, StatusCode::UNAUTHORIZED, Some("invalid_token"))
        }
        Err(McpAccessError::LoginUnavailable) => {
            challenge(&runtime, StatusCode::UNAUTHORIZED, Some("login_required"))
        }
        Err(McpAccessError::InvitationRequired) => {
            challenge(&runtime, StatusCode::FORBIDDEN, Some("access_denied"))
        }
        Err(McpAccessError::GrantRequired) => {
            challenge(&runtime, StatusCode::FORBIDDEN, Some("insufficient_scope"))
        }
        Err(McpAccessError::Upstream) | Err(McpAccessError::Database) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn challenge(runtime: &McpRuntime, status: StatusCode, error: Option<&str>) -> Response {
    let prefix = error
        .map(|value| format!("error=\"{value}\", "))
        .unwrap_or_default();
    let metadata = format!(
        "{}/.well-known/oauth-protected-resource/mcp",
        runtime.origin.trim_end_matches('/')
    );
    (
        status,
        [
            (
                header::WWW_AUTHENTICATE,
                format!(
                    "Bearer {prefix}resource_metadata=\"{metadata}\", scope=\"mcp:read mcp:write\""
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
    )
        .into_response()
}

struct WorkServer {
    runtime: Arc<McpRuntime>,
}

impl ServerHandler for WorkServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let _caller = caller(&context)?;
        let mut tools = Vec::new();
        for name in crate::mcp::tool_names() {
            tools.push(Tool::new(*name, tool_description(name), schema(name)));
        }
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = caller(&context)?;
        let arguments = request.arguments.clone().unwrap_or_default();
        let value = crate::mcp::call_tool(
            &self.runtime,
            &caller.user_id,
            request.name.as_ref(),
            &arguments,
        )
        .await
        .map_err(tool_error)?;
        let text = serde_json::to_string(&value)
            .map_err(|_| ErrorData::internal_error("the tool result could not be encoded", None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

fn caller(context: &RequestContext<rmcp::RoleServer>) -> Result<McpCaller, ErrorData> {
    let parts = context
        .extensions
        .get::<Parts>()
        .ok_or_else(|| ErrorData::invalid_request("authentication is required", None))?;
    parts
        .extensions
        .get::<McpCaller>()
        .cloned()
        .ok_or_else(|| ErrorData::invalid_request("authentication is required", None))
}

fn tool_error(failure: ToolFailure) -> ErrorData {
    match failure {
        ToolFailure::Invalid(message) => ErrorData::invalid_params(message, None),
        ToolFailure::Forbidden => {
            ErrorData::invalid_request("a Puxiri grant and project role are required", None)
        }
        ToolFailure::Unavailable => {
            ErrorData::resource_not_found("the work item is not available", None)
        }
        ToolFailure::Conflict(message) => ErrorData::invalid_request(message, None),
        ToolFailure::UnavailableService => {
            ErrorData::internal_error("the work item could not be loaded", None)
        }
    }
}

fn tool_description(name: &str) -> &'static str {
    match name {
        "list_projects" => "Projects this person may read under the Puxiri grant",
        "get_project_schema" => "Visible statuses and issue types for one granted project",
        "search_issues" => "Search issues the person can read in one granted project",
        "get_issue" => "Read one issue the person can see",
        "create_issue" => {
            "Create an issue when the grant is write and the project role can change work"
        }
        "update_issue" => "Replace an issue title and description",
        "move_issue" => "Move an issue to another visible status",
        "add_comment" => "Add a comment when the project role can comment",
        _ => "Puxiri work tool",
    }
}

fn schema(name: &str) -> Arc<Map<String, Value>> {
    let value = match name {
        "list_projects" => json!({
            "type": "object",
            "properties": {"after_project_key": {"type": "string"}},
            "additionalProperties": false
        }),
        "get_project_schema" => json!({
            "type": "object",
            "properties": {"project_id": {"type": "string"}},
            "required": ["project_id"],
            "additionalProperties": false
        }),
        "search_issues" => json!({
            "type": "object",
            "properties": {"project_id": {"type": "string"}, "query": {"type": "string"}},
            "required": ["project_id", "query"],
            "additionalProperties": false
        }),
        "get_issue" | "create_issue" | "update_issue" | "move_issue" | "add_comment" => {
            object_schema(name)
        }
        _ => json!({"type": "object", "additionalProperties": false}),
    };
    match value {
        Value::Object(object) => Arc::new(object),
        _ => Arc::new(Map::new()),
    }
}

fn object_schema(name: &str) -> Value {
    match name {
        "get_issue" => json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "issue_number": {"type": "integer", "minimum": 1}
            },
            "required": ["project_id", "issue_number"],
            "additionalProperties": false
        }),
        "create_issue" => json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "title": {"type": "string"},
                "description": {"type": "string"}
            },
            "required": ["project_id", "title"],
            "additionalProperties": false
        }),
        "update_issue" => json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "issue_number": {"type": "integer", "minimum": 1},
                "expected_version": {"type": "integer", "minimum": 1},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "idempotency_key": {"type": "string"}
            },
            "required": ["project_id", "issue_number", "expected_version", "title", "idempotency_key"],
            "additionalProperties": false
        }),
        "move_issue" => json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "issue_number": {"type": "integer", "minimum": 1},
                "expected_version": {"type": "integer", "minimum": 1},
                "status_id": {"type": "string"},
                "idempotency_key": {"type": "string"}
            },
            "required": ["project_id", "issue_number", "expected_version", "status_id", "idempotency_key"],
            "additionalProperties": false
        }),
        _ => json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"},
                "issue_number": {"type": "integer", "minimum": 1},
                "body": {"type": "string"},
                "idempotency_key": {"type": "string"}
            },
            "required": ["project_id", "issue_number", "body", "idempotency_key"],
            "additionalProperties": false
        }),
    }
}
