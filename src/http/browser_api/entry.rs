use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::{
    auth::{
        PasswordError, SessionToken,
        entry::{EntryError, MuiraExchangeRequest, MuiraLoginOutcome},
        normalize_email_address,
        setup::SetupToken,
    },
    organization_timezone::OrganizationTimezone,
    projects::{ProjectCreationError, ProjectDefaultView},
};

use super::{
    super::{
        browser_session::{
            clear_muira_state_cookie, issued_muira_state_cookie, issued_session_cookie,
            muira_state_cookie,
        },
        error::ApiError,
    },
    BrowserState, CurrentSession, authenticated_token, malformed_query, session_error,
    storage_unavailable, transport_error, unauthenticated,
};

const MAX_CLIENT_SECRET_BYTES: usize = 512;

#[derive(Serialize)]
struct InstallationStatusBody {
    setup_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    login: Option<&'static str>,
}

#[derive(Serialize)]
struct AuthorizationBody {
    authorization_url: String,
}

#[derive(Serialize)]
struct CreatedProject {
    id: String,
    project_key: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    default_view: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PuxiriSetupBody {
    setup_token: String,
    organization_name: String,
    locale: String,
    timezone: String,
    display_name: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MuiraSetupBody {
    setup_token: String,
    organization_name: String,
    locale: String,
    timezone: String,
    display_name: String,
    email: String,
    issuer: String,
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PuxiriLoginBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyBody {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectBody {
    project_key: String,
    name: String,
    description: Option<String>,
}

pub(super) async fn setup_status(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, _) = request.into_parts();
    state
        .security
        .authorize_safe_read(&request.method, &request.uri, &request.headers)
        .map_err(transport_error)?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
    }
    let status = state.entry.status().await.map_err(entry_error)?;
    Ok(no_store(InstallationStatusBody {
        setup_required: status.setup_required,
        login: match status.login {
            Some(crate::auth::entry::LoginSource::Puxiri) => Some("puxiri"),
            Some(crate::auth::entry::LoginSource::Muira) => Some("muira"),
            None => None,
        },
    }))
}

pub(super) async fn setup_puxiri(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    authorize_anonymous(&state, &request)?;
    reject_query(&request)?;
    let input: PuxiriSetupBody = read_json(body).await?;
    let token = setup_token(&input.setup_token)?;
    validate_organization(
        &input.organization_name,
        &input.locale,
        &input.timezone,
        &input.display_name,
        &input.email,
    )?;
    let now = Utc::now();
    let session = state
        .entry
        .complete_puxiri_setup(
            &token,
            crate::auth::entry::BuiltinSetup {
                organization_name: &input.organization_name,
                locale: &input.locale,
                timezone: &input.timezone,
                display_name: &input.display_name,
                email: &input.email,
                password: input.password,
            },
            now,
        )
        .await
        .map_err(entry_error)?;
    session_response(&state, session, now).await
}

pub(super) async fn setup_muira(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    authorize_anonymous(&state, &request)?;
    reject_query(&request)?;
    let input: MuiraSetupBody = read_json(body).await?;
    let token = setup_token(&input.setup_token)?;
    validate_organization(
        &input.organization_name,
        &input.locale,
        &input.timezone,
        &input.display_name,
        &input.email,
    )?;
    validate_muira_client(&input.issuer, &input.client_id, &input.client_secret)?;
    let now = Utc::now();
    let authorization = state
        .entry
        .begin_muira_setup(
            &token,
            crate::auth::entry::MuiraInstallation {
                organization_name: &input.organization_name,
                locale: &input.locale,
                timezone: &input.timezone,
                display_name: &input.display_name,
                email: &input.email,
                issuer: &input.issuer,
                client_id: &input.client_id,
                client_secret: &input.client_secret,
                redirect_uri: &callback_uri(&state),
            },
            now,
        )
        .await
        .map_err(entry_error)?;
    authorization_response(authorization.url)
}

pub(super) async fn login_muira(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    authorize_anonymous(&state, &request)?;
    reject_query(&request)?;
    let EmptyBody {} = read_json(body).await?;
    let authorization = state
        .entry
        .begin_muira_login(&callback_uri(&state), Utc::now())
        .await
        .map_err(entry_error)?;
    authorization_response(authorization.url)
}

pub(super) async fn login_puxiri(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    authorize_anonymous(&state, &request)?;
    reject_query(&request)?;
    let input: PuxiriLoginBody = read_json(body).await?;
    normalize_email_address(&input.email)
        .map_err(|_| ApiError::invalid_field("email", "Use one email address"))?;
    let now = Utc::now();
    let session = state
        .entry
        .login_puxiri(&input.email, input.password, now)
        .await
        .map_err(entry_error)?;
    session_response(&state, session, now).await
}

pub(super) async fn muira_callback(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Response {
    let (request, _) = request.into_parts();
    if request.method == axum::http::Method::HEAD {
        return method_not_allowed_get();
    }
    if let Err(error) = state.security.authorize_callback_navigation(
        &request.method,
        &request.uri,
        &request.headers,
    ) {
        return transport_error(error).into_response();
    }
    let Ok((code, query_state)) = callback_query(request.uri.query()) else {
        return malformed_query().into_response();
    };
    let Ok(query_state) = SessionToken::parse(&query_state) else {
        return malformed_query().into_response();
    };
    let Ok(cookie_state) = muira_state_cookie(&request.headers) else {
        return callback_terminal(&state, "identidade", None);
    };
    if !bool::from(
        query_state
            .expose()
            .as_bytes()
            .ct_eq(cookie_state.expose().as_bytes()),
    ) {
        return callback_terminal(&state, "identidade", None);
    }
    let now = Utc::now();
    let materials = match state.entry.exchange_materials(cookie_state.expose()).await {
        Ok(materials) => materials,
        Err(error) => return callback_terminal(&state, callback_error_code(&error), None),
    };
    let identity = match state
        .exchange
        .exchange(MuiraExchangeRequest {
            issuer: materials.issuer,
            client_id: materials.client_id,
            client_secret: materials.client_secret,
            redirect_uri: callback_uri(&state),
            code,
            verifier: materials.verifier.to_string(),
            nonce: materials.nonce,
        })
        .await
    {
        Ok(identity) => identity,
        Err(error) => return callback_terminal(&state, callback_error_code(&error), None),
    };
    match state
        .entry
        .finish_muira_callback(cookie_state.expose(), &identity, now)
        .await
    {
        Ok(MuiraLoginOutcome::Session(session)) => {
            callback_terminal(&state, "projects", Some(session))
        }
        Ok(MuiraLoginOutcome::InvitationRequired) => callback_terminal(&state, "convite", None),
        Err(error) => callback_terminal(&state, callback_error_code(&error), None),
    }
}

pub(super) async fn create_project(
    State(state): State<std::sync::Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (request, body) = request.into_parts();
    let token = authenticated_token(&state, &request).await?;
    reject_query(&request)?;
    let input: CreateProjectBody = read_json(body).await?;
    let description = match input.description {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        None => None,
    };
    let now = Utc::now();
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let principal = state
        .sessions
        .authenticate_in_transaction(&mut transaction, &token, now)
        .await
        .map_err(session_error)?
        .ok_or_else(unauthenticated)?;
    let actor = principal.user_id().to_owned();
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let project = state
        .project_writer
        .create(
            &actor,
            input.project_key.trim(),
            &input.name,
            description.as_deref(),
            now,
        )
        .await
        .map_err(project_error)?;
    let location = HeaderValue::from_str(&format!("/api/v1/projects/{}", project.id))
        .map_err(|_| unexpected())?;
    let mut response = (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(CreatedProject {
            id: project.id,
            project_key: project.project_key,
            name: project.name,
            description: project.description,
            default_view: match project.default_view {
                ProjectDefaultView::Board => "board",
                ProjectDefaultView::List => "list",
            },
        }),
    )
        .into_response();
    response.headers_mut().insert(header::LOCATION, location);
    Ok(response)
}

fn authorize_anonymous(
    state: &BrowserState,
    request: &axum::http::request::Parts,
) -> Result<(), ApiError> {
    state
        .security
        .authorize_anonymous_mutation(&request.method, &request.uri, &request.headers)
        .map_err(transport_error)
}

fn reject_query(request: &axum::http::request::Parts) -> Result<(), ApiError> {
    if request.uri.query().is_some() {
        Err(malformed_query())
    } else {
        Ok(())
    }
}

fn callback_uri(state: &BrowserState) -> String {
    format!(
        "{}/api/v1/auth/muira/callback",
        state.security.canonical_origin()
    )
}

fn authorization_response(url: String) -> Result<Response, ApiError> {
    let state = state_from_authorization(&url)?;
    let cookie = issued_muira_state_cookie(&state).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_error",
            "Unexpected error",
            false,
        )
    })?;
    let mut response = no_store(AuthorizationBody {
        authorization_url: url,
    });
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

fn state_from_authorization(url: &str) -> Result<String, ApiError> {
    let Some((_, query)) = url.split_once('?') else {
        return Err(unexpected());
    };
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("state=") {
            SessionToken::parse(value).map_err(|_| unexpected())?;
            return Ok(value.to_owned());
        }
    }
    Err(unexpected())
}

