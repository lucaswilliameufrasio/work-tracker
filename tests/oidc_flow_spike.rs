//! Integrated local-only attempt -> exchange -> verified identity. No login,
//! account linking, real provider, production endpoint or operational recovery.
use axum::{
    Form, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use openidconnect::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
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
#[derive(Clone, Copy)]
enum Profile {
    Gitlab,
    Apple,
}
impl Profile {
    fn issuer(self) -> &'static str {
        match self {
            Self::Gitlab => ISSUER,
            Self::Apple => "https://appleid.apple.com",
        }
    }
    fn client(self) -> &'static str {
        match self {
            Self::Gitlab => CLIENT,
            Self::Apple => "com.example.puxiri.web",
        }
    }
    fn revision(self) -> &'static str {
        match self {
            Self::Gitlab => "gitlab-v1",
            Self::Apple => "apple-v1",
        }
    }
    fn provider(self) -> &'static str {
        match self {
            Self::Gitlab => "gitlab",
            Self::Apple => "apple",
        }
    }
    fn redirect(self) -> &'static str {
        match self {
            Self::Gitlab => REDIRECT,
            Self::Apple => "https://puxiri.example.invalid/callbacks/apple",
        }
    }
    fn authorize(self) -> &'static str {
        match self {
            Self::Gitlab => "https://gitlab.com/oauth/authorize",
            Self::Apple => "https://appleid.apple.com/auth/authorize",
        }
    }
}
fn artificial_apple_secret() -> String {
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    // Public artificial key material. No product key loading or cache.
    let key = SigningKey::from_slice(&[42; 32]).unwrap();
    let input=format!("{}.{}",URL_SAFE_NO_PAD.encode(json!({"alg":"ES256","kid":"KEY1234567"}).to_string()),
        URL_SAFE_NO_PAD.encode(json!({"iss":"TEAM123456","sub":Profile::Apple.client(),"aud":Profile::Apple.issuer(),"iat":NOW,"exp":NOW+3600}).to_string()));
    let signature: Signature = key.sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}
struct Endpoint {
    db: Arc<Database>,
    keys: CoreJsonWebKeySet,
    token_url: String,
    profile: Profile,
    secret: String,
}
struct Attempt {
    state: String,
    cookie: String,
    nonce: String,
    url: reqwest::Url,
}
#[tokio::test]
async fn apple_claim_once_sends_signed_secret_in_body_without_basic_or_inferred_pkce_then_records_verified_identity()
 {
    let fixture = Fixture::with_profile(Profile::Apple).await;
    let attempt = begin(&fixture.endpoint).await;
    let authorization: std::collections::BTreeMap<_, _> =
        attempt.url.query_pairs().into_owned().collect();
    assert_eq!(
        attempt.url.origin().ascii_serialization(),
        Profile::Apple.issuer()
    );
    assert_eq!(authorization["response_mode"], "form_post");
    assert_eq!(authorization["nonce"], attempt.nonce);
    assert!(!authorization.contains_key("code_challenge"));
    assert!(!attempt.url.as_str().contains(&fixture.endpoint.secret));
    let mut claims = fixture.token(&attempt.nonce);
    claims["email_verified"] = json!("true");
    fixture.respond(Some(claims));
    assert_eq!(fixture.post(&attempt, "wrong-browser").await, 401);
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    let (a, b) = tokio::join!(
        fixture.post(&attempt, &attempt.cookie),
        fixture.post(&attempt, &attempt.cookie)
    );
    assert!(matches!((a, b), (202, 401) | (401, 202)));
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    let (headers, body) = fixture.provider.requests.lock().unwrap()[0].clone();
    assert!(!headers.contains_key("authorization"));
    assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
    let url = reqwest::Url::parse(&format!("https://fixture.invalid/?{body}")).unwrap();
    let fields: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(fields["grant_type"], "authorization_code");
    assert_eq!(fields["code"], "artificial-code");
    assert_eq!(fields["client_id"], Profile::Apple.client());
    assert_eq!(fields["redirect_uri"], Profile::Apple.redirect());
    assert_eq!(fields["client_secret"], fixture.endpoint.secret);
    assert!(!fields.contains_key("code_verifier"));
    // Independent OpenSSL verification of ES256 client credential sent on wire.
    let pieces: Vec<_> = fields["client_secret"].split('.').collect();
    let payload: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1]).unwrap()).unwrap();
    assert_eq!(payload["aud"], Profile::Apple.issuer());
    assert_eq!(payload["sub"], Profile::Apple.client());
    let signing_key = p256::ecdsa::SigningKey::from_slice(&[42; 32]).unwrap();
    let point = signing_key.verifying_key().to_sec1_point(false);
    let group = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
    let mut ctx = openssl::bn::BigNumContext::new().unwrap();
    let point = openssl::ec::EcPoint::from_bytes(&group, point.as_bytes(), &mut ctx).unwrap();
    let key =
        PKey::from_ec_key(openssl::ec::EcKey::from_public_key(&group, &point).unwrap()).unwrap();
    let signature =
        p256::ecdsa::Signature::from_slice(&URL_SAFE_NO_PAD.decode(pieces[2]).unwrap()).unwrap();
    let mut verifier = openssl::sign::Verifier::new(MessageDigest::sha256(), &key).unwrap();
    verifier
        .update(format!("{}.{}", pieces[0], pieces[1]).as_bytes())
        .unwrap();
    assert!(verifier.verify(signature.to_der().as_bytes()).unwrap());
    let identity: (String, String, String) =
        sqlx::query_as("SELECT provider,issuer,subject FROM flow_identities")
            .fetch_one(fixture.endpoint.db.pool())
            .await
            .unwrap();
    assert_eq!(
        identity,
        (
            "apple".into(),
            Profile::Apple.issuer().into(),
            "stable-subject".into()
        )
    );
    assert_eq!(fixture.status(&attempt).await, "verified");
    assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
}

