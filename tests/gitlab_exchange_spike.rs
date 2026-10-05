//! Local-only OAuth wire contract, not a product login or real GitLab grant.
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenUrl, basic::BasicClient,
};
use openssl::{memcmp, sha::sha256};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const CLIENT: &str = "fixture-client";
const SECRET: &str = "artificial-client-secret";
const REDIRECT: &str = "https://puxiri.example.invalid/callbacks/gitlab";
struct Attempt {
    state: CsrfToken,
    verifier: PkceCodeVerifier,
    nonce: openidconnect::Nonce,
    url: reqwest::Url,
}
struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}
struct Call {
    headers: HeaderMap,
    body: String,
}
struct Server {
    reply: Mutex<Reply>,
    calls: Mutex<Vec<Call>>,
}
struct Fixture {
    base: String,
    server: Arc<Server>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Debug, PartialEq, thiserror::Error)]
enum Failure {
    #[error("state mismatch")]
    State,
    #[error("transport failed")]
    Transport,
    #[error("exchange failed")]
    Exchange,
}
impl Fixture {
    async fn new() -> Self {
        let server=Arc::new(Server {reply:Mutex::new(Reply {status:200,body:json!({"access_token":"artificial-access","token_type":"Bearer","expires_in":7200,"id_token":"not-verified-here"}).to_string(),delay:Duration::ZERO}),calls:Mutex::new(Vec::new())});
        let app = Router::new()
            .route("/oauth/token", post(receive))
            .with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, server, task }
    }
    fn begin(&self) -> Attempt {
        let client = BasicClient::new(ClientId::new(CLIENT.into()))
            .set_auth_uri(AuthUrl::new("https://gitlab.com/oauth/authorize".into()).unwrap())
            .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let nonce = openidconnect::Nonce::new_random();
        let (url, state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".into()))
            .add_scope(Scope::new("email".into()))
            .add_extra_param("nonce", nonce.secret().clone())
            .set_pkce_challenge(challenge)
            .url();
        Attempt {
            state,
            verifier,
            nonce,
            url,
        }
    }
    async fn exchange(&self, attempt: Attempt, returned_state: &str) -> Result<(), Failure> {
        let expected = attempt.state.secret().as_bytes();
        if expected.len() != returned_state.len()
            || !memcmp::eq(expected, returned_state.as_bytes())
        {
            return Err(Failure::State);
        }
        let endpoint = format!("{}/oauth/token", self.base);
        let client = BasicClient::new(ClientId::new(CLIENT.into()))
            .set_client_secret(ClientSecret::new(SECRET.into()))
            .set_token_uri(TokenUrl::new(endpoint.clone()).unwrap())
            .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        // Thin transport adapter for oauth2 5 + repository's reqwest 0.13. The
        // maintained library generates request/auth/form and parses response.
        let adapter = |request: HttpRequest| {
            let http = &http;
            let endpoint = &endpoint;
            async move {
                if request.uri().to_string() != *endpoint {
                    return Err(Failure::Transport);
                }
                let (parts, body) = request.into_parts();
                let mut response = http
                    .request(parts.method, endpoint)
                    .headers(parts.headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(|_| Failure::Transport)?;
                let mut builder = oauth2::http::Response::builder().status(response.status());
                *builder.headers_mut().unwrap() = response.headers().clone();
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Transport)? {
                    if bytes.len() + chunk.len() > 65536 {
                        return Err(Failure::Transport);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                builder.body(bytes).map_err(|_| Failure::Transport)
            }
        };
        let response = client
            .exchange_code(AuthorizationCode::new("artificial-code".into()))
            .set_pkce_verifier(attempt.verifier)
            .request_async(&adapter)
            .await
            .map_err(|_| Failure::Exchange)?;
        drop(response); // Access token is NOT identity; no session or linking.
        Ok(())
    }
    fn reply(&self, status: u16, body: Value, delay: Duration) {
        *self.server.reply.lock().unwrap() = Reply {
            status,
            body: body.to_string(),
            delay,
        };
    }
}
async fn receive(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    server.calls.lock().unwrap().push(Call {
        headers,
        body: String::from_utf8(body.to_vec()).unwrap(),
    });
    let (status, body, delay) = {
        let reply = server.reply.lock().unwrap();
        (reply.status, reply.body.clone(), reply.delay)
    };
    tokio::time::sleep(delay).await;
    (
        StatusCode::from_u16(status).unwrap(),
        [
            ("content-type", "application/json"),
            ("location", "http://127.0.0.1:1/never-follow"),
        ],
        body,
    )
}
fn fields(body: &str) -> std::collections::BTreeMap<String, String> {
    let url = reqwest::Url::parse(&format!("https://example.invalid/?{body}")).unwrap();
    url.query_pairs().into_owned().collect()
}

#[tokio::test]
async fn library_generates_s256_state_scopes_and_exact_redirect_and_sends_matching_verifier_with_basic_auth()
 {
    let fixture = Fixture::new().await;
    let attempt = fixture.begin();
    let authorization = fields(attempt.url.query().unwrap());
    assert_eq!(
        attempt.url.origin().ascii_serialization(),
        "https://gitlab.com"
    );
    assert_eq!(authorization["response_type"], "code");
    assert_eq!(authorization["client_id"], CLIENT);
    assert_eq!(authorization["redirect_uri"], REDIRECT);
    assert_eq!(authorization["scope"], "openid email");
    assert_eq!(authorization["code_challenge_method"], "S256");
    assert_eq!(authorization["state"], *attempt.state.secret());
    assert_eq!(authorization["nonce"], *attempt.nonce.secret());
    assert_eq!(
        authorization["code_challenge"],
        URL_SAFE_NO_PAD.encode(sha256(attempt.verifier.secret().as_bytes()))
    );
    assert!(!attempt.url.as_str().contains(SECRET));
    assert!(!attempt.url.as_str().contains(attempt.verifier.secret()));
    let verifier = attempt.verifier.secret().clone();
    let state = attempt.state.secret().clone();
    assert_eq!(fixture.exchange(attempt, &state).await, Ok(()));
    let calls = fixture.server.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(
        calls[0].headers["authorization"],
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT}:{SECRET}"))
        )
    );
    let request = fields(&calls[0].body);
    assert_eq!(request["grant_type"], "authorization_code");
    assert_eq!(request["code"], "artificial-code");
    assert_eq!(request["redirect_uri"], REDIRECT);
    assert_eq!(request["code_verifier"], verifier);
    assert!(!request.contains_key("client_secret"));
}

