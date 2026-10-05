//! Development-only resource gate around the official Rust Streamable HTTP
//! service. Opaque grants are seeded, NOT issued by an OAuth authorization flow.
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use openssl::sha::sha256;
use puxiri::db::Database;
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
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const SCOPE: &str = "mcp:read";
const VERSION: &str = "2025-11-25";
#[derive(Clone)]
struct Diagnostic;
impl ServerHandler for Diagnostic {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![Tool::new(
                "diagnostic",
                "Artificial development-only diagnostic",
                Arc::new(
                    json!({"type":"object","properties":{},"additionalProperties":false})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if params.name != "diagnostic" {
            return Err(ErrorData::invalid_params("unknown diagnostic tool", None));
        }
        Ok(
            CallToolResult::success(vec![ContentBlock::text("fixture only; no project data")])
                .into(),
        )
    }
}
struct Gate {
    db: Arc<Database>,
    resource: String,
    metadata: String,
    origin: String,
}
type GrantRow = (String, String, i64, i64, i64);
fn challenge(gate: &Gate, status: StatusCode, error: Option<&str>) -> Response {
    let prefix = error
        .map(|e| format!("error=\"{e}\", "))
        .unwrap_or_default();
    (
        status,
        [
            (
                "www-authenticate",
                format!(
                    "Bearer {prefix}resource_metadata=\"{}\", scope=\"{SCOPE}\"",
                    gate.metadata
                ),
            ),
            ("cache-control", "no-store".into()),
        ],
    )
        .into_response()
}
async fn authorize(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    // Fixed configured origin/resource; never derive discovery from Host or
    // Forwarded headers. Query-string tokens are never accepted, even with auth.
    if request.uri().query().is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let origins: Vec<_> = request.headers().get_all("origin").iter().collect();
    if origins.len() > 1
        || origins
            .first()
            .is_some_and(|v| v.to_str().ok() != Some(gate.origin.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let headers: Vec<_> = request.headers().get_all("authorization").iter().collect();
    if headers.len() != 1 {
        return challenge(&gate, StatusCode::UNAUTHORIZED, None);
    }
    let Some(token) = headers[0]
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return challenge(&gate, StatusCode::UNAUTHORIZED, Some("invalid_token"));
    };
    if token.is_empty()
        || token.len() > 512
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return challenge(&gate, StatusCode::UNAUTHORIZED, Some("invalid_token"));
    }
    let row: Result<Option<GrantRow>, _> = sqlx::query_as(
        "SELECT resource,scope,expires,revoked,banned FROM mcp_grants WHERE digest=?",
    )
    .bind(sha256(token.as_bytes()).to_vec())
    .fetch_optional(gate.db.pool())
    .await;
    let (resource, scope, expires, revoked, banned) = match row {
        Ok(Some(row)) => row,
        Ok(None) => return challenge(&gate, StatusCode::UNAUTHORIZED, Some("invalid_token")),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if resource != gate.resource || expires <= NOW || revoked != 0 || banned != 0 {
        return challenge(&gate, StatusCode::UNAUTHORIZED, Some("invalid_token"));
    }
    if !scope.split_ascii_whitespace().any(|s| s == SCOPE) {
        return challenge(&gate, StatusCode::FORBIDDEN, Some("insufficient_scope"));
    }
    next.run(request).await
}
struct Fixture {
    directory: TempDir,
    gate: Arc<Gate>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::raw_sql("CREATE TABLE mcp_grants(digest BLOB PRIMARY KEY,resource TEXT NOT NULL,scope TEXT NOT NULL,expires INTEGER NOT NULL,revoked INTEGER NOT NULL,banned INTEGER NOT NULL)")
            .execute(db.pool()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let gate = Arc::new(Gate {
            db,
            resource: format!("{base}/mcp"),
            metadata: format!("{base}/.well-known/oauth-protected-resource/mcp"),
            origin: base.clone(),
        });
        let service: StreamableHttpService<Diagnostic, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(Diagnostic),
                Default::default(),
                StreamableHttpServerConfig::default()
                    .with_legacy_session_mode(false)
                    .with_json_response(true)
                    .with_sse_keep_alive(None),
            );
        let protected = Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(gate.clone(), authorize));
        let metadata = gate.clone();
        let app=Router::new().route("/.well-known/oauth-protected-resource/mcp",get(move ||{let gate=metadata.clone();async move {
            Json(json!({"resource":gate.resource,"authorization_servers":["https://auth.example.invalid"],"scopes_supported":[SCOPE],"bearer_methods_supported":["header"]}))
        }})).merge(protected);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            gate,
            task,
        }
    }
    async fn seed(&self, token: &str, resource: &str, scope: &str, validity: (i64, i64, i64)) {
        let (expires, revoked, banned) = validity;
        sqlx::query("INSERT INTO mcp_grants VALUES(?,?,?,?,?,?)")
            .bind(sha256(token.as_bytes()).to_vec())
            .bind(resource)
            .bind(scope)
            .bind(expires)
            .bind(revoked)
            .bind(banned)
            .execute(self.gate.db.pool())
            .await
            .unwrap();
    }
    async fn request(&self, token: Option<&str>, body: Value) -> reqwest::Response {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let mut request = client
            .post(&self.gate.resource)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .json(&body);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.send().await.unwrap()
    }
}
fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"fixture-not-a-product-client","version":"0"}}})
}

