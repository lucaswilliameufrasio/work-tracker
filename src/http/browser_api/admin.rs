use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{
    access::{AccessCapability, AccessStore, authorize},
    accounts::{AccountAdministrationError, AccountAdministrationStore, AccountStatusChange},
    replication::ReplicationError,
};

use super::{
    BrowserState, authenticated_token, malformed_query, no_store, storage_unavailable,
    unauthenticated,
};

pub(super) fn routes() -> Router<Arc<BrowserState>> {
    Router::new()
        .route("/api/v1/admin/overview", get(overview))
        .route("/api/v1/admin/people", get(people))
        .route("/api/v1/admin/people/{person_id}/ban", post(ban_person))
        .route("/api/v1/admin/people/{person_id}/unban", post(unban_person))
        .route("/api/v1/admin/catalog", get(catalog))
        .route("/api/v1/admin/audit", get(audit))
        .route("/api/v1/admin/replication/capture", post(capture))
        .route("/api/v1/admin/replication/restore", post(restore))
        .route(
            "/api/v1/admin/replication/remote/restore",
            post(restore_remote),
        )
}

#[derive(Serialize)]
struct OverviewBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    login_source: Option<&'static str>,
    mail_configured: bool,
    replication: ReplicationBody,
}

#[derive(Serialize)]
struct ReplicationBody {
    snapshot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<String>,
    same_filesystem: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<RemoteStatusBody>,
}

#[derive(Serialize)]
struct RemoteStatusBody {
    location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    acknowledged_txid: Option<String>,
}

#[derive(Serialize)]
struct PersonBody {
    user_id: String,
    display_name: String,
    organization_role: String,
    state: String,
    installation_admin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_email: Option<String>,
}

#[derive(Serialize)]
struct PeopleBody {
    data: Vec<PersonBody>,
    has_more: bool,
}

#[derive(Serialize)]
struct ProjectBody {
    id: String,
    project_key: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
    membership_count: i64,
    issue_count: i64,
}

#[derive(Serialize)]
struct ProjectPageBody {
    data: Vec<ProjectBody>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct AuditBody {
    id: String,
    event_kind: String,
    result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_user_id: Option<String>,
    target_kind: String,
    target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    occurred_at: String,
}

#[derive(Serialize)]
struct AuditPageBody {
    data: Vec<AuditBody>,
    has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BanInput {
    reason: String,
}

#[derive(Serialize)]
struct ChangeBody {
    changed: bool,
}

#[derive(Serialize)]
struct CaptureBody {
    txid: String,
    transactions: u64,
    wal_frames: u64,
    snapshot_created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<RemoteSyncBody>,
}

#[derive(Serialize)]
struct RemoteSyncBody {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    objects_confirmed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_confirmed: Option<u64>,
}

#[derive(Serialize)]
struct RestoreBody {
    file_name: String,
}

async fn overview(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let login_source = login_source(&state).await?;
    let mail_configured = state
        .mail
        .status()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "mail status failed");
            storage_unavailable()
        })?
        .is_some();
    let replication = crate::replication::status(&state.database).await;
    Ok(no_store(OverviewBody {
        login_source,
        mail_configured,
        replication: ReplicationBody {
            snapshot: replication.snapshot,
            txid: replication.txid,
            same_filesystem: replication.same_filesystem,
            remote: replication.remote.map(|remote| RemoteStatusBody {
                location: remote.location,
                acknowledged_txid: remote.acknowledged_txid,
            }),
        },
    }))
}

async fn people(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let rows = sqlx::query(
        "SELECT users.id, users.display_name, users.state, organization_members.role, \
                EXISTS(SELECT 1 FROM installation_admins \
                       WHERE installation_admins.user_id = users.id) AS installation_admin, \
                (SELECT user_emails.address FROM user_emails \
                  WHERE user_emails.user_id = users.id AND user_emails.is_primary = 1 \
                  LIMIT 1) AS primary_email \
         FROM users \
         JOIN organization_members ON organization_members.user_id = users.id \
         ORDER BY users.display_name ASC, users.id ASC \
         LIMIT 201",
    )
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "people query failed");
        storage_unavailable()
    })?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let has_more = rows.len() > 200;
    let mut data = Vec::new();
    for row in rows.into_iter().take(200) {
        let organization_role: String = row.try_get("role").map_err(|_| storage_unavailable())?;
        let state_name: String = row.try_get("state").map_err(|_| storage_unavailable())?;
        if !matches!(
            organization_role.as_str(),
            "owner" | "admin" | "member" | "guest"
        ) || !matches!(state_name.as_str(), "active" | "banned" | "disabled")
        {
            return Err(storage_unavailable());
        }
        let flag: i64 = row
            .try_get("installation_admin")
            .map_err(|_| storage_unavailable())?;
        let installation_admin = match flag {
            0 => false,
            1 => true,
            _ => return Err(storage_unavailable()),
        };
        let primary_email: Option<String> = row
            .try_get("primary_email")
            .map_err(|_| storage_unavailable())?;
        data.push(PersonBody {
            user_id: row.try_get("id").map_err(|_| storage_unavailable())?,
            display_name: row
                .try_get("display_name")
                .map_err(|_| storage_unavailable())?,
            organization_role,
            state: state_name,
            installation_admin,
            primary_email: primary_email.filter(|email| !email.is_empty()),
        });
    }
    Ok(no_store(PeopleBody { data, has_more }))
}

