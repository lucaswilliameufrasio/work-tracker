//! Local protocol fake, NOT a production OAuth server or browser consent UI.
//! Maintained oauth2 client drives S256/resource requests against real SQLite.
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use openssl::{memcmp, sha::sha256};
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
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const CLIENT: &str = "preregistered-public-fixture";
const REDIRECT: &str = "https://client.example.invalid/oauth/callback";
const SCOPE: &str = "mcp:read";
const BROWSER: &str = "fixture_browser=artificial-authenticated-user";
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
                "OAuth-connected local fixture",
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
        request: CallToolRequestParams,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name != "diagnostic" {
            return Err(ErrorData::invalid_params("unknown diagnostic tool", None));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "issued token accepted; fixture only",
        )])
        .into())
    }
}
struct Server {
    db: Arc<Database>,
    base: String,
    resource: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorize {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    scope: String,
    state: String,
    resource: String,
    code_challenge: String,
    code_challenge_method: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
}
async fn register(
    State(server): State<Arc<Server>>,
    body: Result<Json<Registration>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    };
    // Explicit exact allowlist for this fixture; never fetch submitted URLs.
    if request.client_name.trim().is_empty()
        || request.client_name.len() > 120
        || request.client_name.chars().any(char::is_control)
        || request.redirect_uris != [REDIRECT]
        || request.grant_types != ["authorization_code", "refresh_token"]
        || request.response_types != ["code"]
        || request.token_endpoint_auth_method != "none"
    {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    let mut tx = match server.db.pool().begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let policy: Result<(i64, i64), _> =
        sqlx::query_as("SELECT enabled,max_clients FROM oauth_registration_policy")
            .fetch_one(&mut *tx)
            .await;
    let (enabled, limit) = match policy {
        Ok(policy) => policy,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    if enabled != 1 {
        return error(StatusCode::FORBIDDEN, "registration_disabled");
    }
    let count: Result<i64, _> =
        sqlx::query_scalar("SELECT count(*) FROM oauth_clients WHERE dynamic=1")
            .fetch_one(&mut *tx)
            .await;
    match count {
        Ok(count) if count < limit => {}
        Ok(_) => return error(StatusCode::FORBIDDEN, "registration_limit"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let id = CsrfToken::new_random().secret().clone();
    if sqlx::query("INSERT INTO oauth_clients VALUES(?,?,?,1,1)")
        .bind(&id)
        .bind(REDIRECT)
        .bind(&request.client_name)
        .execute(&mut *tx)
        .await
        .is_err()
        || tx.commit().await.is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    (StatusCode::CREATED,[("cache-control","no-store")],Json(json!({"client_id":id,"client_id_issued_at":NOW,"client_name":request.client_name,"redirect_uris":request.redirect_uris,"grant_types":request.grant_types,"response_types":request.response_types,"token_endpoint_auth_method":"none"}))).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    client_id: String,
    redirect_uri: String,
    grant_type: String,
    code: String,
    resource: String,
    code_verifier: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    client_id: String,
    resource: String,
    grant_type: String,
    redirect_uri: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    scope: Option<String>,
}
async fn token_endpoint(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Form(request): Form<TokenRequest>,
) -> Response {
    if headers.contains_key("authorization") {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    match request.grant_type.as_str() {
        "authorization_code" if request.refresh_token.is_none() && request.scope.is_none() => {
            let (Some(redirect_uri), Some(code), Some(code_verifier)) =
                (request.redirect_uri, request.code, request.code_verifier)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            exchange(
                State(server),
                headers,
                Form(Exchange {
                    client_id: request.client_id,
                    redirect_uri,
                    code,
                    code_verifier,
                    grant_type: request.grant_type,
                    resource: request.resource,
                }),
            )
            .await
        }
        "refresh_token"
            if request.redirect_uri.is_none()
                && request.code.is_none()
                && request.code_verifier.is_none() =>
        {
            let Some(refresh) = request.refresh_token else {
                return error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            rotate(
                server,
                request.client_id,
                request.resource,
                refresh,
                request.scope,
            )
            .await
        }
        _ => error(StatusCode::BAD_REQUEST, "invalid_request"),
    }
}
struct Credentials {
    access: String,
    refresh: String,
}
#[derive(Debug, thiserror::Error)]
#[error("transport failed")]
struct Transport;
async fn transport(
    request: oauth2::HttpRequest,
    endpoint: String,
    http: reqwest::Client,
) -> Result<oauth2::HttpResponse, Transport> {
    if request.uri().to_string() != endpoint {
        return Err(Transport);
    }
    let (parts, body) = request.into_parts();
    let mut response = http
        .request(parts.method, &endpoint)
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .map_err(|_| Transport)?;
    let mut builder = oauth2::http::Response::builder().status(response.status());
    *builder.headers_mut().unwrap() = response.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Transport)? {
        if bytes.len() + chunk.len() > 65536 {
            return Err(Transport);
        }
        bytes.extend_from_slice(&chunk);
    }
    builder.body(bytes).map_err(|_| Transport)
}
fn token_response(credentials: Credentials, scope: String, expires: i64) -> Response {
    ([("cache-control","no-store"),("pragma","no-cache")],Json(json!({"access_token":credentials.access,"refresh_token":credentials.refresh,"token_type":"Bearer","expires_in":expires-NOW,"scope":scope}))).into_response()
}
async fn insert_pair(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    consent: i64,
    family: &str,
    client: &str,
    resource: &str,
    scope: &str,
    family_expires: i64,
) -> Result<Credentials, sqlx::Error> {
    let credentials = Credentials {
        access: CsrfToken::new_random().secret().clone(),
        refresh: CsrfToken::new_random().secret().clone(),
    };
    sqlx::query("INSERT INTO oauth_tokens VALUES(?,?,?,?,?,?)")
        .bind(sha256(credentials.access.as_bytes()).to_vec())
        .bind(consent)
        .bind(resource)
        .bind(scope)
        .bind((NOW + 300).min(family_expires))
        .bind(family)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO oauth_refresh VALUES(?,?,?,?,?,?,0)")
        .bind(sha256(credentials.refresh.as_bytes()).to_vec())
        .bind(family)
        .bind(client)
        .bind(resource)
        .bind(scope)
        .bind(family_expires)
        .execute(&mut **tx)
        .await?;
    Ok(credentials)
}
#[derive(sqlx::FromRow)]
struct RefreshRow {
    family: String,
    client: String,
    resource: String,
    scope: String,
    expires: i64,
    used: i64,
    consent: i64,
    revoked: i64,
    family_expires: i64,
    active: i64,
    consent_client: String,
    consent_resource: String,
    consent_scope: String,
}
async fn rotate(
    server: Arc<Server>,
    client: String,
    resource: String,
    refresh: String,
    scope: Option<String>,
) -> Response {
    if refresh.is_empty()
        || refresh.len() > 512
        || !refresh
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let mut tx = match server.db.pool().begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let digest = sha256(refresh.as_bytes()).to_vec();
    let row:Result<Option<RefreshRow>,_>=sqlx::query_as("SELECT r.family,r.client,r.resource,r.scope,r.expires,r.used,f.consent,f.revoked,f.expires AS family_expires,c.active,c.client AS consent_client,c.resource AS consent_resource,c.scope AS consent_scope FROM oauth_refresh r JOIN oauth_families f ON f.id=r.family JOIN oauth_consent c ON c.id=f.consent JOIN oauth_clients p ON p.id=r.client AND p.active=1 WHERE r.digest=? AND f.client=r.client")
        .bind(&digest).fetch_optional(&mut *tx).await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    // Wrong client/resource cannot use a stolen token to revoke its family.
    if row.client != client
        || row.resource != resource
        || resource != server.resource
        || row.consent_client != client
        || row.consent_resource != resource
        || row.active != 1
        || row.revoked != 0
        || row.expires <= NOW
        || row.family_expires <= NOW
        || row.consent_scope != row.scope
    {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    if row.used != 0 {
        if sqlx::query("UPDATE oauth_families SET revoked=1 WHERE id=?")
            .bind(&row.family)
            .execute(&mut *tx)
            .await
            .is_err()
            || tx.commit().await.is_err()
        {
            return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
        }
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    if scope.as_ref().is_some_and(|scope| scope != &row.scope) {
        return error(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    if sqlx::query("UPDATE oauth_refresh SET used=1 WHERE digest=? AND used=0")
        .bind(digest)
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let credentials = match insert_pair(
        &mut tx,
        row.consent,
        &row.family,
        &row.client,
        &row.resource,
        &row.scope,
        row.family_expires,
    )
    .await
    {
        Ok(credentials) => credentials,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    if tx.commit().await.is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    token_response(credentials, row.scope, (NOW + 300).min(row.family_expires))
}
fn error(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Json(json!({"error":code})),
    )
        .into_response()
}
async fn authorize(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Query(request): Query<Authorize>,
) -> Response {
    let cookies: Vec<_> = headers.get_all("cookie").iter().collect();
    if cookies.len() != 1 || cookies[0].to_str().ok() != Some(BROWSER) {
        return error(StatusCode::UNAUTHORIZED, "login_required");
    }
    if request.client_id.is_empty()
        || request.client_id.len() > 256
        || request.response_type != "code"
        || request.scope != SCOPE
        || request.resource != server.resource
        || request.state.is_empty()
        || request.state.len() > 256
        || request.code_challenge_method != "S256"
        || !URL_SAFE_NO_PAD
            .decode(&request.code_challenge)
            .is_ok_and(|bytes| bytes.len() == 32)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let mut tx = match server.db.pool().begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let registered: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT redirect FROM oauth_clients WHERE id=? AND active=1")
            .bind(&request.client_id)
            .fetch_optional(&mut *tx)
            .await;
    match registered {
        Ok(Some(redirect)) if redirect == request.redirect_uri => {}
        Ok(_) => return error(StatusCode::BAD_REQUEST, "invalid_client"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let consent: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT id FROM oauth_consent WHERE client=? AND resource=? AND scope=? AND active=1",
    )
    .bind(&request.client_id)
    .bind(&server.resource)
    .bind(SCOPE)
    .fetch_optional(&mut *tx)
    .await;
    let id = match consent {
        Ok(Some(id)) => id,
        Ok(None) => return error(StatusCode::FORBIDDEN, "consent_required"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let code = CsrfToken::new_random().secret().clone();
    if sqlx::query("INSERT INTO oauth_codes VALUES(?,?,?,?,?,?,?,?)")
        .bind(sha256(code.as_bytes()).to_vec())
        .bind(&request.client_id)
        .bind(&request.redirect_uri)
        .bind(&server.resource)
        .bind(request.code_challenge)
        .bind(SCOPE)
        .bind(NOW + 120)
        .bind(id)
        .execute(&mut *tx)
        .await
        .is_err()
        || tx.commit().await.is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let mut target = reqwest::Url::parse(&request.redirect_uri).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &request.state);
    (
        StatusCode::FOUND,
        [
            ("location", target.to_string()),
            ("cache-control", "no-store".into()),
        ],
    )
        .into_response()
}
type CodeRow = (String, String, String, String, String, i64, i64);
async fn exchange(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Form(request): Form<Exchange>,
) -> Response {
    if headers.contains_key("authorization")
        || request.grant_type != "authorization_code"
        || request.code.len() > 256
        || !(43..=128).contains(&request.code_verifier.len())
        || !request
            .code_verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let mut tx = match server.db.pool().begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let digest = sha256(request.code.as_bytes()).to_vec();
    let row:Result<Option<CodeRow>,_>=sqlx::query_as("SELECT c.client,c.redirect,c.resource,c.challenge,c.scope,c.expires,c.consent FROM oauth_codes c JOIN oauth_consent g ON g.id=c.consent JOIN oauth_clients p ON p.id=c.client AND p.active=1 AND p.redirect=c.redirect WHERE c.digest=? AND g.active=1 AND g.client=c.client AND g.resource=c.resource AND g.scope=c.scope")
        .bind(&digest).fetch_optional(&mut *tx).await;
    let (client, redirect, resource, challenge, scope, expires, consent) = match row {
        Ok(Some(row)) => row,
        Ok(None) => return error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    let expected = URL_SAFE_NO_PAD.encode(sha256(request.code_verifier.as_bytes()));
    if client != request.client_id
        || redirect != request.redirect_uri
        || resource != request.resource
        || resource != server.resource
        || expires <= NOW
        || expected.len() != challenge.len()
        || !memcmp::eq(expected.as_bytes(), challenge.as_bytes())
    {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let family = CsrfToken::new_random().secret().clone();
    if sqlx::query("DELETE FROM oauth_codes WHERE digest=?")
        .bind(digest)
        .execute(&mut *tx)
        .await
        .is_err()
        || sqlx::query("INSERT INTO oauth_families VALUES(?,?,?,0,?)")
            .bind(&family)
            .bind(consent)
            .bind(NOW + 3600)
            .bind(&client)
            .execute(&mut *tx)
            .await
            .is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let credentials = match insert_pair(
        &mut tx,
        consent,
        &family,
        &client,
        &resource,
        &scope,
        NOW + 3600,
    )
    .await
    {
        Ok(credentials) => credentials,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    };
    if tx.commit().await.is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    token_response(credentials, scope, NOW + 300)
}
fn challenge(server: &Server, status: StatusCode, error: &str) -> Response {
    (status,[("www-authenticate",format!("Bearer error=\"{error}\", resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"{SCOPE}\"",server.base)),("cache-control","no-store".into())]).into_response()
}
type TokenRow = (String, String, i64, i64, String, String, String, i64, i64);
async fn token_gate(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    if request.uri().query().is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let headers = request.headers();
    let origins: Vec<_> = headers.get_all("origin").iter().collect();
    if origins.len() > 1
        || origins
            .first()
            .is_some_and(|value| value.to_str().ok() != Some(server.base.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let values: Vec<_> = headers.get_all("authorization").iter().collect();
    if values.len() != 1 {
        return challenge(&server, StatusCode::UNAUTHORIZED, "invalid_token");
    }
    let Some(token) = values[0]
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return challenge(&server, StatusCode::UNAUTHORIZED, "invalid_token");
    };
    if token.is_empty()
        || token.len() > 512
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return challenge(&server, StatusCode::UNAUTHORIZED, "invalid_token");
    }
    let row:Result<Option<TokenRow>,_>=sqlx::query_as("SELECT t.resource,t.scope,t.expires,c.active,c.resource,c.scope,c.client,f.revoked,f.expires FROM oauth_tokens t JOIN oauth_consent c ON c.id=t.consent JOIN oauth_families f ON f.id=t.family JOIN oauth_clients p ON p.id=c.client AND p.active=1 WHERE t.digest=? AND f.client=c.client")
        .bind(sha256(token.as_bytes()).to_vec()).fetch_optional(server.db.pool()).await;
    let (
        resource,
        scope,
        expires,
        active,
        consent_resource,
        consent_scope,
        consent_client,
        family_revoked,
        family_expires,
    ) = match row {
        Ok(Some(row)) => row,
        Ok(None) => return challenge(&server, StatusCode::UNAUTHORIZED, "invalid_token"),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if resource != server.resource
        || consent_resource != resource
        || consent_client.is_empty()
        || expires <= NOW
        || active != 1
        || family_revoked != 0
        || family_expires <= NOW
    {
        return challenge(&server, StatusCode::UNAUTHORIZED, "invalid_token");
    }
    if !scope.split_ascii_whitespace().any(|s| s == SCOPE)
        || !consent_scope.split_ascii_whitespace().any(|s| s == SCOPE)
    {
        return challenge(&server, StatusCode::FORBIDDEN, "insufficient_scope");
    }
    next.run(request).await
}
struct Fixture {
    _directory: TempDir,
    server: Arc<Server>,
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
        sqlx::raw_sql("CREATE TABLE oauth_consent(id INTEGER PRIMARY KEY,client TEXT,resource TEXT,scope TEXT,active INTEGER); CREATE TABLE oauth_codes(digest BLOB PRIMARY KEY,client TEXT,redirect TEXT,resource TEXT,challenge TEXT,scope TEXT,expires INTEGER,consent INTEGER REFERENCES oauth_consent(id)); CREATE TABLE oauth_families(id TEXT PRIMARY KEY,consent INTEGER REFERENCES oauth_consent(id),expires INTEGER,revoked INTEGER,client TEXT NOT NULL); CREATE TABLE oauth_tokens(digest BLOB PRIMARY KEY,consent INTEGER REFERENCES oauth_consent(id),resource TEXT,scope TEXT,expires INTEGER,family TEXT REFERENCES oauth_families(id)); CREATE TABLE oauth_refresh(digest BLOB PRIMARY KEY,family TEXT REFERENCES oauth_families(id),client TEXT,resource TEXT,scope TEXT,expires INTEGER,used INTEGER);")
            .execute(db.pool()).await.unwrap();
        sqlx::raw_sql("CREATE TABLE oauth_clients(id TEXT PRIMARY KEY,redirect TEXT NOT NULL,name TEXT NOT NULL,active INTEGER NOT NULL,dynamic INTEGER NOT NULL); CREATE TABLE oauth_registration_policy(enabled INTEGER NOT NULL,max_clients INTEGER NOT NULL); INSERT INTO oauth_registration_policy VALUES(0,2)").execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO oauth_clients VALUES(?,?,'Preregistered fixture',1,0)")
            .bind(CLIENT)
            .bind(REDIRECT)
            .execute(db.pool())
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = Arc::new(Server {
            db,
            resource: format!("{base}/mcp"),
            base,
        });
        let metadata = server.clone();
        let resource_metadata = server.clone();
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
            .layer(middleware::from_fn_with_state(server.clone(), token_gate));
        let app=Router::new().route("/authorize",get(authorize)).route("/token",post(token_endpoint)).route("/register",post(register))
            .route("/.well-known/oauth-protected-resource/mcp",get(move ||{let s=resource_metadata.clone();async move {Json(json!({"resource":s.resource,"authorization_servers":[s.base],"scopes_supported":[SCOPE],"bearer_methods_supported":["header"]}))}}))
            .route("/.well-known/oauth-authorization-server",get(move ||{let s=metadata.clone();async move {
                let mut document=json!({"issuer":s.base,"authorization_endpoint":format!("{}/authorize",s.base),"token_endpoint":format!("{}/token",s.base),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"],"scopes_supported":[SCOPE]});
                if sqlx::query_scalar::<_,i64>("SELECT enabled FROM oauth_registration_policy").fetch_one(s.db.pool()).await.ok()==Some(1) {document["registration_endpoint"]=json!(format!("{}/register",s.base));}Json(document)
            }}))
            .with_state(server.clone()).merge(protected).layer(DefaultBodyLimit::max(8192));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _directory: directory,
            server,
            task,
        }
    }
    async fn consent(&self) {
        self.consent_for(CLIENT).await;
    }
    async fn consent_for(&self, client: &str) {
        sqlx::query("INSERT INTO oauth_consent VALUES(NULL,?,?,?,1)")
            .bind(client)
            .bind(&self.server.resource)
            .bind(SCOPE)
            .execute(self.server.db.pool())
            .await
            .unwrap();
    }
    fn begin(&self) -> (reqwest::Url, CsrfToken, PkceCodeVerifier) {
        self.begin_for(CLIENT)
    }
    fn begin_for(&self, id: &str) -> (reqwest::Url, CsrfToken, PkceCodeVerifier) {
        let client = BasicClient::new(ClientId::new(id.into()))
            .set_auth_uri(AuthUrl::new(format!("{}/authorize", self.server.base)).unwrap())
            .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(SCOPE.into()))
            .add_extra_param("resource", self.server.resource.clone())
            .set_pkce_challenge(challenge)
            .url();
        (url, state, verifier)
    }
    fn http() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
    }
    async fn code(&self, url: reqwest::Url, state: CsrfToken) -> String {
        let response = Self::http()
            .get(url)
            .header("cookie", BROWSER)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 302);
        let redirect =
            reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        let fields: std::collections::BTreeMap<_, _> =
            redirect.query_pairs().into_owned().collect();
        assert_eq!(fields["state"], *state.secret());
        fields["code"].clone()
    }
    async fn redeem(&self, code: String, verifier: PkceCodeVerifier) -> Result<String, ()> {
        self.redeem_pair(code, verifier)
            .await
            .map(|credentials| credentials.access)
    }
    async fn redeem_pair(
        &self,
        code: String,
        verifier: PkceCodeVerifier,
    ) -> Result<Credentials, ()> {
        self.redeem_for(CLIENT, code, verifier).await
    }
    async fn redeem_for(
        &self,
        id: &str,
        code: String,
        verifier: PkceCodeVerifier,
    ) -> Result<Credentials, ()> {
        let endpoint = format!("{}/token", self.server.base);
        let http = Self::http();
        let adapter =
            |request: oauth2::HttpRequest| transport(request, endpoint.clone(), http.clone());
        let client = BasicClient::new(ClientId::new(id.into()))
            .set_token_uri(TokenUrl::new(endpoint.clone()).unwrap())
            .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
        client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .add_extra_param("resource", self.server.resource.clone())
            .request_async(&adapter)
            .await
            .map_err(|_| ())
            .and_then(|r| {
                r.refresh_token()
                    .map(|refresh| Credentials {
                        access: r.access_token().secret().clone(),
                        refresh: refresh.secret().clone(),
                    })
                    .ok_or(())
            })
    }
    async fn refresh(&self, raw: &str) -> Result<Credentials, ()> {
        self.refresh_for(CLIENT, raw).await
    }
    async fn refresh_for(&self, id: &str, raw: &str) -> Result<Credentials, ()> {
        let endpoint = format!("{}/token", self.server.base);
        let http = Self::http();
        let adapter = |request| transport(request, endpoint.clone(), http.clone());
        let client = BasicClient::new(ClientId::new(id.into()))
            .set_token_uri(TokenUrl::new(endpoint.clone()).unwrap());
        client
            .exchange_refresh_token(&RefreshToken::new(raw.into()))
            .add_extra_param("resource", self.server.resource.clone())
            .request_async(&adapter)
            .await
            .map_err(|_| ())
            .and_then(|r| {
                r.refresh_token()
                    .map(|refresh| Credentials {
                        access: r.access_token().secret().clone(),
                        refresh: refresh.secret().clone(),
                    })
                    .ok_or(())
            })
    }
    async fn raw(&self, fields: Vec<(&str, String)>) -> reqwest::Response {
        let mut encoded = reqwest::Url::parse("https://fixture.invalid").unwrap();
        encoded.query_pairs_mut().extend_pairs(fields);
        Self::http()
            .post(format!("{}/token", self.server.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(encoded.query().unwrap().to_string())
            .send()
            .await
            .unwrap()
    }
    async fn mcp(&self, token: Option<&str>, body: Value) -> reqwest::Response {
        let mut request = Self::http()
            .post(&self.server.resource)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }
}
fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"oauth-fixture-not-a-product-client","version":"0"}}})
}
fn registration() -> Value {
    json!({"client_name":"Untrusted fixture name","redirect_uris":[REDIRECT],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"})
}
async fn register_request(fixture: &Fixture, body: Value) -> reqwest::Response {
    Fixture::http()
        .post(format!("{}/register", fixture.server.base))
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn enable_registration(fixture: &Fixture) {
    sqlx::query("UPDATE oauth_registration_policy SET enabled=1")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn controlled_registration_never_grants_consent_and_disabled_client_blocks_code_refresh_and_mcp()
 {
    let fixture = Fixture::new().await;
    assert_eq!(
        register_request(&fixture, registration()).await.status(),
        403
    );
    enable_registration(&fixture).await;
    let metadata: Value = Fixture::http()
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            fixture.server.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        metadata["registration_endpoint"],
        format!("{}/register", fixture.server.base)
    );
    assert!(
        metadata
            .get("client_id_metadata_document_supported")
            .is_none()
    );
    let response = register_request(&fixture, registration()).await;
    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let document: Value = response.json().await.unwrap();
    let id = document["client_id"].as_str().unwrap();
    assert_ne!(id, CLIENT);
    assert!(document.get("client_secret").is_none());
    assert!(document.get("access_token").is_none());
    assert_eq!(document["redirect_uris"], json!([REDIRECT]));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_consent")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    let (url, _, _) = fixture.begin_for(id);
    assert_eq!(
        Fixture::http()
            .get(url)
            .header("cookie", BROWSER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    fixture.consent_for(id).await;
    let (url, state, verifier) = fixture.begin_for(id);
    let code = fixture.code(url, state).await;
    let pair = fixture.redeem_for(id, code, verifier).await.unwrap();
    let rotated = fixture.refresh_for(id, &pair.refresh).await.unwrap();
    assert_eq!(
        fixture
            .mcp(Some(&rotated.access), initialize())
            .await
            .status(),
        200
    );
    sqlx::query("UPDATE oauth_consent SET client=? WHERE client=?")
        .bind(CLIENT)
        .bind(id)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .mcp(Some(&rotated.access), initialize())
            .await
            .status(),
        401
    );
    sqlx::query("UPDATE oauth_consent SET client=? WHERE client=?")
        .bind(id)
        .bind(CLIENT)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_clients SET active=0 WHERE id=?")
        .bind(id)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .mcp(Some(&rotated.access), initialize())
            .await
            .status(),
        401
    );
    assert!(fixture.refresh_for(id, &rotated.refresh).await.is_err());
    let (url, _, _) = fixture.begin_for(id);
    assert_eq!(
        Fixture::http()
            .get(url)
            .header("cookie", BROWSER)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn registration_rejects_unapproved_redirects_extra_metadata_and_unsupported_credentials_without_fetching_urls()
 {
    let fixture = Fixture::new().await;
    enable_registration(&fixture).await;
    for (field, value) in [
        (
            "redirect_uris",
            json!(["https://evil.example.invalid/callback"]),
        ),
        ("redirect_uris", json!(["http://127.0.0.1:9/callback"])),
        ("redirect_uris", json!(["javascript:alert(1)"])),
        ("redirect_uris", json!([REDIRECT, REDIRECT])),
        ("grant_types", json!(["client_credentials"])),
        ("response_types", json!(["token"])),
        ("token_endpoint_auth_method", json!("client_secret_basic")),
        ("client_name", json!("")),
        ("client_uri", json!("http://169.254.169.254/")),
        ("client_id", json!(CLIENT)),
    ] {
        let mut body = registration();
        body[field] = value;
        assert_eq!(
            register_request(&fixture, body).await.status(),
            400,
            "accepted {field}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_clients WHERE dynamic=1")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn registration_quota_is_atomic_under_concurrency_and_turning_policy_off_blocks_new_registrations()
 {
    let fixture = Fixture::new().await;
    enable_registration(&fixture).await;
    sqlx::query("UPDATE oauth_registration_policy SET max_clients=1")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        register_request(&fixture, registration()),
        register_request(&fixture, registration())
    );
    assert!(matches!(
        (a.status().as_u16(), b.status().as_u16()),
        (201, 403) | (403, 201)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_clients WHERE dynamic=1")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        1
    );
    sqlx::query("UPDATE oauth_registration_policy SET enabled=0")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        register_request(&fixture, registration()).await.status(),
        403
    );
    let metadata: Value = Fixture::http()
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            fixture.server.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(metadata.get("registration_endpoint").is_none());
}

#[tokio::test]
async fn registration_failure_has_no_partial_client_and_unavailable_registry_fails_closed() {
    let fixture = Fixture::new().await;
    enable_registration(&fixture).await;
    sqlx::raw_sql("CREATE TRIGGER fail_registration BEFORE INSERT ON oauth_clients BEGIN SELECT RAISE(ABORT,'fixture registration failure'); END").execute(fixture.server.db.pool()).await.unwrap();
    assert_eq!(
        register_request(&fixture, registration()).await.status(),
        503
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_clients WHERE dynamic=1")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER fail_registration")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        register_request(&fixture, registration()).await.status(),
        201
    );
    sqlx::query("DROP TABLE oauth_clients")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        register_request(&fixture, registration()).await.status(),
        503
    );
}

#[tokio::test]
async fn maintained_refresh_client_rotates_digests_and_reuse_revokes_entire_family_persistently() {
    let mut fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let original = fixture.redeem_pair(code, verifier).await.unwrap();
    let first = fixture.refresh(&original.refresh).await.unwrap();
    let second = fixture.refresh(&first.refresh).await.unwrap();
    assert_ne!(original.refresh, first.refresh);
    assert_ne!(first.refresh, second.refresh);
    assert_ne!(first.access, second.access);
    assert_eq!(
        fixture
            .mcp(Some(&second.access), initialize())
            .await
            .status(),
        200
    );
    let expiry: i64 = sqlx::query_scalar("SELECT expires FROM oauth_families")
        .fetch_one(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(expiry, NOW + 3600);
    let digests: Vec<Vec<u8>> = sqlx::query_scalar("SELECT digest FROM oauth_refresh")
        .fetch_all(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(digests.len(), 3);
    assert!(digests.contains(&sha256(second.refresh.as_bytes()).to_vec()));
    assert!(!digests.contains(&second.refresh.as_bytes().to_vec()));
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let unrelated = fixture.redeem_pair(code, verifier).await.unwrap();
    assert!(fixture.refresh(&original.refresh).await.is_err()); // Reuse is not an ordinary refresh retry.
    for access in [&original.access, &first.access, &second.access] {
        assert_eq!(fixture.mcp(Some(access), initialize()).await.status(), 401);
    }
    assert!(fixture.refresh(&second.refresh).await.is_err());
    assert_eq!(
        fixture
            .mcp(Some(&unrelated.access), initialize())
            .await
            .status(),
        200
    ); // Reuse fences this family, not unrelated grants.
    fixture.task.abort();
    let _ = (&mut fixture.task).await;
    fixture.server.db.close().await;
    let directory = std::mem::replace(&mut fixture._directory, TempDir::new().unwrap());
    drop(fixture);
    let db = Database::open(directory.path()).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT revoked FROM oauth_families WHERE id=(SELECT family FROM oauth_refresh WHERE digest=?)").bind(sha256(original.refresh.as_bytes()).to_vec()).fetch_one(db.pool()).await.unwrap(),1);
    db.close().await;
}

#[tokio::test]
async fn concurrent_refresh_has_one_rotation_but_reuse_detection_invalidates_even_the_winners_credentials()
 {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let original = fixture.redeem_pair(code, verifier).await.unwrap();
    let (a, b) = tokio::join!(
        fixture.refresh(&original.refresh),
        fixture.refresh(&original.refresh)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let winner = a.or(b).unwrap();
    assert_eq!(
        fixture
            .mcp(Some(&winner.access), initialize())
            .await
            .status(),
        401
    );
    assert!(fixture.refresh(&winner.refresh).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_refresh")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn wrong_refresh_bindings_or_scope_cannot_consume_or_revoke_and_consent_or_expiry_cannot_be_bypassed()
 {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let original = fixture.redeem_pair(code, verifier).await.unwrap();
    let fields = [
        ("grant_type", "refresh_token".to_string()),
        ("client_id", CLIENT.into()),
        ("resource", fixture.server.resource.clone()),
        ("refresh_token", original.refresh.clone()),
    ];
    for (key, value) in [
        ("client_id", "other-client"),
        ("resource", "https://other.example.invalid/mcp"),
    ] {
        let changed = fields
            .iter()
            .map(|(k, v)| (*k, if *k == key { value.into() } else { v.clone() }))
            .collect();
        assert_eq!(fixture.raw(changed).await.status(), 400);
    }
    let mut elevated = fields.to_vec();
    elevated.push(("scope", "mcp:write".into()));
    let response = fixture.raw(elevated).await;
    assert_eq!(response.status(), 400);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"], "invalid_scope");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT used FROM oauth_refresh")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revoked FROM oauth_families")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    let rotated = fixture.refresh(&original.refresh).await.unwrap();
    assert_eq!(
        fixture
            .mcp(Some(&rotated.access), initialize())
            .await
            .status(),
        200
    );
    sqlx::query("UPDATE oauth_consent SET active=0")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert!(fixture.refresh(&rotated.refresh).await.is_err());
    assert_eq!(
        fixture
            .mcp(Some(&rotated.access), initialize())
            .await
            .status(),
        401
    );
    for expired_table in ["oauth_refresh", "oauth_families"] {
        let fixture = Fixture::new().await;
        fixture.consent().await;
        let (url, state, verifier) = fixture.begin();
        let code = fixture.code(url, state).await;
        let original = fixture.redeem_pair(code, verifier).await.unwrap();
        let sql = if expired_table == "oauth_refresh" {
            "UPDATE oauth_refresh SET expires=?"
        } else {
            "UPDATE oauth_families SET expires=?"
        };
        sqlx::query(sql)
            .bind(NOW)
            .execute(fixture.server.db.pool())
            .await
            .unwrap();
        assert!(fixture.refresh(&original.refresh).await.is_err());
        if expired_table == "oauth_families" {
            assert_eq!(
                fixture
                    .mcp(Some(&original.access), initialize())
                    .await
                    .status(),
                401
            );
        }
    }
}

#[tokio::test]
async fn failed_successor_insert_rolls_back_used_flag_and_access_insert_without_hidden_retry() {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let original = fixture.redeem_pair(code, verifier).await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_successor BEFORE INSERT ON oauth_refresh BEGIN SELECT RAISE(ABORT,'fixture successor failure'); END").execute(fixture.server.db.pool()).await.unwrap();
    assert!(fixture.refresh(&original.refresh).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_refresh")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT used FROM oauth_refresh")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revoked FROM oauth_families")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
    // Harness observes actual rollback. A real client with ambiguous timeout
    // cannot infer rollback and must NOT blindly retry the old refresh token.
    sqlx::query("DROP TRIGGER fail_successor")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert!(fixture.refresh(&original.refresh).await.is_ok());
}

#[tokio::test]
async fn discovery_to_code_exchange_to_official_mcp_tool_call_uses_one_issued_token_until_consent_revocation()
 {
    let fixture = Fixture::new().await;
    let unauthorized = fixture.mcp(None, initialize()).await;
    assert_eq!(unauthorized.status(), 401);
    let challenge = unauthorized.headers()["www-authenticate"].to_str().unwrap();
    let metadata_url = format!(
        "{}/.well-known/oauth-protected-resource/mcp",
        fixture.server.base
    );
    assert!(challenge.contains(&format!("resource_metadata=\"{metadata_url}\"")));
    let resource: Value = Fixture::http()
        .get(metadata_url)
        .header("host", "evil.example.invalid")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resource["resource"], fixture.server.resource);
    assert_eq!(
        resource["authorization_servers"],
        json!([fixture.server.base])
    );
    let issuer = resource["authorization_servers"][0].as_str().unwrap();
    let metadata: Value = Fixture::http()
        .get(format!("{issuer}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["issuer"], issuer);
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    assert_eq!(
        format!("{}{}", url.origin().ascii_serialization(), url.path()),
        metadata["authorization_endpoint"].as_str().unwrap()
    );
    let code = fixture.code(url, state).await;
    let token = fixture.redeem(code, verifier).await.unwrap();
    let initialized = fixture.mcp(Some(&token), initialize()).await;
    assert_eq!(initialized.status(), 200);
    let initialized: Value = initialized.json().await.unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], VERSION);
    let listed = fixture
        .mcp(
            Some(&token),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        )
        .await;
    assert_eq!(listed.status(), 200);
    let listed: Value = listed.json().await.unwrap();
    assert_eq!(listed["result"]["tools"][0]["name"], "diagnostic");
    let operation = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnostic","arguments":{}}});
    let called = fixture.mcp(Some(&token), operation.clone()).await;
    assert_eq!(called.status(), 200);
    let called: Value = called.json().await.unwrap();
    assert_eq!(
        called["result"]["content"][0]["text"],
        "issued token accepted; fixture only"
    );
    sqlx::query("UPDATE oauth_consent SET active=0")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    let denied = fixture.mcp(Some(&token), operation).await;
    assert_eq!(denied.status(), 401);
    assert!(
        !denied.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains(&token)
    );
    assert_eq!(fixture.mcp(None, initialize()).await.status(), 401);
}

#[tokio::test]
async fn issued_token_cannot_gain_other_resource_scope_or_lifetime_and_database_failure_denies_mcp()
{
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let token = fixture.redeem(code, verifier).await.unwrap();
    sqlx::query("UPDATE oauth_consent SET scope='mcp:write'")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 403);
    sqlx::query("UPDATE oauth_consent SET scope=?")
        .bind(SCOPE)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_consent SET client='another-client'")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 401);
    sqlx::query("UPDATE oauth_consent SET client=?,resource='https://other.example.invalid/mcp'")
        .bind(CLIENT)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 401);
    sqlx::query("UPDATE oauth_consent SET resource=?")
        .bind(&fixture.server.resource)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_tokens SET resource='https://other.example.invalid/mcp'")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 401);
    sqlx::query("UPDATE oauth_tokens SET resource=?,scope='mcp:read-all'")
        .bind(&fixture.server.resource)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    let denied = fixture.mcp(Some(&token), initialize()).await;
    assert_eq!(denied.status(), 403);
    assert!(
        denied.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("insufficient_scope")
    );
    sqlx::query("UPDATE oauth_tokens SET scope=?,expires=?")
        .bind(SCOPE)
        .bind(NOW)
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 401);
    sqlx::query("DROP TABLE oauth_tokens")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 503);
}

#[tokio::test]
async fn issued_token_is_required_on_each_http_method_and_query_duplicate_auth_or_untrusted_origin_never_bypasses_gate()
 {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let token = fixture.redeem(code, verifier).await.unwrap();
    assert_eq!(
        Fixture::http()
            .get(&fixture.server.resource)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        Fixture::http()
            .delete(&fixture.server.resource)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let operation = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"diagnostic","arguments":{}}});
    assert_eq!(fixture.mcp(None, operation.clone()).await.status(), 401);
    assert_eq!(
        fixture
            .mcp(
                Some("external-oidc-provider-access-token"),
                operation.clone()
            )
            .await
            .status(),
        401
    );
    let denied = Fixture::http()
        .post(format!("{}?access_token={token}", fixture.server.resource))
        .bearer_auth(&token)
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 400);
    let denied = Fixture::http()
        .post(&fixture.server.resource)
        .bearer_auth(&token)
        .header("origin", "https://evil.example.invalid")
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let mut headers = HeaderMap::new();
    headers.append("authorization", format!("Bearer {token}").parse().unwrap());
    headers.append("authorization", format!("Bearer {token}").parse().unwrap());
    let denied = Fixture::http()
        .post(&fixture.server.resource)
        .headers(headers)
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
}

#[tokio::test]
async fn maintained_public_client_uses_s256_resource_and_exact_redirect_with_persisted_consent() {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let metadata: Value = Fixture::http()
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            fixture.server.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["issuer"], fixture.server.base);
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        metadata["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
    assert!(metadata.get("registration_endpoint").is_none());
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let token = fixture.redeem(code, verifier).await.unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 200);
    let stored: (Vec<u8>, String, String) =
        sqlx::query_as("SELECT digest,resource,scope FROM oauth_tokens")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap();
    assert_eq!(
        stored,
        (
            sha256(token.as_bytes()).to_vec(),
            fixture.server.resource.clone(),
            SCOPE.into()
        )
    );
    sqlx::query("UPDATE oauth_consent SET active=0")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.mcp(Some(&token), initialize()).await.status(), 401);
}

#[tokio::test]
async fn authorization_requires_authenticated_browser_consent_registered_client_redirect_resource_scope_and_s256()
 {
    let fixture = Fixture::new().await;
    let (url, _, _) = fixture.begin();
    assert_eq!(
        Fixture::http()
            .get(url.clone())
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        Fixture::http()
            .get(url.clone())
            .header("cookie", BROWSER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    fixture.consent().await;
    for (field, value) in [
        ("client_id", "unknown"),
        ("redirect_uri", "https://evil.example.invalid"),
        ("resource", "https://other.example.invalid/mcp"),
        ("scope", "mcp:write"),
        ("code_challenge_method", "plain"),
    ] {
        let fields: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    if k == field {
                        value.to_string()
                    } else {
                        v.to_string()
                    },
                )
            })
            .collect();
        let mut changed = url.clone();
        changed.set_query(None);
        changed.query_pairs_mut().extend_pairs(fields);
        let response = Fixture::http()
            .get(changed)
            .header("cookie", BROWSER)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(!response.headers().contains_key("location"));
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_codes")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn wrong_code_bindings_fail_without_consuming_then_concurrent_correct_exchange_has_one_winner()
 {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    let fields = [
        ("grant_type", "authorization_code".into()),
        ("client_id", CLIENT.into()),
        ("redirect_uri", REDIRECT.into()),
        ("resource", fixture.server.resource.clone()),
        ("code", code.clone()),
        ("code_verifier", verifier.secret().clone()),
    ];
    for (field, value) in [
        ("client_id", "unknown"),
        ("redirect_uri", "https://evil.example.invalid"),
        ("resource", "https://other.example.invalid/mcp"),
        (
            "code_verifier",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
    ] {
        let changed = fields
            .iter()
            .map(|(k, v)| (*k, if *k == field { value.into() } else { v.clone() }))
            .collect();
        let response = fixture.raw(changed).await;
        assert_eq!(response.status(), 400);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"], "invalid_grant");
    }
    let second = PkceCodeVerifier::new(verifier.secret().clone());
    let (a, b) = tokio::join!(
        fixture.redeem(code.clone(), verifier),
        fixture.redeem(code, second)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn token_insert_failure_rolls_back_code_consumption_and_revoked_consent_or_expired_code_cannot_issue()
 {
    let fixture = Fixture::new().await;
    fixture.consent().await;
    let (url, state, verifier) = fixture.begin();
    let code = fixture.code(url, state).await;
    sqlx::raw_sql("CREATE TRIGGER fail_token BEFORE INSERT ON oauth_tokens BEGIN SELECT RAISE(ABORT,'fixture token failure'); END").execute(fixture.server.db.pool()).await.unwrap();
    assert!(
        fixture
            .redeem(
                code.clone(),
                PkceCodeVerifier::new(verifier.secret().clone())
            )
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_codes")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        1
    );
    sqlx::query("DROP TRIGGER fail_token")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_consent SET active=0")
        .execute(fixture.server.db.pool())
        .await
        .unwrap();
    assert!(
        fixture
            .redeem(
                code.clone(),
                PkceCodeVerifier::new(verifier.secret().clone())
            )
            .await
            .is_err()
    );
    // Never reactivate revoked consent or resurrect its codes/tokens.
    let expired = Fixture::new().await;
    expired.consent().await;
    let (url, state, verifier) = expired.begin();
    let code = expired.code(url, state).await;
    sqlx::query("UPDATE oauth_codes SET expires=?")
        .bind(NOW)
        .execute(expired.server.db.pool())
        .await
        .unwrap();
    assert!(expired.redeem(code, verifier).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens")
            .fetch_one(fixture.server.db.pool())
            .await
            .unwrap(),
        0
    );
}