#[tokio::test]
async fn apple_missing_or_wrong_nonce_id_token_never_becomes_identity_or_replays_exchange() {
    let fixture = Fixture::with_profile(Profile::Apple).await;
    for missing in [false, true] {
        let attempt = begin(&fixture.endpoint).await;
        let mut claims = fixture.token("wrong-nonce");
        claims["email_verified"] = json!("true");
        fixture.respond(if missing { None } else { Some(claims) });
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
        assert_eq!(fixture.status(&attempt).await, "rejected");
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
    }
    assert_eq!(fixture.identities().await, 0);
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn apple_provider_errors_redirect_malformed_success_and_timeout_are_unknown_without_hidden_retry()
 {
    let fixture = Fixture::with_profile(Profile::Apple).await;
    for (index, (status, body, delay)) in [
        (
            400,
            json!({"error":"invalid_client","error_description":"private-provider-diagnostic"}),
            Duration::ZERO,
        ),
        (302, json!({}), Duration::ZERO),
        (503, json!({}), Duration::ZERO),
        (200, json!({"token_type":"Bearer"}), Duration::ZERO),
        (200, json!({"padding":"x".repeat(65536)}), Duration::ZERO),
        (
            200,
            json!({"access_token":"artificial","token_type":"Bearer"}),
            Duration::from_millis(300),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let attempt = begin(&fixture.endpoint).await;
        *fixture.provider.reply.lock().unwrap() = (status, body, delay);
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 503);
        assert_eq!(fixture.status(&attempt).await, "unknown");
        assert_eq!(fixture.post(&attempt, &attempt.cookie).await, 401);
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), index + 1);
    }
    assert_eq!(fixture.identities().await, 0);
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
        ClientId::new(endpoint.profile.client().into()),
        IssuerUrl::new(endpoint.profile.issuer().into()).unwrap(),
        endpoint.keys.clone(),
    )
    .set_auth_uri(AuthUrl::new(endpoint.profile.authorize().into()).unwrap())
    .set_redirect_uri(RedirectUrl::new(endpoint.profile.redirect().into()).unwrap());
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let request = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_extra_param("response_mode", "form_post");
    let (url, state, nonce) = match endpoint.profile {
        Profile::Gitlab => request.set_pkce_challenge(challenge).url(),
        Profile::Apple => request.url(), // Discovery does not advertise PKCE.
    };
    let verifier = match endpoint.profile {
        Profile::Gitlab => Some(verifier.secret()),
        Profile::Apple => None,
    };
    let cookie = CsrfToken::new_random().secret().clone();
    sqlx::query("INSERT INTO flow_attempts(state,cookie,nonce,verifier,config,expires,status) VALUES(?,?,?,?,?,?,'ready')")
        .bind(sha256(state.secret().as_bytes()).to_vec()).bind(sha256(cookie.as_bytes()).to_vec()).bind(nonce.secret()).bind(verifier).bind(endpoint.profile.revision()).bind(NOW+300).execute(endpoint.db.pool()).await.unwrap();
    Attempt {
        state: state.secret().clone(),
        cookie,
        nonce: nonce.secret().clone(),
        url,
    }
}
async fn claim(
    endpoint: &Endpoint,
    state: &str,
    cookie: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    let mut tx = endpoint.db.pool().begin_with("BEGIN IMMEDIATE").await?;
    let claimed=sqlx::query_as("UPDATE flow_attempts SET status='exchanging' WHERE state=? AND cookie=? AND config=? AND expires>? AND status='ready' RETURNING nonce,verifier")
        .bind(sha256(state.as_bytes()).to_vec()).bind(sha256(cookie.as_bytes()).to_vec()).bind(endpoint.profile.revision()).bind(NOW).fetch_optional(&mut *tx).await?;
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
            if sqlx::query("INSERT INTO flow_identities(provider,issuer,subject) VALUES(?,?,?) ON CONFLICT DO NOTHING")
                .bind(endpoint.profile.provider()).bind(endpoint.profile.issuer()).bind(subject).execute(&mut *tx).await.is_err() {return StatusCode::SERVICE_UNAVAILABLE;}
            ("verified", StatusCode::ACCEPTED)
        }
        Ok(None) => ("rejected", StatusCode::UNAUTHORIZED),
        Err(_) => ("unknown", StatusCode::SERVICE_UNAVAILABLE),
    };
    let updated=sqlx::query("UPDATE flow_attempts SET status=?,nonce=NULL,verifier=NULL WHERE state=? AND status='exchanging' AND config=?")
        .bind(outcome).bind(state).bind(endpoint.profile.revision()).execute(&mut *tx).await;
    if !updated.is_ok_and(|result| result.rows_affected() == 1) || tx.commit().await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    response
}
async fn exchange(
    endpoint: &Endpoint,
    code: String,
    nonce: String,
    verifier: Option<String>,
) -> Result<Option<String>, Transport> {
    let client = CoreClient::new(
        ClientId::new(endpoint.profile.client().into()),
        IssuerUrl::new(endpoint.profile.issuer().into()).unwrap(),
        endpoint.keys.clone(),
    )
    .set_client_secret(ClientSecret::new(endpoint.secret.clone()))
    .set_auth_type(match endpoint.profile {
        Profile::Gitlab => AuthType::BasicAuth,
        Profile::Apple => AuthType::RequestBody,
    })
    .set_token_uri(TokenUrl::new(endpoint.token_url.clone()).unwrap())
    .set_redirect_uri(RedirectUrl::new(endpoint.profile.redirect().into()).unwrap());
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
    let request = client.exchange_code(AuthorizationCode::new(code));
    let request = if let Some(verifier) = verifier {
        request.set_pkce_verifier(PkceCodeVerifier::new(verifier))
    } else {
        request
    };
    let response = request
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
            .is_some_and(|p| p.as_str() != endpoint.profile.client())
    {
        return Ok(None);
    }
    Ok(Some(claims.subject().as_str().into()))
}
struct Provider {
    reply: Mutex<(u16, Value, Duration)>,
    calls: AtomicUsize,
    requests: Mutex<Vec<(HeaderMap, String)>>,
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
        Self::with_profile(Profile::Gitlab).await
    }
    async fn with_profile(profile: Profile) -> Self {
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
            requests: Mutex::new(Vec::new()),
        });
        let source = provider.clone();
        let app = Router::new().route(
            "/token",
            post(move |headers: HeaderMap, body: Bytes| {
                let source = source.clone();
                async move {
                    source.calls.fetch_add(1, Ordering::SeqCst);
                    source
                        .requests
                        .lock()
                        .unwrap()
                        .push((headers, String::from_utf8(body.to_vec()).unwrap()));
                    let (status, body, delay) = source.reply.lock().unwrap().clone();
                    tokio::time::sleep(delay).await;
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [
                            ("content-type", "application/json"),
                            ("location", "http://127.0.0.1:1/never-follow"),
                        ],
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
            profile,
            secret: match profile {
                Profile::Gitlab => "artificial-secret".into(),
                Profile::Apple => artificial_apple_secret(),
            },
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
        json!({"iss":self.endpoint.profile.issuer(),"aud":self.endpoint.profile.client(),"sub":"stable-subject","exp":NOW+300,"iat":NOW,"nonce":nonce})
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
        profile: Profile::Gitlab,
        secret: "artificial-secret".into(),
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