async fn session_response(
    state: &BrowserState,
    session: crate::auth::session::IssuedSession,
    now: DateTime<Utc>,
) -> Result<Response, ApiError> {
    let authenticated = state
        .sessions
        .validate(&session.token, now)
        .await
        .map_err(session_error)?
        .ok_or_else(storage_unavailable)?;
    let csrf = state.security.csrf_token(&session.token);
    let cookie = issued_session_cookie(&session, now).map_err(|_| storage_unavailable())?;
    let mut response = no_store(CurrentSession {
        user_id: &authenticated.user_id,
        session_id: &authenticated.id,
        csrf_token: csrf.expose(),
    });
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

fn callback_terminal(
    state: &BrowserState,
    kind: &str,
    session: Option<crate::auth::session::IssuedSession>,
) -> Response {
    let mut cookies = vec![clear_muira_state_cookie()];
    let mut kind = kind;
    if let Some(session) = session.as_ref() {
        match issued_session_cookie(session, Utc::now()) {
            Ok(cookie) => cookies.push(cookie),
            Err(_) => kind = "indisponivel",
        }
    }
    let origin = state.security.canonical_origin();
    let location = if kind == "projects" {
        format!("{origin}/projects")
    } else if kind == "convite" {
        format!("{origin}/convite")
    } else {
        format!("{origin}/?erro={kind}")
    };
    let mut response = (
        StatusCode::SEE_OTHER,
        [
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::LOCATION,
                HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
            ),
        ],
    )
        .into_response();
    for cookie in cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

fn callback_error_code(error: &EntryError) -> &'static str {
    match error {
        EntryError::SetupUnavailable => "setup",
        EntryError::LoginUnavailable => "login",
        EntryError::IdentityRejected => "identidade",
        EntryError::Database(_)
        | EntryError::Secret(_)
        | EntryError::Session(_)
        | EntryError::Invitation(_)
        | EntryError::Audit(_)
        | EntryError::Password(_) => "indisponivel",
    }
}

