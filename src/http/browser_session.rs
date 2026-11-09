use axum::http::{HeaderMap, HeaderValue, Method, Uri, header};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::{SessionToken, master_key::MasterKey, session::IssuedSession};

const SESSION_COOKIE: &str = "__Host-puxiri_session";
const MUIRA_STATE_COOKIE: &str = "__Host-puxiri_muira";
const MAX_COOKIE_HEADER_BYTES: usize = 8 * 1024;

#[derive(Debug, Error, PartialEq, Eq, Copy, Clone)]
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
    #[error("Muira login state does not match this browser")]
    InvalidMuiraState,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserOrigin {
    origin: String,
    authority: String,
}

impl std::str::FromStr for BrowserOrigin {
    type Err = BrowserSessionError;

    fn from_str(origin: &str) -> Result<Self, Self::Err> {
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
        })
    }
}

impl BrowserOrigin {
    pub fn as_str(&self) -> &str {
        &self.origin
    }
}

impl BrowserSessionSecurity {
    pub fn new(origin: &str, master_key: MasterKey) -> Result<Self, BrowserSessionError> {
        Ok(Self::from_origin(origin.parse()?, master_key))
    }

    pub fn from_origin(origin: BrowserOrigin, master_key: MasterKey) -> Self {
        Self {
            origin: origin.origin,
            authority: origin.authority,
            master_key,
        }
    }

    pub fn canonical_origin(&self) -> &str {
        &self.origin
    }

    pub(super) fn master_key(&self) -> MasterKey {
        self.master_key.clone()
    }

    /// Validates browser transport/CSRF only. Callers must still authenticate the
    /// returned bearer against current persistence and authorize the operation.
    /// Same transport rules as a mutation, without a session cookie. The setup
    /// token or the act of starting login is the credential.
    pub(super) fn authorize_anonymous_mutation(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<(), BrowserSessionError> {
        if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        self.authorize_transport(method, uri, headers, false)
    }

    /// Top-level return from Muira. The state cookie, not Fetch Metadata,
    /// binds the callback to the browser that started it.
    pub(super) fn authorize_callback_navigation(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<(), BrowserSessionError> {
        if *method != Method::GET {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        self.authorize_transport(method, uri, headers, true)
    }

    pub(super) fn authorize_safe_read(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<(), BrowserSessionError> {
        if !matches!(*method, Method::GET | Method::HEAD) {
            return Err(BrowserSessionError::RequestOriginDenied);
        }
        self.authorize_transport(method, uri, headers, false)
    }

    pub fn authenticate_request(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<SessionToken, BrowserSessionError> {
        self.authorize_transport(method, uri, headers, false)?;
        let session = session_cookie(headers)?;
        let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
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

    fn authorize_transport(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        allow_cross_site: bool,
    ) -> Result<(), BrowserSessionError> {
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
        {
            let allowed = if allow_cross_site {
                matches!(site, "same-origin" | "same-site" | "cross-site" | "none")
            } else {
                matches!(site, "same-origin" | "none")
            };
            if !allowed {
                return Err(BrowserSessionError::RequestOriginDenied);
            }
        }
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(BrowserSessionError::AmbiguousCredentials);
        }
        Ok(())
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

pub fn issued_muira_state_cookie(state: &str) -> Result<HeaderValue, BrowserSessionError> {
    let token = SessionToken::parse(state).map_err(|_| BrowserSessionError::InvalidMuiraState)?;
    let value = Zeroizing::new(format!(
        "{MUIRA_STATE_COOKIE}={}; Path=/; Max-Age=600; Secure; HttpOnly; SameSite=Lax",
        token.expose()
    ));
    let mut header =
        HeaderValue::from_str(&value).map_err(|_| BrowserSessionError::InvalidMuiraState)?;
    header.set_sensitive(true);
    Ok(header)
}

pub fn clear_muira_state_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-puxiri_muira=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax",
    )
}

pub(super) fn muira_state_cookie(headers: &HeaderMap) -> Result<SessionToken, BrowserSessionError> {
    named_session_cookie(
        headers,
        MUIRA_STATE_COOKIE,
        BrowserSessionError::InvalidMuiraState,
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
    named_session_cookie(
        headers,
        SESSION_COOKIE,
        BrowserSessionError::InvalidSessionCookie,
    )
}

fn named_session_cookie(
    headers: &HeaderMap,
    cookie_name: &str,
    error: BrowserSessionError,
) -> Result<SessionToken, BrowserSessionError> {
    let mut found = None;
    let mut total_bytes = 0_usize;
    for header in headers.get_all(header::COOKIE) {
        total_bytes = total_bytes.saturating_add(header.as_bytes().len());
        if total_bytes > MAX_COOKIE_HEADER_BYTES {
            return Err(error);
        }
        let header = header.to_str().map_err(|_| error)?;
        for part in header.split(';') {
            let (name, value) = part.trim().split_once('=').ok_or(error)?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err(error);
            }
            if name == cookie_name {
                if found.is_some() {
                    return Err(error);
                }
                found = Some(SessionToken::parse(value).map_err(|_| error)?);
            }
        }
    }
    found.ok_or(error)
}