async fn catalog(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let rows = sqlx::query(
        "SELECT projects.id, projects.project_key, projects.name, projects.archived_at, \
                (SELECT COUNT(*) FROM project_members \
                  WHERE project_members.project_id = projects.id) AS membership_count, \
                (SELECT COUNT(*) FROM issues \
                  WHERE issues.project_id = projects.id) AS issue_count \
         FROM projects \
         WHERE projects.organization_id = 1 \
         ORDER BY projects.project_key ASC \
         LIMIT 101",
    )
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "catalog query failed");
        storage_unavailable()
    })?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let has_more = rows.len() > 100;
    let mut data = Vec::new();
    for row in rows.into_iter().take(100) {
        let archived_at: Option<String> = row
            .try_get("archived_at")
            .map_err(|_| storage_unavailable())?;
        data.push(ProjectBody {
            id: row.try_get("id").map_err(|_| storage_unavailable())?,
            project_key: row
                .try_get("project_key")
                .map_err(|_| storage_unavailable())?,
            name: row.try_get("name").map_err(|_| storage_unavailable())?,
            archived_at: archived_at.filter(|value| !value.is_empty()),
            membership_count: row
                .try_get("membership_count")
                .map_err(|_| storage_unavailable())?,
            issue_count: row
                .try_get("issue_count")
                .map_err(|_| storage_unavailable())?,
        });
    }
    let next_cursor = if has_more {
        data.last().map(|project| project.project_key.clone())
    } else {
        None
    };
    Ok(no_store(ProjectPageBody {
        data,
        has_more,
        next_cursor,
    }))
}

async fn audit(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|_| storage_unavailable())?;
    let rows = sqlx::query(
        "SELECT id, event_kind, result, actor_user_id, target_kind, target_id, reason, occurred_at \
         FROM audit_events \
         ORDER BY occurred_at DESC, id DESC \
         LIMIT 51",
    )
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "audit query failed");
        storage_unavailable()
    })?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    let has_more = rows.len() > 50;
    let mut data = Vec::new();
    for row in rows.into_iter().take(50) {
        let result: String = row.try_get("result").map_err(|_| storage_unavailable())?;
        if !matches!(result.as_str(), "success" | "failure") {
            return Err(storage_unavailable());
        }
        let event_kind: String = row
            .try_get("event_kind")
            .map_err(|_| storage_unavailable())?;
        let occurred_at: String = row
            .try_get("occurred_at")
            .map_err(|_| storage_unavailable())?;
        let target_id: String = row
            .try_get("target_id")
            .map_err(|_| storage_unavailable())?;
        if event_kind.is_empty() || occurred_at.is_empty() || !canonical_user_id(&target_id) {
            return Err(storage_unavailable());
        }
        let actor_user_id: Option<String> = row
            .try_get("actor_user_id")
            .map_err(|_| storage_unavailable())?;
        let reason: Option<String> = row.try_get("reason").map_err(|_| storage_unavailable())?;
        data.push(AuditBody {
            id: row.try_get("id").map_err(|_| storage_unavailable())?,
            event_kind,
            result,
            actor_user_id: actor_user_id.filter(|value| canonical_user_id(value)),
            target_kind: row
                .try_get("target_kind")
                .map_err(|_| storage_unavailable())?,
            target_id,
            reason: reason.filter(|value| !value.is_empty()),
            occurred_at,
        });
    }
    Ok(no_store(AuditPageBody { data, has_more }))
}

async fn ban_person(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let actor = installation_admin(&state, &parts).await?;
    let Path(person_id) = path.map_err(|_| {
        crate::http::error::ApiError::invalid_field("person_id", "Use a canonical UUID")
    })?;
    if !canonical_user_id(&person_id) {
        return Err(crate::http::error::ApiError::invalid_field(
            "person_id",
            "Use a canonical UUID",
        ));
    }
    let input: BanInput = read_json(body).await?;
    let changed = AccountAdministrationStore::new(state.database.pool().clone())
        .ban(&actor, &person_id, &input.reason, Utc::now())
        .await
        .map_err(account_error)?;
    Ok(no_store(ChangeBody {
        changed: changed == AccountStatusChange::Changed,
    }))
}

async fn unban_person(
    State(state): State<Arc<BrowserState>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let actor = installation_admin(&state, &parts).await?;
    let Path(person_id) = path.map_err(|_| {
        crate::http::error::ApiError::invalid_field("person_id", "Use a canonical UUID")
    })?;
    if !canonical_user_id(&person_id) {
        return Err(crate::http::error::ApiError::invalid_field(
            "person_id",
            "Use a canonical UUID",
        ));
    }
    let changed = AccountAdministrationStore::new(state.database.pool().clone())
        .unban(&actor, &person_id, Utc::now())
        .await
        .map_err(account_error)?;
    Ok(no_store(ChangeBody {
        changed: changed == AccountStatusChange::Changed,
    }))
}

