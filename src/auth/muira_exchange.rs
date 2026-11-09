use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use openidconnect::{
    AuthType, AuthorizationCode, ClientId, ClientSecret, IssuerUrl, Nonce, PkceCodeVerifier,
    RedirectUrl, TokenResponse, TokenUrl,
    core::{CoreClient, CoreJsonWebKeySet, CoreJwsSigningAlgorithm},
};
use serde_json::Value;

use super::entry::{
    EntryError, MuiraCodeExchange, MuiraExchangeFuture, MuiraExchangeRequest, VerifiedMuiraIdentity,
};

const RESPONSE_LIMIT: usize = 65_536;

/// Exchanges one authorization code with the configured Muira issuer.
/// Only that issuer's token and JWKS URLs are contacted, and redirects are
/// refused so a discovery document cannot move the code to another host.
pub struct HttpMuiraExchange;

impl MuiraCodeExchange for HttpMuiraExchange {
    fn exchange(&self, request: MuiraExchangeRequest) -> MuiraExchangeFuture {
        Box::pin(exchange(request))
    }
}

async fn exchange(request: MuiraExchangeRequest) -> Result<VerifiedMuiraIdentity, EntryError> {
    let issuer = normalize_https_issuer(&request.issuer)?;
    let token_url = format!("{issuer}/oauth/token");
    let jwks_url = format!("{issuer}/oauth/jwks.json");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| EntryError::LoginUnavailable)?;
    let jwks = fetch_jwks(&http, &jwks_url).await?;
    let client = CoreClient::new(
        ClientId::new(request.client_id.clone()),
        IssuerUrl::new(issuer).map_err(|_| EntryError::LoginUnavailable)?,
        jwks,
    )
    .set_client_secret(ClientSecret::new(request.client_secret.to_string()))
    .set_auth_type(AuthType::BasicAuth)
    .set_token_uri(TokenUrl::new(token_url.clone()).map_err(|_| EntryError::LoginUnavailable)?)
    .set_redirect_uri(
        RedirectUrl::new(request.redirect_uri.clone()).map_err(|_| EntryError::LoginUnavailable)?,
    );
    let adapter = move |http_request: openidconnect::HttpRequest| {
        let http = http.clone();
        let token_url = token_url.clone();
        async move {
            if http_request.uri().to_string() != token_url || http_request.method() != "POST" {
                return Err(ExchangeTransport);
            }
            let (parts, body) = http_request.into_parts();
            if body.len() > RESPONSE_LIMIT {
                return Err(ExchangeTransport);
            }
            let mut response = http
                .request(parts.method, &token_url)
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|_| ExchangeTransport)?;
            read_limited(&mut response).await
        }
    };
    let token_response = client
        .exchange_code(AuthorizationCode::new(request.code))
        .set_pkce_verifier(PkceCodeVerifier::new(request.verifier.to_string()))
        .request_async(&adapter)
        .await
        .map_err(|_| EntryError::IdentityRejected)?;
    let Some(id_token) = token_response.id_token() else {
        return Err(EntryError::IdentityRejected);
    };
    let now = Utc::now().timestamp();
    let verifier = client
        .id_token_verifier()
        .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256])
        .set_time_fn(move || DateTime::<Utc>::from_timestamp(now, 0).unwrap_or_else(Utc::now))
        .set_issue_time_verifier_fn(move |issued| {
            let issued = issued.timestamp();
            if (now - 300..=now + 60).contains(&issued) {
                Ok(())
            } else {
                Err("invalid issue time".into())
            }
        });
    let serialized = id_token.to_string();
    let claims = id_token
        .claims(&verifier, &Nonce::new(request.nonce))
        .map_err(|_| EntryError::IdentityRejected)?;
    if !not_before_allows(&serialized, now) {
        return Err(EntryError::IdentityRejected);
    }
    let subject = claims.subject().as_str();
    if subject.is_empty() || subject.len() > 256 {
        return Err(EntryError::IdentityRejected);
    }
    if claims
        .authorized_party()
        .is_some_and(|party| party.as_str() != request.client_id)
    {
        return Err(EntryError::IdentityRejected);
    }
    Ok(VerifiedMuiraIdentity {
        issuer: request.issuer,
        subject: subject.to_owned(),
        email: claims.email().map(|email| email.as_str().to_owned()),
        email_verified: claims.email_verified().unwrap_or(false),
    })
}

fn normalize_https_issuer(issuer: &str) -> Result<String, EntryError> {
    let issuer = issuer.trim().trim_end_matches('/');
    if !issuer.starts_with("https://") || issuer.contains([' ', '?', '#']) {
        return Err(EntryError::LoginUnavailable);
    }
    Ok(issuer.to_owned())
}

async fn fetch_jwks(
    http: &reqwest::Client,
    jwks_url: &str,
) -> Result<CoreJsonWebKeySet, EntryError> {
    let response = http
        .get(jwks_url)
        .send()
        .await
        .map_err(|_| EntryError::LoginUnavailable)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(EntryError::LoginUnavailable);
    }
    let bytes = read_bytes(response).await?;
    serde_json::from_slice(&bytes).map_err(|_| EntryError::LoginUnavailable)
}

async fn read_bytes(mut response: reqwest::Response) -> Result<Vec<u8>, EntryError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| EntryError::LoginUnavailable)?
    {
        if bytes.len() + chunk.len() > RESPONSE_LIMIT {
            return Err(EntryError::LoginUnavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_limited(
    response: &mut reqwest::Response,
) -> Result<openidconnect::HttpResponse, ExchangeTransport> {
    let status = response.status();
    let headers = response.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ExchangeTransport)? {
        if bytes.len() + chunk.len() > RESPONSE_LIMIT {
            return Err(ExchangeTransport);
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut builder = openidconnect::http::Response::builder().status(status);
    *builder.headers_mut().unwrap() = headers;
    builder.body(bytes).map_err(|_| ExchangeTransport)
}

fn not_before_allows(token: &str, now: i64) -> bool {
    let Some(payload) = token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
    else {
        return false;
    };
    let Ok(extra) = serde_json::from_slice::<Value>(&payload) else {
        return false;
    };
    !extra
        .get("nbf")
        .is_some_and(|value| value.as_i64().is_none_or(|not_before| not_before > now))
}

#[derive(Debug)]
struct ExchangeTransport;

impl std::fmt::Display for ExchangeTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("muira token exchange failed")
    }
}

impl std::error::Error for ExchangeTransport {}