fn method_not_allowed_get() -> Response {
    let mut response = ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Method not supported for this resource",
        false,
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET"));
    response
}

async fn read_json<T: for<'de> Deserialize<'de>>(body: Body) -> Result<T, ApiError> {
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request body could not be read",
            false,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request JSON is malformed or unsupported",
            false,
        )
    })
}

fn setup_token(value: &str) -> Result<SetupToken, ApiError> {
    SetupToken::parse(value)
        .map_err(|_| ApiError::invalid_field("setup_token", "Use the current setup token"))
}

fn validate_organization(
    organization_name: &str,
    locale: &str,
    timezone: &str,
    display_name: &str,
    email: &str,
) -> Result<(), ApiError> {
    required_text(organization_name, "organization_name")?;
    if !matches!(locale, "pt-BR" | "en") {
        return Err(ApiError::invalid_field("locale", "Use pt-BR or en"));
    }
    OrganizationTimezone::parse(timezone)
        .map_err(|_| ApiError::invalid_field("timezone", "Use a supported IANA timezone"))?;
    required_text(display_name, "display_name")?;
    normalize_email_address(email)
        .map_err(|_| ApiError::invalid_field("email", "Use one email address"))?;
    Ok(())
}

fn validate_muira_client(
    issuer: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<(), ApiError> {
    let normalized = issuer.trim().trim_end_matches('/');
    if !normalized.starts_with("https://")
        || normalized.len() <= "https://".len()
        || normalized.contains([' ', '?', '#'])
    {
        return Err(ApiError::invalid_field(
            "issuer",
            "Use the Muira HTTPS issuer",
        ));
    }
    required_text(client_id, "client_id")?;
    if client_secret.is_empty() || client_secret.len() > MAX_CLIENT_SECRET_BYTES {
        return Err(ApiError::invalid_field(
            "client_secret",
            "Use the Muira client secret",
        ));
    }
    Ok(())
}

fn required_text(value: &str, field: &'static str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        Err(ApiError::invalid_field(field, "This field is required"))
    } else {
        Ok(())
    }
}

