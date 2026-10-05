//! Integrated local-only attempt -> exchange -> verified identity. No login,
//! account linking, real provider, production endpoint or operational recovery.
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use openidconnect::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse, TokenUrl,
    core::{CoreAuthenticationFlow, CoreClient, CoreJsonWebKeySet, CoreJwsSigningAlgorithm},
};
use openssl::{
    hash::MessageDigest,
    pkey::{PKey, Private},
    rsa::Rsa,
    sha::sha256,
    sign::Signer,
};
use puxiri::db::Database;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const ISSUER: &str = "https://gitlab.com";
const CLIENT: &str = "fixture-client";
const REDIRECT: &str = "https://puxiri.example.invalid/callbacks/gitlab";
struct Endpoint {
    db: Arc<Database>,
    keys: CoreJsonWebKeySet,
    token_url: String,
}
struct Attempt {
    state: String,
    cookie: String,
    nonce: String,
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: String,
}
#[derive(Debug, thiserror::Error)]
#[error("exchange transport failed")]
struct Transport;
async fn begin(endpoint: &Endpoint) -> Attempt {
    let client = CoreClient::new(
        ClientId::new(CLIENT.into()),
        IssuerUrl::new(ISSUER.into()).unwrap(),
        endpoint.keys.clone(),
    )
    .set_auth_uri(AuthUrl::new("https://gitlab.com/oauth/authorize".into()).unwrap())
    .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (_, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_extra_param("response_mode", "form_post")
        .set_pkce_challenge(challenge)
        .url();
    let cookie = CsrfToken::new_random().secret().clone();
    sqlx::query("INSERT INTO flow_attempts(state,cookie,nonce,verifier,config,expires,status) VALUES(?,?,?,?,'gitlab-v1',?,'ready')")
        .bind(sha256(state.secret().as_bytes()).to_vec()).bind(sha256(cookie.as_bytes()).to_vec()).bind(nonce.secret()).bind(verifier.secret()).bind(NOW+300).execute(endpoint.db.pool()).await.unwrap();
    Attempt {
        state: state.secret().clone(),
        cookie,
        nonce: nonce.secret().clone(),
    }
}
async fn claim(
    endpoint: &Endpoint,
    state: &str,
    cookie: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let mut tx = endpoint.db.pool().begin_with("BEGIN IMMEDIATE").await?;
    let claimed=sqlx::query_as("UPDATE flow_attempts SET status='exchanging' WHERE state=? AND cookie=? AND config='gitlab-v1' AND expires>? AND status='ready' RETURNING nonce,verifier")
        .bind(sha256(state.as_bytes()).to_vec()).bind(sha256(cookie.as_bytes()).to_vec()).bind(NOW).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(claimed)
}
async fn callback(
    State(endpoint): State<Arc<Endpoint>>,
    headers: HeaderMap,
    form: Result<Form<Callback>, axum::extract::rejection::FormRejection>,
) -> StatusCode {
    let Ok(Form(form)) = form else {
        return StatusCode::BAD_REQUEST;
    };
    if form.state.is_empty()
        || form.state.len() > 256
        || form.code.is_empty()
        || form.code.len() > 2048
    {
        return StatusCode::BAD_REQUEST;
    }
    let mut cookies = Vec::new();
    for value in headers.get_all("cookie") {
        let Ok(value) = value.to_str() else {
            return StatusCode::UNAUTHORIZED;
        };
        for pair in value.split(';') {
            if let Some(("fixture_browser", value)) = pair.trim().split_once('=') {
                cookies.push(value);
            }
        }
    }
    if cookies.len() != 1 {
        return StatusCode::UNAUTHORIZED;
    }
    let claimed = claim(&endpoint, &form.state, cookies[0]).await;
    let (nonce, verifier) = match claimed {
        Ok(Some(values)) => values,
        Ok(None) => return StatusCode::UNAUTHORIZED,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    let result = exchange(&endpoint, form.code, nonce, verifier).await;
    let mut tx = match endpoint.db.pool().begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    let state = sha256(form.state.as_bytes()).to_vec();
    let (outcome, response) = match result {
        Ok(Some(subject)) => {
            if sqlx::query("INSERT INTO flow_identities(provider,issuer,subject) VALUES('gitlab',?,?) ON CONFLICT DO NOTHING")
                .bind(ISSUER).bind(subject).execute(&mut *tx).await.is_err() {return StatusCode::SERVICE_UNAVAILABLE;}
            ("verified", StatusCode::ACCEPTED)
        }
        Ok(None) => ("rejected", StatusCode::UNAUTHORIZED),
        Err(_) => ("unknown", StatusCode::SERVICE_UNAVAILABLE),
    };
    let updated=sqlx::query("UPDATE flow_attempts SET status=?,nonce=NULL,verifier=NULL WHERE state=? AND status='exchanging' AND config='gitlab-v1'")
        .bind(outcome).bind(state).execute(&mut *tx).await;
    if !updated.is_ok_and(|result| result.rows_affected() == 1) || tx.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    response
}
async fn exchange(
    endpoint: &Endpoint,
    code: String,
    nonce: String,
    verifier: String,
) -> Result<Option<String>, Transport> {
    let client = CoreClient::new(
        ClientId::new(CLIENT.into()),
        IssuerUrl::new(ISSUER.into()).unwrap(),
        endpoint.keys.clone(),
    )
    .set_client_secret(ClientSecret::new("artificial-secret".into()))
    .set_token_uri(TokenUrl::new(endpoint.token_url.clone()).unwrap())
    .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap());
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(100))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let adapter = |request: openidconnect::HttpRequest| {
        let http = http.clone();
        let token_url = endpoint.token_url.clone();
        async move {
            if request.uri().to_string() != token_url {
                return Err(Transport);
            }
            let (parts, body) = request.into_parts();
            let mut response = http
                .request(parts.method, &token_url)
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|_| Transport)?;
            let mut builder = openidconnect::http::Response::builder().status(response.status());
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
    let response = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&adapter)
        .await
        .map_err(|_| Transport)?;
    let Some(token) = response.id_token() else {
        return Ok(None);
    };
    let verifier = client
        .id_token_verifier()
        .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256])
        .set_time_fn(|| DateTime::<Utc>::from_timestamp(NOW, 0).unwrap())
        .set_issue_time_verifier_fn(|iat| {
            if (NOW - 300..=NOW).contains(&iat.timestamp()) {
                Ok(())
            } else {
                Err("invalid issue time".into())
            }
        });
    let Ok(claims) = token.claims(&verifier, &Nonce::new(nonce)) else {
        return Ok(None);
    };
    // CoreClient drops unknown additional claims. Read optional nbf only AFTER
    // library signature validation; this is not an alternate JWT verifier.
    let raw = token.to_string();
    let Some(payload) = raw
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
    else {
        return Ok(None);
    };
    let Ok(extra) = serde_json::from_slice::<Value>(&payload) else {
        return Ok(None);
    };
    if extra
        .get("nbf")
        .is_some_and(|value| value.as_i64().is_none_or(|nbf| nbf > NOW))
    {
        return Ok(None);
    }
    if claims.subject().as_str().is_empty()
        || claims.subject().as_str().len() > 256
        || claims
            .authorized_party()
            .is_some_and(|p| p.as_str() != CLIENT)
    {
        return Ok(None);
    }
    Ok(Some(claims.subject().as_str().into()))
}
struct Provider {
    reply: Mutex<(u16, Value, Duration)>,
    calls: AtomicUsize,
}
struct Fixture {
    directory: TempDir,
    endpoint: Arc<Endpoint>,
    provider: Arc<Provider>,
    key: PKey<Private>,
    base: String,
    provider_task: tokio::task::JoinHandle<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        self.provider_task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = Arc::new(Database::open(directory.path()).await.unwrap());
        sqlx::raw_sql("CREATE TABLE flow_attempts(state BLOB PRIMARY KEY,cookie BLOB NOT NULL,nonce TEXT,verifier TEXT,config TEXT NOT NULL,expires INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN ('ready','exchanging','verified','rejected','unknown')));
            CREATE TABLE flow_identities(provider TEXT,issuer TEXT,subject TEXT,PRIMARY KEY(provider,issuer,subject));")
            .execute(db.pool()).await.unwrap();
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let rsa = key.rsa().unwrap();
        let keys:CoreJsonWebKeySet=serde_json::from_value(json!({"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":"fixture","n":URL_SAFE_NO_PAD.encode(rsa.n().to_vec()),"e":URL_SAFE_NO_PAD.encode(rsa.e().to_vec())}]})).unwrap();
        let provider = Arc::new(Provider {
            reply: Mutex::new((200, json!({}), Duration::ZERO)),
            calls: AtomicUsize::new(0),
        });
        let source = provider.clone();
        let app = Router::new().route(
            "/token",
            post(move || {
                let source = source.clone();
                async move {
                    source.calls.fetch_add(1, Ordering::SeqCst);
                    let (status, body, delay) = source.reply.lock().unwrap().clone();
                    tokio::time::sleep(delay).await;
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body.to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let provider_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let endpoint = Arc::new(Endpoint {
            db,
            keys,
            token_url,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/callback", post(callback))
            .layer(DefaultBodyLimit::max(8192))
            .with_state(endpoint.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            endpoint,
            provider,
            key,
            base,
            provider_task,
            task,
        }
    }
    fn token(&self, nonce: &str) -> Value {
        json!({"iss":ISSUER,"aud":CLIENT,"sub":"stable-subject","exp":NOW+300,"iat":NOW,"nonce":nonce})
    }
    fn respond(&self, claims: Option<Value>) {
        let mut body = json!({"access_token":"artificial-access","token_type":"Bearer"});
        if let Some(claims) = claims {
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"fixture"}"#),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            );
            let mut signer = Signer::new(MessageDigest::sha256(), &self.key).unwrap();
            signer.update(input.as_bytes()).unwrap();
            body["id_token"] = json!(format!(
                "{input}.{}",
                URL_SAFE_NO_PAD.encode(signer.sign_to_vec().unwrap())
            ));
        }
        *self.provider.reply.lock().unwrap() = (200, body, Duration::ZERO);
    }
    async fn post(&self, attempt: &Attempt, cookie: &str) -> u16 {
        reqwest::Client::new()
            .post(format!("{}/callback", self.base))
            .header("cookie", format!("fixture_browser={cookie}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("state={}&code=artificial-code", attempt.state))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
    async fn status(&self, attempt: &Attempt) -> String {
        sqlx::query_scalar("SELECT status FROM flow_attempts WHERE state=?")
            .bind(sha256(attempt.state.as_bytes()).to_vec())
            .fetch_one(self.endpoint.db.pool())
            .await
            .unwrap()
    }
    async fn identities(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM flow_identities")
            .fetch_one(self.endpoint.db.pool())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn concurrent_callback_claims_once_then_exchange_validates_nonce_before_atomic_identity_registration()
 {
    let fixture = Fixture::new().await;
    let attempt = begin(&fixture.endpoint).await;
    fixture.respond(Some(fixture.token(&attempt.nonce)));
    let (a, b) = tokio::join!(
        fixture.post(&attempt, &attempt.cookie),
        fixture.post(&attempt, &attempt.cookie)
    );
    assert!(matches!((a, b), (202, 401) | (401, 202)));
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.identities().await, 1);
    assert_eq!(fixture.status(&attempt).await, "verified");
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM flow_attempts WHERE nonce IS NOT NULL OR verifier IS NOT NULL"
        )
        .fetch_one(fixture.endpoint.db.pool())
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn mismatched_browser_expired_or_inactive_attempt_has_no_token_request() {
    let fixture = Fixture::new().await;
    let attempt = begin(&fixture.endpoint).await;
    fixture.respond(Some(fixture.token(&attempt.nonce)));
    assert_eq!(fixture.post(&attempt, "another-browser").await, 401);
    sqlx::query("UPDATE flow_attempts SET expires=?")
        .bind(NOW)
        .execute(fixture.endpoint.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    sqlx::query("UPDATE flow_attempts SET expires=?,config='inactive'")
        .bind(NOW + 300)
        .execute(fixture.endpoint.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.identities().await, 0);
}

#[tokio::test]
async fn invalid_id_token_or_missing_id_token_never_registers_identity_and_cannot_replay_code() {
    let fixture = Fixture::new().await;
    for field in ["nonce", "iss", "aud", "exp", "nbf", "missing"] {
        let attempt = begin(&fixture.endpoint).await;
        let mut token = fixture.token(&attempt.nonce);
        match field {
            "iss" => token["iss"] = json!("https://wrong.example.invalid"),
            "exp" => token["exp"] = json!(NOW),
            "nbf" => token["nbf"] = json!(NOW + 1),
            "missing" => {}
            name => token[name] = json!("wrong"),
        }
        fixture.respond(if field == "missing" {
            None
        } else {
            Some(token)
        });
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
        assert_eq!(fixture.status(&attempt).await, "rejected");
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    }
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 6);
    assert_eq!(fixture.identities().await, 0);
}

#[tokio::test]
async fn ambiguous_exchange_and_failed_completion_remain_nonretryable_without_partial_identity() {
    let fixture = Fixture::new().await;
    let attempt = begin(&fixture.endpoint).await;
    *fixture.provider.reply.lock().unwrap() = (
        200,
        json!({"access_token":"artificial","token_type":"Bearer"}),
        Duration::from_millis(300),
    );
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 503);
    assert_eq!(fixture.status(&attempt).await, "unknown");
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    let second = begin(&fixture.endpoint).await;
    fixture.respond(Some(fixture.token(&second.nonce)));
    sqlx::raw_sql("CREATE TRIGGER fail_complete BEFORE UPDATE ON flow_attempts WHEN NEW.status='verified' BEGIN SELECT RAISE(ABORT,'fixture completion failure'); END")
        .execute(fixture.endpoint.db.pool()).await.unwrap();
    assert_eq!(fixture.post(&second, &second.cookie).await, 503);
    assert_eq!(fixture.status(&second).await, "exchanging");
    assert_eq!(fixture.identities().await, 0);
    sqlx::query("DROP TRIGGER fail_complete")
        .execute(fixture.endpoint.db.pool())
        .await
        .unwrap();
    assert_eq!(fixture.post(&second, &second.cookie).await, 401);
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn interrupted_after_claim_remains_consumed_after_reopen_without_reusing_authorization_code()
{
    let mut fixture = Fixture::new().await;
    let attempt = begin(&fixture.endpoint).await;
    assert!(
        claim(&fixture.endpoint, &attempt.state, &attempt.cookie)
            .await
            .unwrap()
            .is_some()
    );
    let keys = fixture.endpoint.keys.clone();
    let token_url = fixture.endpoint.token_url.clone();
    fixture.task.abort();
    let _ = (&mut fixture.task).await;
    fixture.endpoint.db.close().await;
    let directory = std::mem::replace(&mut fixture.directory, TempDir::new().unwrap());
    drop(fixture);
    let db = Arc::new(Database::open(directory.path()).await.unwrap());
    let reopened = Endpoint {
        db: db.clone(),
        keys,
        token_url,
    };
    assert!(
        claim(&reopened, &attempt.state, &attempt.cookie)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM flow_attempts")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "exchanging"
    );
    db.close().await;
}