async fn capture(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let report = crate::replication::capture(&state.database)
        .await
        .map_err(replication_error)?;
    Ok(no_store(CaptureBody {
        txid: report.txid,
        transactions: report.transactions,
        wal_frames: report.wal_frames,
        snapshot_created: report.snapshot_created,
        remote: report.remote.map(|remote| match remote {
            crate::replication::RemoteSync::Synced {
                txid,
                objects_confirmed,
                bytes_confirmed,
            } => RemoteSyncBody {
                status: "synced",
                txid: Some(txid),
                objects_confirmed: Some(objects_confirmed),
                bytes_confirmed: Some(bytes_confirmed),
            },
            crate::replication::RemoteSync::Failed => RemoteSyncBody {
                status: "failed",
                txid: None,
                objects_confirmed: None,
                bytes_confirmed: None,
            },
        }),
    }))
}

async fn restore_remote(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let report = crate::replication::restore_latest_from_remote(&state.database)
        .await
        .map_err(replication_error)?;
    Ok(no_store(RestoreBody {
        file_name: report.file_name,
    }))
}

async fn restore(
    State(state): State<Arc<BrowserState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    let _actor = installation_admin(&state, &parts).await?;
    let report = crate::replication::restore_latest(&state.database)
        .await
        .map_err(replication_error)?;
    Ok(no_store(RestoreBody {
        file_name: report.file_name,
    }))
}

type ApiError = crate::http::error::ApiError;

async fn installation_admin(
    state: &BrowserState,
    request: &axum::http::request::Parts,
) -> Result<String, ApiError> {
    let token = authenticated_token(state, request).await?;
    if request.uri.query().is_some() {
        return Err(malformed_query());
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
    let context = AccessStore::load_organization_context_in_transaction(
        &mut transaction,
        actor.user_id(),
        false,
    )
    .await
    .map_err(|_| storage_unavailable())?
    .ok_or_else(forbidden)?;
    transaction
        .commit()
        .await
        .map_err(|_| storage_unavailable())?;
    authorize(&context, AccessCapability::ConfigureInstallationSettings)
        .map_err(|_| forbidden())?;
    Ok(actor.user_id().to_owned())
}

async fn login_source(state: &BrowserState) -> Result<Option<&'static str>, ApiError> {
    let source: Option<String> =
        sqlx::query_scalar("SELECT source FROM installation_login WHERE singleton = 1")
            .fetch_optional(state.database.pool())
            .await
            .map_err(|_| storage_unavailable())?;
    match source.as_deref() {
        None => Ok(None),
        Some("puxiri") => Ok(Some("puxiri")),
        Some("muira") => Ok(Some("muira")),
        Some(_) => Err(storage_unavailable()),
    }
}

async fn read_json<T: for<'de> Deserialize<'de>>(body: axum::body::Body) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, 512 * 1024).await.map_err(|_| {
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

fn account_error(error: AccountAdministrationError) -> ApiError {
    match error {
        AccountAdministrationError::Forbidden => forbidden(),
        AccountAdministrationError::TargetNotFound => crate::http::error::ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "That account was not found",
            false,
        ),
        AccountAdministrationError::InvalidReason => crate::http::error::ApiError::invalid_field(
            "reason",
            "Use one single line, at most 500 characters",
        ),
        AccountAdministrationError::LastInstallationAdmin
        | AccountAdministrationError::LastOrganizationOwner
        | AccountAdministrationError::LastProjectManager => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "last_administrator",
            "Keep another active administrator before this change",
            false,
        ),
        AccountAdministrationError::TargetNotActive
        | AccountAdministrationError::TargetNotBanned
        | AccountAdministrationError::TargetNotDisabled
        | AccountAdministrationError::AnonymizedAccount => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "account_state",
            "That account cannot change in this way",
            false,
        ),
        AccountAdministrationError::CorruptState
        | AccountAdministrationError::Access(_)
        | AccountAdministrationError::Database(_)
        | AccountAdministrationError::Audit(_) => storage_unavailable(),
    }
}

fn replication_error(error: ReplicationError) -> ApiError {
    match error {
        ReplicationError::Busy => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "replication_busy",
            "Another capture already holds the database",
            false,
        ),
        ReplicationError::RemoteNotConfigured => crate::http::error::ApiError::new(
            StatusCode::CONFLICT,
            "replication_remote_not_configured",
            "No S3-compatible replication destination is configured",
            false,
        ),
        ReplicationError::Remote(error) => {
            tracing::warn!(%error, "remote replication request failed");
            crate::http::error::ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "replication_remote_unavailable",
                "The S3-compatible replication destination could not be read",
                true,
            )
        }
        ReplicationError::Unavailable(error) => {
            tracing::warn!(%error, "replication request failed");
            storage_unavailable()
        }
    }
}

fn forbidden() -> ApiError {
    crate::http::error::ApiError::new(
        StatusCode::FORBIDDEN,
        "permission_denied",
        "Installation administration is required",
        false,
    )
}

fn canonical_user_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