#[tokio::test]
async fn wrong_state_never_reaches_token_endpoint_and_attempts_use_distinct_random_values() {
    let fixture = Fixture::new().await;
    let first = fixture.begin();
    let second = fixture.begin();
    assert_ne!(first.state.secret(), second.state.secret());
    assert_ne!(first.verifier.secret(), second.verifier.secret());
    assert_eq!(
        fixture.exchange(first, second.state.secret()).await,
        Err(Failure::State)
    );
    assert!(fixture.server.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_grant_redirect_server_error_and_malformed_success_do_not_retry_or_disclose_credentials()
 {
    let fixture = Fixture::new().await;
    for (index, (status, body)) in [
        (
            400,
            json!({"error":"invalid_grant","error_description":SECRET}),
        ),
        (302, json!({})),
        (503, json!({"error":"temporarily_unavailable"})),
        (200, json!({"token_type":"Bearer"})),
        (
            200,
            json!({"access_token":"artificial","token_type":"Bearer","expires_in":"wrong"}),
        ),
        (200, json!({"padding":"x".repeat(65536)})),
    ]
    .into_iter()
    .enumerate()
    {
        fixture.reply(status, body, Duration::ZERO);
        let attempt = fixture.begin();
        let state = attempt.state.secret().clone();
        let error = fixture.exchange(attempt, &state).await.unwrap_err();
        assert_eq!(error, Failure::Exchange);
        assert_eq!(format!("{error:?}"), "Exchange");
        assert_eq!(fixture.server.calls.lock().unwrap().len(), index + 1);
    }
}

#[tokio::test]
async fn timeout_after_receiving_code_is_ambiguous_and_does_not_trigger_hidden_exchange_retry() {
    let fixture = Fixture::new().await;
    fixture.reply(
        200,
        json!({"access_token":"artificial","token_type":"Bearer"}),
        Duration::from_millis(300),
    );
    let attempt = fixture.begin();
    let state = attempt.state.secret().clone();
    assert_eq!(
        fixture.exchange(attempt, &state).await,
        Err(Failure::Exchange)
    );
    assert_eq!(fixture.server.calls.lock().unwrap().len(), 1);
}
