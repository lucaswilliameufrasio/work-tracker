use axum::http::{HeaderMap, HeaderValue, Method, Uri, header};
use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{SessionToken, master_key::MasterKeyStore, session::IssuedSession},
    http::browser_session::{
        BrowserSessionError, BrowserSessionSecurity, clear_session_cookie, issued_session_cookie,
    },
};
use tempfile::TempDir;

fn headers(token: &SessionToken) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::HOST,
        HeaderValue::from_static("tracker.example.test"),
    );
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("__Host-puxiri_session={}", token.expose())).unwrap(),
    );
    headers
}

fn policy(directory: &TempDir) -> BrowserSessionSecurity {
    BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap(),
    )
    .unwrap()
}

#[test]
fn configuration_accepts_only_explicit_https_origins_and_canonicalizes_default_port() {
    let directory = TempDir::new().unwrap();
    let key = MasterKeyStore::load_or_create(&directory.path().join("master.key")).unwrap();
    for origin in [
        "http://tracker.example.test",
        "https://user@tracker.example.test",
        "https://tracker.example.test/path",
        "https://tracker.example.test?x=1",
        "https://tracker.example.test#fragment",
        "https://tracker.example.test:0",
        "https://tracker.example.test:99999",
        "https://tracker.example.test:bad",
        "https://tracker.example.test:",
        "https://tracker.example.test.",
        "//tracker.example.test",
        "https://",
    ] {
        assert!(
            BrowserSessionSecurity::new(origin, key.clone()).is_err(),
            "invalid origin accepted: {origin}"
        );
    }
    for (input, expected) in [
        (
            "https://TRACKER.example.test:443/",
            "https://tracker.example.test",
        ),
        (
            "https://tracker.example.test:8443",
            "https://tracker.example.test:8443",
        ),
        ("https://[::1]:8443", "https://[::1]:8443"),
    ] {
        assert_eq!(
            BrowserSessionSecurity::new(input, key.clone())
                .unwrap()
                .canonical_origin(),
            expected
        );
    }
}

#[test]
fn cookies_are_host_only_secure_http_only_lax_bounded_and_marked_sensitive() {
    let now = Utc::now();
    let issued = IssuedSession {
        id: "synthetic session".into(),
        token: SessionToken::generate().unwrap(),
        expires_at: now + TimeDelta::hours(1),
    };
    let cookie = issued_session_cookie(&issued, now).unwrap();
    assert!(cookie.is_sensitive());
    let text = cookie.to_str().unwrap();
    assert!(text.starts_with("__Host-puxiri_session="));
    assert!(text.ends_with("; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Lax"));
    assert!(!text.contains("Domain="));
    assert!(issued_session_cookie(&issued, issued.expires_at).is_err());
    let clear = clear_session_cookie();
    assert_eq!(
        clear.to_str().unwrap(),
        "__Host-puxiri_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax"
    );
}

#[test]
fn duplicate_malformed_oversized_or_ambiguous_credentials_never_select_a_bearer() {
    let directory = TempDir::new().unwrap();
    let policy = policy(&directory);
    let token = SessionToken::generate().unwrap();
    let uri: Uri = "/api/v1/projects".parse().unwrap();
    let good = headers(&token);
    assert!(
        policy
            .authenticate_request(&Method::GET, &uri, &good)
            .is_ok()
    );
    for extra in [
        format!("__Host-puxiri_session={}", token.expose()),
        "__Host-puxiri_session=bad".to_owned(),
    ] {
        let mut request = good.clone();
        request.append(header::COOKIE, HeaderValue::from_str(&extra).unwrap());
        assert!(matches!(
            policy.authenticate_request(&Method::GET, &uri, &request),
            Err(BrowserSessionError::InvalidSessionCookie)
        ));
    }
    for cookie in [
        "__Host-puxiri_session=bad".to_owned(),
        format!("__Host-puxiri_session=\"{}\"", token.expose()),
        format!(
            "__Host-puxiri_session={}; __Host-puxiri_session={}",
            token.expose(),
            token.expose()
        ),
        format!("broken; __Host-puxiri_session={}", token.expose()),
        format!("bad name=1; __Host-puxiri_session={}", token.expose()),
        format!(
            "padding={}; __Host-puxiri_session={}",
            "x".repeat(8192),
            token.expose()
        ),
        format!("session={}", token.expose()),
    ] {
        let mut request = good.clone();
        request.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        assert!(
            policy
                .authenticate_request(&Method::GET, &uri, &request)
                .is_err()
        );
    }
    let mut request = good.clone();
    request.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer not-a-browser-session"),
    );
    assert!(matches!(
        policy.authenticate_request(&Method::GET, &uri, &request),
        Err(BrowserSessionError::AmbiguousCredentials)
    ));
    let mut request = good.clone();
    request.remove(header::COOKIE);
    assert!(
        policy
            .authenticate_request(&Method::GET, &uri, &request)
            .is_err()
    );
    let mut request = good;
    request.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!(
            "theme=green; __Host-puxiri_session={}; locale=en",
            token.expose()
        ))
        .unwrap(),
    );
    assert!(
        policy
            .authenticate_request(&Method::GET, &uri, &request)
            .is_ok()
    );
}

