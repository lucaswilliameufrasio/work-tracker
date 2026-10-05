//! Development-only OIDC verification. Fake signing keys, no account/token
//! exchange or product login. Fixed issuer/client/key origin, not token URLs.
use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use openidconnect::{
    AdditionalClaims, ClientId, IdToken, IssuerUrl, Nonce,
    core::{
        CoreGenderClaim, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    },
};
use openssl::{
    hash::MessageDigest,
    pkey::{PKey, Private},
    rsa::Rsa,
    sign::Signer,
};
use puxiri::db::Database;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
const NOW: i64 = 1_800_000_000;
const ISSUER: &str = "https://gitlab.com";
const CLIENT: &str = "local-fixture-client";
const NONCE: &str = "artificial-single-attempt-nonce";
const APPLE_ISSUER: &str = "https://appleid.apple.com";
const APPLE_CLIENT: &str = "com.example.puxiri.web";
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Extra {
    nbf: Option<i64>,
    is_private_email: Option<Value>,
}
impl AdditionalClaims for Extra {}
type Token =
    IdToken<Extra, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm>;
#[derive(Debug, PartialEq)]
enum Failure {
    Keys,
    Token,
}
struct Identity {
    issuer: String,
    subject: String,
    email: Option<String>,
    verified: bool,
    private_email: Option<bool>,
}
fn verify(raw: &str, keys: CoreJsonWebKeySet, nonce: &str) -> Result<Identity, Failure> {
    verify_for(raw, keys, nonce, ISSUER, CLIENT)
}
fn verify_for(
    raw: &str,
    keys: CoreJsonWebKeySet,
    nonce: &str,
    issuer: &str,
    client: &str,
) -> Result<Identity, Failure> {
    if raw.len() > 16384 {
        return Err(Failure::Token);
    }
    let token: Token = serde_json::from_value(json!(raw)).map_err(|_| Failure::Token)?;
    let verifier = CoreIdTokenVerifier::new_public_client(
        ClientId::new(client.into()),
        IssuerUrl::new(issuer.into()).unwrap(),
        keys,
    )
    .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256])
    .set_time_fn(|| DateTime::<Utc>::from_timestamp(NOW, 0).unwrap())
    .set_issue_time_verifier_fn(|issued| {
        if issued.timestamp() <= NOW && issued.timestamp() >= NOW - 300 {
            Ok(())
        } else {
            Err("invalid issue time".into())
        }
    });
    let claims = token
        .claims(&verifier, &Nonce::new(nonce.into()))
        .map_err(|_| Failure::Token)?;
    if claims.subject().as_str().is_empty()
        || claims.subject().as_str().len() > 256
        || claims.additional_claims().nbf.is_some_and(|nbf| nbf > NOW)
        || claims
            .authorized_party()
            .is_some_and(|party| party.as_str() != client)
    {
        return Err(Failure::Token);
    }
    // Apple boolean/string variants are metadata only, never account keys.
    let private_email = match claims.additional_claims().is_private_email.as_ref() {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(value)) if value == "true" => Some(true),
        Some(Value::String(value)) if value == "false" => Some(false),
        _ => return Err(Failure::Token),
    };
    Ok(Identity {
        issuer: claims.issuer().as_str().into(),
        subject: claims.subject().as_str().into(),
        email: claims.email().map(|email| email.as_str().into()),
        verified: claims.email_verified() == Some(true),
        private_email,
    })
}
fn key() -> PKey<Private> {
    PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap()
}
fn jwks(key: &PKey<Private>, kid: &str) -> Value {
    let rsa = key.rsa().unwrap();
    json!({"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":kid,"n":URL_SAFE_NO_PAD.encode(rsa.n().to_vec()),"e":URL_SAFE_NO_PAD.encode(rsa.e().to_vec())}]})
}
fn claims() -> Value {
    json!({"iss":ISSUER,"aud":CLIENT,"sub":"stable-subject","iat":NOW,"exp":NOW+300,"nonce":NONCE,"email":"relay@example.invalid","email_verified":true})
}
fn signed(key: &PKey<Private>, kid: &str, claims: Value) -> String {
    let header = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&json!({"alg":"RS256","kid":kid,"typ":"JWT"})).unwrap());
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let input = format!("{header}.{payload}");
    let mut signer = Signer::new(MessageDigest::sha256(), key).unwrap();
    signer.update(input.as_bytes()).unwrap();
    format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(signer.sign_to_vec().unwrap())
    )
}
fn apple_claims() -> Value {
    json!({"iss":APPLE_ISSUER,"aud":APPLE_CLIENT,"sub":"apple-pairwise-subject","iat":NOW,"exp":NOW+300,"nonce":NONCE,"email":"artificial@privaterelay.appleid.com","email_verified":"true","is_private_email":"true","nonce_supported":true})
}

