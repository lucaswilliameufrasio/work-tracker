//! Local protocol fake, NOT a production OAuth server or browser consent UI.
//! Maintained oauth2 client drives S256/resource requests against real SQLite.
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use openssl::{memcmp, sha::sha256};
use puxiri::db::Database;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const CLIENT: &str = "preregistered-public-fixture";
const REDIRECT: &str = "https://client.example.invalid/oauth/callback";
const SCOPE: &str = "mcp:read";
const BROWSER: &str = "fixture_browser=artificial-authenticated-user";
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
struct Exchange {
    client_id: String,
    redirect_uri: String,
    grant_type: String,
    code: String,
    resource: String,
    code_verifier: String,
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
    if request.client_id != CLIENT
        || request.redirect_uri != REDIRECT
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
    let consent: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT id FROM oauth_consent WHERE client=? AND resource=? AND scope=? AND active=1",
    )
    .bind(CLIENT)
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
        .bind(CLIENT)
        .bind(REDIRECT)
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
    let mut target = reqwest::Url::parse(REDIRECT).unwrap();
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
    let row:Result<Option<CodeRow>,_>=sqlx::query_as("SELECT c.client,c.redirect,c.resource,c.challenge,c.scope,c.expires,c.consent FROM oauth_codes c JOIN oauth_consent g ON g.id=c.consent WHERE c.digest=? AND g.active=1")
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
    let token = CsrfToken::new_random().secret().clone();
    if sqlx::query("DELETE FROM oauth_codes WHERE digest=?")
        .bind(digest)
        .execute(&mut *tx)
        .await
        .is_err()
        || sqlx::query("INSERT INTO oauth_tokens VALUES(?,?,?,?,?)")
            .bind(sha256(token.as_bytes()).to_vec())
            .bind(consent)
            .bind(resource)
            .bind(&scope)
            .bind(NOW + 300)
            .execute(&mut *tx)
            .await
            .is_err()
        || tx.commit().await.is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    (
        [("cache-control", "no-store"), ("pragma", "no-cache")],
        Json(json!({"access_token":token,"token_type":"Bearer","expires_in":300,"scope":scope})),
    )
        .into_response()
}
async fn probe(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    let values: Vec<_> = headers.get_all("authorization").iter().collect();
    if values.len() != 1 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(token) = values[0]
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let allowed:Result<i64,_>=sqlx::query_scalar("SELECT count(*) FROM oauth_tokens t JOIN oauth_consent c ON c.id=t.consent WHERE t.digest=? AND t.resource=? AND t.scope=? AND t.expires>? AND c.active=1")
        .bind(sha256(token.as_bytes()).to_vec()).bind(&server.resource).bind(SCOPE).bind(NOW).fetch_one(server.db.pool()).await;
    match allowed {
        Ok(1) => StatusCode::OK.into_response(),
        Ok(_) => StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
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
        sqlx::raw_sql("CREATE TABLE oauth_consent(id INTEGER PRIMARY KEY,client TEXT,resource TEXT,scope TEXT,active INTEGER); CREATE TABLE oauth_codes(digest BLOB PRIMARY KEY,client TEXT,redirect TEXT,resource TEXT,challenge TEXT,scope TEXT,expires INTEGER,consent INTEGER REFERENCES oauth_consent(id)); CREATE TABLE oauth_tokens(digest BLOB PRIMARY KEY,consent INTEGER REFERENCES oauth_consent(id),resource TEXT,scope TEXT,expires INTEGER);")
            .execute(db.pool()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = Arc::new(Server {
            db,
            resource: format!("{base}/mcp"),
            base,
        });
        let metadata = server.clone();
        let app=Router::new().route("/authorize",get(authorize)).route("/token",post(exchange)).route("/mcp",get(probe))
            .route("/.well-known/oauth-authorization-server",get(move ||{let s=metadata.clone();async move {Json(json!({"issuer":s.base,"authorization_endpoint":format!("{}/authorize",s.base),"token_endpoint":format!("{}/token",s.base),"response_types_supported":["code"],"grant_types_supported":["authorization_code"],"token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"],"scopes_supported":[SCOPE]}))}}))
            .layer(DefaultBodyLimit::max(8192)).with_state(server.clone());
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
        sqlx::query("INSERT INTO oauth_consent VALUES(1,?,?,?,1)")
            .bind(CLIENT)
            .bind(&self.server.resource)
            .bind(SCOPE)
            .execute(self.server.db.pool())
            .await
            .unwrap();
    }
    fn begin(&self) -> (reqwest::Url, CsrfToken, PkceCodeVerifier) {
        let client = BasicClient::new(ClientId::new(CLIENT.into()))
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
        #[derive(Debug, thiserror::Error)]
        #[error("transport failed")]
        struct Transport;
        let endpoint = format!("{}/token", self.server.base);
        let http = Self::http();
        let adapter = |request: oauth2::HttpRequest| {
            let endpoint = endpoint.clone();
            let http = http.clone();
            async move {
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
        };
        let client = BasicClient::new(ClientId::new(CLIENT.into()))
            .set_token_uri(TokenUrl::new(endpoint.clone()).unwrap())
            .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
        client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .add_extra_param("resource", self.server.resource.clone())
            .request_async(&adapter)
            .await
            .map(|r| r.access_token().secret().clone())
            .map_err(|_| ())
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
    assert_eq!(
        Fixture::http()
            .get(&fixture.server.resource)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
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
    assert_eq!(
        Fixture::http()
            .get(&fixture.server.resource)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
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