#[tokio::test]
async fn unauthenticated_challenge_and_public_metadata_do_not_trust_request_host_or_disclose_tokens()
 {
    let fixture = Fixture::new().await;
    let response = fixture.request(None, initialize()).await;
    assert_eq!(response.status(), 401);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers()["www-authenticate"],
        format!(
            "Bearer resource_metadata=\"{}\", scope=\"{SCOPE}\"",
            fixture.gate.metadata
        )
    );
    let metadata: Value = reqwest::Client::new()
        .get(&fixture.gate.metadata)
        .header("host", "attacker.example.invalid")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["resource"], fixture.gate.resource);
    assert_eq!(
        metadata["authorization_servers"],
        json!(["https://auth.example.invalid"])
    );
    assert_eq!(metadata["bearer_methods_supported"], json!(["header"]));
    assert_eq!(metadata["scopes_supported"], json!([SCOPE]));
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(&fixture.gate.resource)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .delete(&fixture.gate.resource)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn opaque_token_digest_and_revocation_survive_database_reopen_without_restoring_authority() {
    let mut fixture = Fixture::new().await;
    let token = "artificial-secret-not-stored-in-clear";
    fixture
        .seed(token, &fixture.gate.resource, SCOPE, (NOW + 300, 0, 0))
        .await;
    let digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM mcp_grants")
        .fetch_one(fixture.gate.db.pool())
        .await
        .unwrap();
    assert_eq!(digest, sha256(token.as_bytes()));
    assert_ne!(digest, token.as_bytes());
    sqlx::query("UPDATE mcp_grants SET revoked=1")
        .execute(fixture.gate.db.pool())
        .await
        .unwrap();
    fixture.task.abort();
    let _ = (&mut fixture.task).await;
    fixture.gate.db.close().await;
    let directory = std::mem::replace(&mut fixture.directory, TempDir::new().unwrap());
    drop(fixture);
    let reopened = Database::open(directory.path()).await.unwrap();
    let revoked: i64 = sqlx::query_scalar("SELECT revoked FROM mcp_grants WHERE digest=?")
        .bind(digest)
        .fetch_one(reopened.pool())
        .await
        .unwrap();
    assert_eq!(revoked, 1);
    reopened.close().await;
}

