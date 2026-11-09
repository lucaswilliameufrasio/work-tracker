use std::sync::Arc;

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    access::{AccessCapability, AccessStore, ProjectRole, authorize},
    auth::time::format_timestamp,
    invitations::{
        InvitationProjectAssignment, OrganizationInvitationError, OrganizationInviteRole,
    },
    mail::{InvitationMail, MailError, MailProvider, MailSenderConfiguration, MailSenderStatus},
};

use super::{
    BrowserState, authenticated_token, malformed_query, storage_unavailable, unauthenticated,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectInput {
    project_id: String,
    role: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueInput {
    email: String,
    role: String,
    #[serde(default)]
    projects: Vec<ProjectInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailInput {
    provider: String,
    api_key: String,
    from_address: Option<String>,
    from_name: Option<String>,
    template: Option<String>,
    origin: Option<String>,
}

#[derive(Serialize)]
struct IssuedBody {
    id: String,
    expires_at: String,
    delivery: &'static str,
    invite_url: String,
    token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_message_id: Option<String>,
}

#[derive(Serialize)]
struct MailBody {
    configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
}

pub(super) async fn issue(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiErrorAlias> {
    let (parts, body) = request.into_parts();
    let token = authenticated_token(&state, &parts).await?;
    if parts.uri.query().is_some() {
        return Err(malformed_query());
    }
    let input: IssueInput = read_json(body).await?;
    let role = match input.role.as_str() {
        "member" => OrganizationInviteRole::Member,
        "guest" => OrganizationInviteRole::Guest,
        _ => {
            return Err(crate::http::error::ApiError::invalid_field(
                "role",
                "Use member or guest",
            ));
        }
    };
    let mut projects = Vec::new();
    for project in input.projects {
        let project_role = match project.role.as_str() {
            "manager" => ProjectRole::Manager,
            "member" => ProjectRole::Member,
            "viewer" => ProjectRole::Viewer,
            _ => {
                return Err(crate::http::error::ApiError::invalid_field(
                    "projects",
                    "Use manager, member, or viewer",
                ));
            }
        };
        projects.push(InvitationProjectAssignment {
            project_id: project.project_id,
            role: project_role,
        });
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let actor = state
        .sessions
        .authenticate_in_transaction(&mut transaction, &token, Utc::now())
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(unauthenticated)?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let issued = state
        .invitations
        .issue(actor.user_id(), &input.email, role, &projects, Utc::now())
        .await
        .map_err(invitation_error)?;
    let invite_url = format!(
        "{}/",
        state.security.canonical_origin().trim_end_matches('/')
    );
    let delivery = state
        .mail
        .send_invitation(&InvitationMail {
            to: input.email,
            invite_url: invite_url.clone(),
            idempotency_key: issued.id.clone(),
        })
        .await;
    let (delivery_name, provider_message_id) = match delivery {
        Ok(receipt) => ("queued", Some(receipt.provider_message_id)),
        Err(MailError::NotConfigured) => ("unconfigured", None),
        Err(MailError::Rejected) => ("rejected", None),
        Err(_) => ("unknown", None),
    };
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(IssuedBody {
            id: issued.id,
            expires_at: format_timestamp(issued.expires_at),
            delivery: delivery_name,
            invite_url,
            token: issued.token.expose().to_owned(),
            provider_message_id,
        }),
    )
        .into_response())
}

pub(super) async fn mail_status(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiErrorAlias> {
    let (parts, _) = request.into_parts();
    let token = authenticated_token(&state, &parts).await?;
    if parts.uri.query().is_some() {
        return Err(malformed_query());
    }
    require_installation_admin(&state, &token).await?;
    let status = state.mail.status().await.map_err(mail_error)?;
    Ok(no_store_mail(status))
}

pub(super) async fn configure_mail(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiErrorAlias> {
    let (parts, body) = request.into_parts();
    let token = authenticated_token(&state, &parts).await?;
    if parts.uri.query().is_some() {
        return Err(malformed_query());
    }
    require_installation_admin(&state, &token).await?;
    let input: MailInput = read_json(body).await?;
    let Some(provider) = MailProvider::parse(&input.provider) else {
        return Err(crate::http::error::ApiError::invalid_field(
            "provider",
            "Use sabia, resend, or millionsend",
        ));
    };
    let status = state
        .mail
        .replace(
            MailSenderConfiguration {
                provider,
                api_key: zeroize::Zeroizing::new(input.api_key),
                from_address: input.from_address,
                from_name: input.from_name,
                template_key: input.template,
                sabia_origin: input.origin,
            },
            Utc::now(),
        )
        .await
        .map_err(mail_error)?;
    Ok(no_store_mail(Some(status)))
}

type ApiErrorAlias = crate::http::error::ApiError;

fn no_store_mail(status: Option<MailSenderStatus>) -> Response {
    let body = match status {
        None => MailBody {
            configured: false,
            provider: None,
            from_address: None,
            from_name: None,
            template: None,
            origin: None,
        },
        Some(status) => MailBody {
            configured: true,
            provider: Some(status.provider.as_str()),
            from_address: status.from_address,
            from_name: status.from_name,
            template: status.template_key,
            origin: status.sabia_origin,
        },
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

async fn require_installation_admin(
    state: &BrowserState,
    token: &crate::auth::SessionToken,
) -> Result<(), ApiErrorAlias> {
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let actor = state
        .sessions
        .authenticate_in_transaction(&mut transaction, token, Utc::now())
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(unauthenticated)?;
    let context = AccessStore::load_organization_context_in_transaction(
        &mut transaction,
        actor.user_id(),
        false,
    )
    .await
    .map_err(|_| storage_unavailable())?
    .ok_or_else(|| {
        crate::http::error::ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "Installation administration is required",
            false,
        )
    })?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    authorize(&context, AccessCapability::ConfigureInstallationSettings).map_err(|_| {
        crate::http::error::ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "Installation administration is required",
            false,
        )
    })
}

async fn read_json<T: for<'de> Deserialize<'de>>(body: Body) -> Result<T, ApiErrorAlias> {
    let bytes = to_bytes(body, 512 * 1024).await.map_err(|_| {
        crate::http::error::ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request body could not be read",
            false,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        crate::http::error::ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "Request JSON is malformed or unsupported",
            false,
        )
    })
}

fn invitation_error(error: OrganizationInvitationError) -> ApiErrorAlias {
    use OrganizationInvitationError::*;
    match error {
        Forbidden => crate::http::error::ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "This invitation is not allowed",
            false,
        ),
        InvalidEmail(_) => {
            crate::http::error::ApiError::invalid_field("email", "Use one email address")
        }
        GuestRequiresProject
        | GuestCannotBeProjectManager
        | DuplicateProject
        | ProjectUnavailable => {
            crate::http::error::ApiError::invalid_field("projects", "The project list is not valid")
        }
        TargetAlreadyMember | InvitationAlreadyOutstanding => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "invitation_conflict",
            "This email cannot receive that invitation",
            false,
        ),
        Disabled => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "invitation_disabled",
            "Invitations are disabled",
            false,
        ),
        Database(_) | Audit(_) | Access(_) | CorruptState | Token(_) | DeadlineOutOfRange => {
            storage_unavailable()
        }
        _ => crate::http::error::ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "The invitation was rejected",
            false,
        ),
    }
}

fn mail_error(error: MailError) -> ApiErrorAlias {
    match error {
        MailError::InvalidConfiguration => crate::http::error::ApiError::invalid_field(
            "provider",
            "The sender configuration is incomplete",
        ),
        MailError::Secret | MailError::Database(_) => storage_unavailable(),
        MailError::NotConfigured => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "mail_unconfigured",
            "No email sender is configured",
            false,
        ),
        MailError::Rejected | MailError::Unknown => storage_unavailable(),
    }
}