#[tokio::test]
async fn apple_profile_accepts_boolean_and_string_metadata_without_using_relay_email_as_identity() {
    let fixture = Fixture::new().await;
    let keys = fixture.keys().await.unwrap();
    for value in [json!(true), json!("true"), json!(false), json!("false")] {
        let mut claims = apple_claims();
        claims["email_verified"] = value.clone();
        claims["is_private_email"] = value.clone();
        let identity = verify_for(
            &signed(&fixture.key, "key-one", claims),
            keys.clone(),
            NONCE,
            APPLE_ISSUER,
            APPLE_CLIENT,
        )
        .unwrap();
        assert_eq!(identity.issuer, APPLE_ISSUER);
        assert_eq!(identity.subject, "apple-pairwise-subject");
        let expected = value == json!(true) || value == json!("true");
        assert_eq!(identity.verified, expected);
        assert_eq!(identity.private_email, Some(expected));
    }
    for email in [Some(""), Some("changed@example.invalid"), None] {
        let mut claims = apple_claims();
        if let Some(email) = email {
            claims["email"] = json!(email);
        } else {
            claims.as_object_mut().unwrap().remove("email");
        }
        claims.as_object_mut().unwrap().remove("is_private_email");
        let identity = verify_for(
            &signed(&fixture.key, "key-one", claims),
            keys.clone(),
            NONCE,
            APPLE_ISSUER,
            APPLE_CLIENT,
        )
        .unwrap();
        assert_eq!(identity.subject, "apple-pairwise-subject");
        assert_eq!(identity.private_email, None);
    }
}

#[tokio::test]
async fn apple_profile_rejects_cross_provider_client_nonce_and_invalid_metadata_without_relaxing_crypto()
 {
    let fixture = Fixture::new().await;
    let keys = fixture.keys().await.unwrap();
    let raw = signed(&fixture.key, "key-one", apple_claims());
    assert!(verify(&raw, keys.clone(), NONCE).is_err());
    for (field, value) in [
        ("iss", json!(ISSUER)),
        ("aud", json!(CLIENT)),
        ("nonce", json!("wrong")),
        ("exp", json!(NOW)),
        ("email_verified", json!("TRUE")),
        ("email_verified", json!(1)),
        ("is_private_email", json!("yes")),
        ("is_private_email", json!(1)),
    ] {
        let mut claims = apple_claims();
        claims[field] = value;
        assert!(
            verify_for(
                &signed(&fixture.key, "key-one", claims),
                keys.clone(),
                NONCE,
                APPLE_ISSUER,
                APPLE_CLIENT
            )
            .is_err(),
            "accepted {field}"
        );
    }
    let parts: Vec<_> = raw.split('.').collect();
    // ES256 is the CLIENT-secret algorithm, not an accepted Apple ID-token alg
    // in this RS256 profile. No header-based auto-enablement.
    let wrong_alg = format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"key-one"}"#),
        parts[1],
        parts[2]
    );
    assert!(verify_for(&wrong_alg, keys.clone(), NONCE, APPLE_ISSUER, APPLE_CLIENT).is_err());
    assert!(
        verify_for(
            &signed(&key(), "key-one", apple_claims()),
            keys,
            NONCE,
            APPLE_ISSUER,
            APPLE_CLIENT
        )
        .is_err()
    );
}
struct Fixture {
    key: PKey<Private>,
    document: Arc<Mutex<(u16, Value)>>,
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let key = key();
        let document = Arc::new(Mutex::new((200, jwks(&key, "key-one"))));
        let source = document.clone();
        let app = Router::new().route(
            "/configured-jwks",
            get(move || {
                let source = source.clone();
                async move {
                    let (status, value) = source.lock().unwrap().clone();
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("location", "http://127.0.0.1:1/never-follow")],
                        value.to_string(),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            key,
            document,
            base,
            task,
        }
    }
    async fn keys(&self) -> Result<CoreJsonWebKeySet, Failure> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let mut response = client
            .get(format!("{}/configured-jwks", self.base))
            .send()
            .await
            .map_err(|_| Failure::Keys)?;
        if response.status() != 200 {
            return Err(Failure::Keys);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Keys)? {
            if bytes.len() + chunk.len() > 65536 {
                return Err(Failure::Keys);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::Keys)
    }
}

#[tokio::test]
async fn maintained_verifier_accepts_only_signed_issuer_audience_time_and_nonce_bound_identity() {
    let fixture = Fixture::new().await;
    let keys = fixture.keys().await.unwrap();
    let raw = signed(&fixture.key, "key-one", claims());
    let identity = verify(&raw, keys.clone(), NONCE).unwrap();
    assert_eq!(identity.issuer, ISSUER);
    assert_eq!(identity.subject, "stable-subject");
    assert!(identity.verified);
    assert!(matches!(
        verify(&raw, keys.clone(), "other-attempt"),
        Err(Failure::Token)
    ));
    for (name, value) in [
        ("iss", json!("https://evil.example.invalid")),
        ("aud", json!("other-client")),
        ("aud", json!([CLIENT, "untrusted-client"])),
        ("exp", json!(NOW)),
        ("iat", json!(NOW + 1)),
        ("iat", json!(NOW - 301)),
        ("nbf", json!(NOW + 1)),
        ("nonce", json!("wrong")),
        ("azp", json!("other-client")),
        ("sub", json!("")),
    ] {
        let mut changed = claims();
        changed[name] = value;
        let token = signed(&fixture.key, "key-one", changed);
        assert!(
            matches!(verify(&token, keys.clone(), NONCE), Err(Failure::Token)),
            "accepted invalid {name}"
        );
    }
    for name in ["iss", "aud", "sub", "exp", "iat", "nonce"] {
        let mut missing = claims();
        missing.as_object_mut().unwrap().remove(name);
        assert!(
            matches!(
                verify(
                    &signed(&fixture.key, "key-one", missing),
                    keys.clone(),
                    NONCE
                ),
                Err(Failure::Token)
            ),
            "accepted missing {name}"
        );
    }
}

#[tokio::test]
async fn algorithm_confusion_tampering_unknown_kid_and_untrusted_key_fail_without_token_disclosure()
{
    let fixture = Fixture::new().await;
    let keys = fixture.keys().await.unwrap();
    let other = key();
    let raw = signed(&fixture.key, "key-one", claims());
    let pieces: Vec<&str> = raw.split('.').collect();
    for token in [
        signed(&other, "key-one", claims()),
        signed(&fixture.key, "unknown", claims()),
        format!(
            "{}.{}.{}",
            pieces[0],
            URL_SAFE_NO_PAD.encode(b"{}"),
            pieces[2]
        ),
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            pieces[1]
        ),
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"key-one"}"#),
            pieces[1],
            pieces[2]
        ),
        "not-a-jwt".into(),
        "x".repeat(16385),
    ] {
        let error = verify(&token, keys.clone(), NONCE).err().unwrap();
        assert_eq!(error, Failure::Token);
        assert_eq!(format!("{error:?}"), "Token");
    }
    *fixture.document.lock().unwrap() = (200, json!({"keys":[]}));
    assert!(matches!(
        verify(&raw, fixture.keys().await.unwrap(), NONCE),
        Err(Failure::Token)
    ));
}