#[tokio::test]
async fn resource_expiry_revocation_ban_and_exact_scope_are_checked_on_each_request() {
    let fixture = Fixture::new().await;
    for (token, resource, scope, expires, revoked, banned, status) in [
        (
            "wrong-resource",
            "https://other.example.invalid/mcp",
            SCOPE,
            NOW + 300,
            0,
            0,
            401,
        ),
        (
            "expired",
            fixture.gate.resource.as_str(),
            SCOPE,
            NOW,
            0,
            0,
            401,
        ),
        (
            "revoked",
            fixture.gate.resource.as_str(),
            SCOPE,
            NOW + 300,
            1,
            0,
            401,
        ),
        (
            "banned",
            fixture.gate.resource.as_str(),
            SCOPE,
            NOW + 300,
            0,
            1,
            401,
        ),
        (
            "prefix-scope",
            fixture.gate.resource.as_str(),
            "mcp:read-all",
            NOW + 300,
            0,
            0,
            403,
        ),
    ] {
        fixture
            .seed(token, resource, scope, (expires, revoked, banned))
            .await;
        let response = fixture.request(Some(token), initialize()).await;
        assert_eq!(response.status(), status);
        assert!(
            !response.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .contains(token)
        );
        if status == 403 {
            assert!(
                response.headers()["www-authenticate"]
                    .to_str()
                    .unwrap()
                    .contains("insufficient_scope")
            );
        }
    }
    assert_eq!(
        fixture
            .request(Some("external-provider-access-token"), initialize())
            .await
            .status(),
        401
    );
}

#[tokio::test]
async fn maintained_streamable_http_service_initializes_lists_and_calls_diagnostic_then_revocation_blocks_reuse()
 {
    let fixture = Fixture::new().await;
    let token = "artificial-opaque-mcp-token";
    fixture
        .seed(token, &fixture.gate.resource, SCOPE, (NOW + 300, 0, 0))
        .await;
    let response = fixture.request(Some(token), initialize()).await;
    assert_eq!(response.status(), 200);
    let initialized: Value = response.json().await.unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], VERSION);
    let tools = fixture
        .request(
            Some(token),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        )
        .await;
    assert_eq!(tools.status(), 200);
    let tools: Value = tools.json().await.unwrap();
    assert_eq!(tools["result"]["tools"][0]["name"], "diagnostic");
    let call=fixture.request(Some(token),json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnostic","arguments":{}}})).await;
    assert_eq!(call.status(), 200);
    let call: Value = call.json().await.unwrap();
    assert_eq!(
        call["result"]["content"][0]["text"],
        "fixture only; no project data"
    );
    sqlx::query("UPDATE mcp_grants SET revoked=1")
        .execute(fixture.gate.db.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture.request(Some(token), initialize()).await.status(),
        401
    );
}

#[tokio::test]
async fn malformed_headers_query_tokens_untrusted_origin_and_database_failure_fail_closed() {
    let fixture = Fixture::new().await;
    fixture
        .seed("valid", &fixture.gate.resource, SCOPE, (NOW + 300, 0, 0))
        .await;
    let client = reqwest::Client::new();
    for headers in [
        vec![
            ("authorization", "Bearer valid"),
            ("authorization", "Bearer valid"),
        ],
        vec![("authorization", "Basic valid")],
        vec![
            ("authorization", "Bearer valid"),
            ("origin", "https://evil.example.invalid"),
        ],
    ] {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(name, value.parse().unwrap());
        }
        let response = client
            .post(&fixture.gate.resource)
            .headers(map)
            .json(&initialize())
            .send()
            .await
            .unwrap();
        assert!(matches!(response.status().as_u16(), 401 | 403));
    }
    let response = client
        .post(format!("{}?access_token=valid", fixture.gate.resource))
        .bearer_auth("valid")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    sqlx::query("DROP TABLE mcp_grants")
        .execute(fixture.gate.db.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture.request(Some("valid"), initialize()).await.status(),
        503
    );
    assert!(fixture.directory.path().exists());
}
