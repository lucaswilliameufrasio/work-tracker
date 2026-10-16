use axum::http::{HeaderMap, HeaderValue, Method, Uri, header};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::{SessionToken, master_key::MasterKey, session::IssuedSession};

const SESSION_COOKIE: &str = "__Host-puxiri_session";
const MAX_COOKIE_HEADER_BYTES: usize = 8 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BrowserSessionError {
    #[error("canonical browser origin must be a valid HTTPS origin")]
    InvalidOriginConfiguration,
    #[error("browser request origin or host is not allowed")]
    RequestOriginDenied,
    #[error("one valid browser session cookie is required")]
    InvalidSessionCookie,
    #[error("browser session credentials are ambiguous")]
    AmbiguousCredentials,
    #[error("a session-bound CSRF token is required")]
    InvalidCsrfToken,
    #[error("session cookie deadline is invalid")]
    InvalidCookieDeadline,
}

pub struct BrowserCsrfToken(Zeroizing<String>);

impl BrowserCsrfToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
pub struct BrowserSessionSecurity {
    origin: String,
    authority: String,
    master_key: MasterKey,
}

impl BrowserSessionSecurity {
    pub fn new(origin: &str, master_key: MasterKey) -> Result<Self, BrowserSessionError> {
        let uri: Uri = origin
            .parse()
            .map_err(|_| BrowserSessionError::InvalidOriginConfiguration)?;
        let authority = uri
            .authority()
            .ok_or(BrowserSessionError::InvalidOriginConfiguration)?;
        if uri.scheme_str() != Some("https")
            || origin.contains('#')
            || uri.path() != "/"
            || uri.query().is_some()
            || authority.as_str().contains('@')
            || authority.host().is_empty()
            || authority.host().ends_with('.')
            || !authority
                .host()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte))
        {
            return Err(BrowserSessionError::InvalidOriginConfiguration);
        }
        // Authority::port()/port_u16() return None for an out-of-range port,
        // which must not silently turn explicit malformed input into port 443.
        let suffix = authority
            .as_str()
            .strip_prefix(authority.host())
            .ok_or(BrowserSessionError::InvalidOriginConfiguration)?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .and_then(|value| value.parse::<u16>().ok())
                    .ok_or(BrowserSessionError::InvalidOriginConfiguration)?,
            )
        };
        let host = authority.host().to_ascii_lowercase();
        let authority = match port {
            Some(0) => return Err(BrowserSessionError::InvalidOriginConfiguration),
            Some(443) | None => host,
            Some(port) => format!("{host}:{port}"),
        };
        let canonical_origin = format!("https://{authority}");
        Ok(Self {
            origin: canonical_origin,
            authority,
            master_key,
        })
    }

    pub fn canonical_origin(&self) -> &str {
        &self.origin
    }

    /// Validates browser transport/CSRF only. Callers must still authenticate the
    /// returned bearer against current persistence and authorize the operation.
    pub fn authenticate_request(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<SessionToken, BrowserSessionError> {
        let host = single_header(headers, "host")
            .map_err(|_| BrowserSessionError::RequestOriginDenied)?
            .ok_or(BrowserSessionError::RequestOriginDenied)?;
        if host != self.authority
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str() != self.authority)
            || uri.scheme_str().is_some_and(|scheme| scheme != "https")
        {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        let origin = single_header(headers, "origin")
            .map_err(|_| BrowserSessionError::RequestOriginDenied)?;
        if origin.is_some_and(|origin| origin != self.origin) || !safe && origin.is_none() {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        if let Some(site) = single_header(headers, "sec-fetch-site")
            .map_err(|_| BrowserSessionError::RequestOriginDenied)?
            && !matches!(site, "same-origin" | "none")
        {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(BrowserSessionError::AmbiguousCredentials);
        }
        let session = session_cookie(headers)?;
        if !safe {
            let presented = single_header(headers, "x-csrf-token")
                .map_err(|_| BrowserSessionError::InvalidCsrfToken)?
                .ok_or(BrowserSessionError::InvalidCsrfToken)?;
            // Reuse strict canonical 256-bit Base64URL parsing, never its digest
            // domain as the actual CSRF derivation or an authentication proof.
            SessionToken::parse(presented).map_err(|_| BrowserSessionError::InvalidCsrfToken)?;
            let expected = self.csrf_token(&session);
            if !bool::from(expected.expose().as_bytes().ct_eq(presented.as_bytes())) {
                return Err(BrowserSessionError::InvalidCsrfToken);
            }
        }
        Ok(session)
    }

    /// Only return this value after authenticating the current session, through
    /// a no-store same-origin response. It is not an authentication credential.
    pub fn csrf_token(&self, session: &SessionToken) -> BrowserCsrfToken {
        let digest = Zeroizing::new(
            self.master_key
                .keyed_digest(b"browser-session-csrf-v1", &session.digest()),
        );
        BrowserCsrfToken(Zeroizing::new(URL_SAFE_NO_PAD.encode(digest.as_slice())))
    }
}

pub fn issued_session_cookie(
    session: &IssuedSession,
    now: DateTime<Utc>,
) -> Result<HeaderValue, BrowserSessionError> {
    let seconds = session.expires_at.signed_duration_since(now).num_seconds();
    if seconds <= 0 {
        return Err(BrowserSessionError::InvalidCookieDeadline);
    }
    let value = Zeroizing::new(format!(
        "{SESSION_COOKIE}={}; Path=/; Max-Age={seconds}; Secure; HttpOnly; SameSite=Lax",
        session.token.expose()
    ));
    let mut header =
        HeaderValue::from_str(&value).map_err(|_| BrowserSessionError::InvalidSessionCookie)?;
    header.set_sensitive(true);
    Ok(header)
}

pub fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-puxiri_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax",
    )
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

fn session_cookie(headers: &HeaderMap) -> Result<SessionToken, BrowserSessionError> {
    let mut found = None;
    let mut total_bytes = 0_usize;
    for header in headers.get_all(header::COOKIE) {
        total_bytes = total_bytes.saturating_add(header.as_bytes().len());
        if total_bytes > MAX_COOKIE_HEADER_BYTES {
            return Err(BrowserSessionError::InvalidSessionCookie);
        }
        let header = header
            .to_str()
            .map_err(|_| BrowserSessionError::InvalidSessionCookie)?;
        for part in header.split(';') {
            let (name, value) = part
                .trim()
                .split_once('=')
                .ok_or(BrowserSessionError::InvalidSessionCookie)?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err(BrowserSessionError::InvalidSessionCookie);
            }
            if name == SESSION_COOKIE {
                if found.is_some() {
                    return Err(BrowserSessionError::InvalidSessionCookie);
                }
                found = Some(
                    SessionToken::parse(value)
                        .map_err(|_| BrowserSessionError::InvalidSessionCookie)?,
                );
            }
        }
    }
    found.ok_or(BrowserSessionError::InvalidSessionCookie)
}