#[test]
fn host_origin_absolute_authority_and_fetch_metadata_must_not_conflict() {
    let directory = TempDir::new().unwrap();
    let policy = policy(&directory);
    let token = SessionToken::generate().unwrap();
    let good = headers(&token);
    let uri: Uri = "/api/v1/projects".parse().unwrap();
    for (name, value) in [
        ("host", "attacker.example.test"),
        ("origin", "https://attacker.example.test"),
        ("origin", "null"),
        ("origin", "https://tracker.example.test:8443"),
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-site", "same-site"),
    ] {
        let mut request = good.clone();
        request.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        assert!(matches!(
            policy.authenticate_request(&Method::GET, &uri, &request),
            Err(BrowserSessionError::RequestOriginDenied)
        ));
    }
    for name in ["host", "origin", "sec-fetch-site"] {
        let mut request = good.clone();
        let value = match name {
            "host" => "tracker.example.test",
            "origin" => "https://tracker.example.test",
            _ => "same-origin",
        };
        request.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_static(value),
        );
        request.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_static(value),
        );
        assert!(
            policy
                .authenticate_request(&Method::GET, &uri, &request)
                .is_err()
        );
    }
    for target in [
        "https://attacker.example.test/api/v1/projects",
        "http://tracker.example.test/api/v1/projects",
    ] {
        assert!(
            policy
                .authenticate_request(&Method::GET, &target.parse().unwrap(), &good)
                .is_err()
        );
    }
    let mut request = good.clone();
    request.remove(header::HOST);
    request.insert(
        "x-forwarded-host",
        HeaderValue::from_static("tracker.example.test"),
    );
    assert!(
        policy
            .authenticate_request(&Method::GET, &uri, &request)
            .is_err()
    );
    let mut request = good;
    request.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://tracker.example.test"),
    );
    request.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    assert!(
        policy
            .authenticate_request(&Method::HEAD, &uri, &request)
            .is_ok()
    );
}

#[test]
fn unsafe_methods_require_same_origin_and_csrf_bound_to_the_exact_session_and_master_key() {
    let directory = TempDir::new().unwrap();
    let policy = policy(&directory);
    let token = SessionToken::generate().unwrap();
    let other = SessionToken::generate().unwrap();
    let uri: Uri = "/api/v1/projects".parse().unwrap();
    let csrf = policy.csrf_token(&token);
    assert_ne!(csrf.expose(), token.expose());
    assert_eq!(csrf.expose(), policy.csrf_token(&token).expose());
    let mut request = headers(&token);
    request.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://tracker.example.test"),
    );
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        assert!(matches!(
            policy.authenticate_request(&method, &uri, &request),
            Err(BrowserSessionError::InvalidCsrfToken)
        ));
        let mut valid = request.clone();
        valid.insert(
            "x-csrf-token",
            HeaderValue::from_str(csrf.expose()).unwrap(),
        );
        assert!(policy.authenticate_request(&method, &uri, &valid).is_ok());
        valid.remove(header::ORIGIN);
        assert!(matches!(
            policy.authenticate_request(&method, &uri, &valid),
            Err(BrowserSessionError::RequestOriginDenied)
        ));
    }
    for wrong in [
        policy.csrf_token(&other).expose(),
        token.expose(),
        "malformed",
    ] {
        let mut wrong_request = request.clone();
        wrong_request.insert("x-csrf-token", HeaderValue::from_str(wrong).unwrap());
        assert!(
            policy
                .authenticate_request(&Method::POST, &uri, &wrong_request)
                .is_err()
        );
    }
    request.insert(
        "x-csrf-token",
        HeaderValue::from_str(csrf.expose()).unwrap(),
    );
    request.append(
        "x-csrf-token",
        HeaderValue::from_str(csrf.expose()).unwrap(),
    );
    assert!(
        policy
            .authenticate_request(&Method::POST, &uri, &request)
            .is_err()
    );
    let second_directory = TempDir::new().unwrap();
    let other_policy = BrowserSessionSecurity::new(
        "https://tracker.example.test",
        MasterKeyStore::load_or_create(&second_directory.path().join("master.key")).unwrap(),
    )
    .unwrap();
    assert_ne!(csrf.expose(), other_policy.csrf_token(&token).expose());
}