fn callback_query(query: Option<&str>) -> Result<(String, String), ()> {
    let Some(query) = query else {
        return Err(());
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(());
        };
        let key = percent_decode(key)?;
        let value = percent_decode(value)?;
        match key.as_str() {
            "code" => {
                if code.is_some() {
                    return Err(());
                }
                code = Some(value);
            }
            "state" => {
                if state.is_some() {
                    return Err(());
                }
                state = Some(value);
            }
            _ => return Err(()),
        }
    }
    let code = code.ok_or(())?;
    let state = state.ok_or(())?;
    if !(1..=1024).contains(&code.len())
        || code
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(());
    }
    Ok((code, state))
}

fn percent_decode(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| ())?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| ())?);
            index += 3;
            continue;
        }
        if bytes[index].is_ascii()
            && !bytes[index].is_ascii_whitespace()
            && !bytes[index].is_ascii_control()
        {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        return Err(());
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn entry_error(error: EntryError) -> ApiError {
    match error {
        EntryError::SetupUnavailable => ApiError::new(
            StatusCode::CONFLICT,
            "setup_unavailable",
            "Installation setup is not available",
            false,
        ),
        EntryError::IdentityRejected | EntryError::LoginUnavailable => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "The credentials were not accepted",
            false,
        ),
        EntryError::Password(PasswordError::PolicyRejected) => ApiError::invalid_field(
            "password",
            "Use a password that meets the installation policy",
        ),
        EntryError::Password(
            PasswordError::Busy | PasswordError::WorkerFailed | PasswordError::HashingFailed,
        )
        | EntryError::Database(_)
        | EntryError::Session(_)
        | EntryError::Invitation(_)
        | EntryError::Audit(_) => storage_unavailable(),
        EntryError::Secret(_) | EntryError::Password(_) => unexpected(),
    }
}

fn project_error(error: ProjectCreationError) -> ApiError {
    match error {
        ProjectCreationError::InvalidProjectKey => {
            ApiError::invalid_field("project_key", "Use 2 to 6 uppercase ASCII letters")
        }
        ProjectCreationError::InvalidProjectName => {
            ApiError::invalid_field("name", "Use a project name")
        }
        ProjectCreationError::UnknownActor => unauthenticated(),
        ProjectCreationError::Forbidden => ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "Permission denied",
            false,
        ),
        ProjectCreationError::ProjectKeyUnavailable => ApiError::new(
            StatusCode::CONFLICT,
            "project_key_unavailable",
            "Project key is already in use",
            false,
        ),
        ProjectCreationError::Database(_) | ProjectCreationError::Audit(_) => storage_unavailable(),
        ProjectCreationError::AccessState(error) => match error {
            crate::access::AccessStoreError::Database(_) => storage_unavailable(),
            _ => unexpected(),
        },
        ProjectCreationError::CorruptOrganizationLocale => unexpected(),
    }
}

fn unexpected() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected_error",
        "Unexpected error",
        false,
    )
}

fn no_store<T: Serialize>(body: T) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}