#[tokio::test]
async fn configured_key_refresh_handles_rotation_but_never_follows_redirect_or_token_key_url() {
    let fixture = Fixture::new().await;
    let old_keys = fixture.keys().await.unwrap();
    let rotated = key();
    let token = signed(&rotated, "key-two", claims());
    assert!(matches!(
        verify(&token, old_keys, NONCE),
        Err(Failure::Token)
    ));
    *fixture.document.lock().unwrap() = (200, jwks(&rotated, "key-two"));
    assert!(verify(&token, fixture.keys().await.unwrap(), NONCE).is_ok());
    for (status, value) in [
        (302, json!({})),
        (503, json!({"private":"not-leaked"})),
        (200, json!({"invalid":"not-a-keyset"})),
        (200, json!({"padding":"x".repeat(65536)})),
    ] {
        *fixture.document.lock().unwrap() = (status, value);
        assert!(matches!(fixture.keys().await, Err(Failure::Keys)));
    }
}

#[tokio::test]
async fn external_identity_key_never_merges_subjects_by_shared_email_and_missing_email_stays_valid()
{
    let fixture = Fixture::new().await;
    let keys = fixture.keys().await.unwrap();
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    sqlx::query("CREATE TABLE fixture_identities(provider TEXT,issuer TEXT,subject TEXT,email TEXT,verified INTEGER,PRIMARY KEY(provider,issuer,subject))").execute(db.pool()).await.unwrap();
    for (subject, email) in [
        ("one", Some("shared@example.invalid")),
        ("two", Some("shared@example.invalid")),
        ("three", None),
    ] {
        let mut event = claims();
        event["sub"] = json!(subject);
        event["email_verified"] = json!(false);
        if let Some(email) = email {
            event["email"] = json!(email);
        } else {
            event.as_object_mut().unwrap().remove("email");
        }
        let identity =
            verify(&signed(&fixture.key, "key-one", event), keys.clone(), NONCE).unwrap();
        assert!(!identity.verified);
        sqlx::query("INSERT INTO fixture_identities VALUES('gitlab',?,?,?,?)")
            .bind(identity.issuer)
            .bind(identity.subject)
            .bind(identity.email)
            .bind(identity.verified)
            .execute(db.pool())
            .await
            .unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fixture_identities")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM fixture_identities WHERE email='shared@example.invalid'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2
    );
    db.close().await;
}
