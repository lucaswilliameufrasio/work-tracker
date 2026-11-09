use std::time::Duration;

use chrono::Utc;
use reqwest::redirect::Policy;
use serde_json::{Map, Value, json};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::{
    access::{AccessStore, ProjectRole},
    auth::{master_key::MasterKey, session::SessionPolicy, time::format_timestamp},
    issues::{
        IssueStore, NewIssue,
        comments::CommentStore,
        movement::{MoveIssue, SessionIssueMover},
        text::{ReplaceIssueText, SessionIssueTextWriter},
    },
    projects::{ProjectListOptions, ProjectStore},
};

const BODY_LIMIT: usize = 65_536;

#[derive(Clone)]
pub struct McpRuntime {
    pub pool: SqlitePool,
    pub policy: SessionPolicy,
    pub master_key: MasterKey,
    pub origin: String,
}

#[derive(Clone)]
pub struct McpCaller {
    pub user_id: String,
}

#[derive(Debug)]
pub enum McpAccessError {
    MalformedToken,
    LoginUnavailable,
    IdentityRejected,
    Upstream,
    InvitationRequired,
    GrantRequired,
    Database,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantScope {
    Read,
    Write,
}

impl GrantScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }

    fn allows_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

struct Grant {
    scope: GrantScope,
    all_projects: bool,
    projects: Vec<String>,
}

#[derive(Debug)]
pub enum GrantError {
    Invalid,
    ProjectUnavailable,
    Database,
}

#[derive(Debug)]
pub enum ToolFailure {
    Invalid(&'static str),
    Unavailable,
    Forbidden,
    Conflict(&'static str),
    UnavailableService,
}

pub struct IssuedGrant {
    pub scope: &'static str,
    pub all_projects: bool,
    pub projects: Vec<String>,
}

pub async fn authenticate_bearer(
    pool: &SqlitePool,
    token: &str,
) -> Result<McpCaller, McpAccessError> {
    if !acceptable_token(token) {
        return Err(McpAccessError::MalformedToken);
    }
    let row =
        sqlx::query("SELECT source, muira_issuer FROM installation_login WHERE singleton = 1")
            .fetch_optional(pool)
            .await
            .map_err(|_| McpAccessError::Database)?;
    let Some(row) = row else {
        return Err(McpAccessError::LoginUnavailable);
    };
    let source: String = row
        .try_get("source")
        .map_err(|_| McpAccessError::Database)?;
    let issuer: Option<String> = row
        .try_get("muira_issuer")
        .map_err(|_| McpAccessError::Database)?;
    if source != "muira" {
        return Err(McpAccessError::LoginUnavailable);
    }
    let Some(issuer) = issuer else {
        return Err(McpAccessError::LoginUnavailable);
    };
    let subject = userinfo_subject(&issuer, token).await?;
    let user_id: Option<String> = sqlx::query_scalar(
        "SELECT users.id \
         FROM auth_identities \
         JOIN users ON users.id = auth_identities.user_id \
         JOIN organization_members \
           ON organization_members.user_id = users.id \
          AND organization_members.organization_id = 1 \
         WHERE auth_identities.provider = 'muira' \
           AND auth_identities.issuer = ? \
           AND auth_identities.subject = ? \
           AND users.state = 'active'",
    )
    .bind(&issuer)
    .bind(&subject)
    .fetch_optional(pool)
    .await
    .map_err(|_| McpAccessError::Database)?;
    let Some(user_id) = user_id else {
        return Err(McpAccessError::InvitationRequired);
    };
    let granted: Option<i64> = sqlx::query_scalar("SELECT 1 FROM mcp_grants WHERE user_id = ?")
        .bind(&user_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| McpAccessError::Database)?;
    if granted.is_none() {
        return Err(McpAccessError::GrantRequired);
    }
    Ok(McpCaller { user_id })
}

pub async fn grant_status(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Option<IssuedGrant>, GrantError> {
    let Some(grant) = load_grant(pool, user_id).await? else {
        return Ok(None);
    };
    Ok(Some(IssuedGrant {
        scope: grant.scope.as_str(),
        all_projects: grant.all_projects,
        projects: grant.projects,
    }))
}

pub async fn replace_grant(
    pool: &SqlitePool,
    user_id: &str,
    scope: &str,
    all_projects: bool,
    projects: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<IssuedGrant, GrantError> {
    let Some(scope) = GrantScope::parse(scope) else {
        return Err(GrantError::Invalid);
    };
    if all_projects != projects.is_empty() {
        return Err(GrantError::Invalid);
    }
    let mut unique = projects.to_vec();
    unique.sort();
    for pair in unique.windows(2) {
        if pair[0] == pair[1] {
            return Err(GrantError::Invalid);
        }
    }
    for project_id in &unique {
        if Uuid::parse_str(project_id)
            .ok()
            .is_none_or(|id| id.to_string() != *project_id)
        {
            return Err(GrantError::Invalid);
        }
    }
    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|_| GrantError::Database)?;
    for project_id in &unique {
        let member: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM project_members \
             WHERE organization_id = 1 AND project_id = ? AND user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| GrantError::Database)?;
        if member.is_none() {
            return Err(GrantError::ProjectUnavailable);
        }
    }
    let timestamp = format_timestamp(now);
    sqlx::query("DELETE FROM mcp_grants WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| GrantError::Database)?;
    sqlx::query(
        "INSERT INTO mcp_grants (user_id, scope, all_projects, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(scope.as_str())
    .bind(i64::from(all_projects))
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut *transaction)
    .await
    .map_err(|_| GrantError::Database)?;
    for project_id in &unique {
        sqlx::query("INSERT INTO mcp_grant_projects (user_id, project_id) VALUES (?, ?)")
            .bind(user_id)
            .bind(project_id)
            .execute(&mut *transaction)
            .await
            .map_err(|_| GrantError::Database)?;
    }
    transaction
        .commit()
        .await
        .map_err(|_| GrantError::Database)?;
    Ok(IssuedGrant {
        scope: scope.as_str(),
        all_projects,
        projects: unique,
    })
}

pub async fn revoke_grant(pool: &SqlitePool, user_id: &str) -> Result<(), GrantError> {
    sqlx::query("DELETE FROM mcp_grants WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(|_| GrantError::Database)?;
    Ok(())
}

pub async fn call_tool(
    runtime: &McpRuntime,
    user_id: &str,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    match name {
        "list_projects" => list_projects(runtime, user_id, arguments).await,
        "get_project_schema" => project_schema(runtime, user_id, arguments).await,
        "search_issues" => search_issues(runtime, user_id, arguments).await,
        "get_issue" => get_issue(runtime, user_id, arguments).await,
        "create_issue" => create_issue(runtime, user_id, arguments).await,
        "update_issue" => update_issue(runtime, user_id, arguments).await,
        "move_issue" => move_issue(runtime, user_id, arguments).await,
        "add_comment" => add_comment(runtime, user_id, arguments).await,
        _ => Err(ToolFailure::Invalid("unknown tool")),
    }
}

pub fn tool_names() -> &'static [&'static str] {
    &[
        "list_projects",
        "get_project_schema",
        "search_issues",
        "get_issue",
        "create_issue",
        "update_issue",
        "move_issue",
        "add_comment",
    ]
}

async fn list_projects(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(arguments, &["after_project_key"])?;
    let grant = require_grant(&runtime.pool, user_id, false, None).await?;
    let after = optional_string(arguments, "after_project_key")?;
    let page = ProjectStore::new(runtime.pool.clone())
        .list_accessible(
            user_id,
            ProjectListOptions {
                after_project_key: after.as_deref(),
                page_size: 50,
                include_archived: false,
            },
        )
        .await
        .map_err(|_| ToolFailure::UnavailableService)?;
    let mut items = Vec::new();
    for project in page.items {
        if grant.all_projects || grant.projects.iter().any(|id| id == &project.id) {
            items.push(json!({
                "id": project.id,
                "project_key": project.project_key,
                "name": project.name,
                "role": role_name(project.role),
            }));
        }
    }
    Ok(json!({
        "projects": items,
        "next_after_project_key": page.next_after_project_key,
    }))
}

async fn project_schema(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(arguments, &["project_id"])?;
    let project_id = required_string(arguments, "project_id")?;
    require_grant(&runtime.pool, user_id, false, Some(&project_id)).await?;
    let mut transaction = runtime
        .pool
        .begin()
        .await
        .map_err(|_| ToolFailure::UnavailableService)?;
    let context = AccessStore::load_project_context_in_transaction(
        &mut transaction,
        user_id,
        &project_id,
        false,
    )
    .await
    .map_err(|_| ToolFailure::UnavailableService)?
    .ok_or(ToolFailure::Unavailable)?;
    if crate::access::authorize(
        &context,
        crate::access::AccessCapability::ReadProjectContent,
    )
    .is_err()
    {
        return Err(ToolFailure::Unavailable);
    }
    let status_rows = sqlx::query(
        "SELECT id, name, category, position, board_visible FROM statuses \
         WHERE project_id = ? AND archived_at IS NULL ORDER BY position, id",
    )
    .bind(&project_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| ToolFailure::UnavailableService)?;
    let type_rows = sqlx::query(
        "SELECT id, name FROM issue_types \
         WHERE project_id = ? AND archived_at IS NULL ORDER BY name, id",
    )
    .bind(&project_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| ToolFailure::UnavailableService)?;
    transaction
        .commit()
        .await
        .map_err(|_| ToolFailure::UnavailableService)?;
    let mut statuses = Vec::new();
    for row in status_rows {
        statuses.push(json!({
            "id": row.try_get::<String, _>("id").map_err(|_| ToolFailure::UnavailableService)?,
            "name": row.try_get::<String, _>("name").map_err(|_| ToolFailure::UnavailableService)?,
            "category": row.try_get::<String, _>("category").map_err(|_| ToolFailure::UnavailableService)?,
            "position": row.try_get::<i64, _>("position").map_err(|_| ToolFailure::UnavailableService)?,
            "board_visible": row.try_get::<i64, _>("board_visible").map_err(|_| ToolFailure::UnavailableService)? == 1,
        }));
    }
    let mut issue_types = Vec::new();
    for row in type_rows {
        issue_types.push(json!({
            "id": row.try_get::<String, _>("id").map_err(|_| ToolFailure::UnavailableService)?,
            "name": row.try_get::<String, _>("name").map_err(|_| ToolFailure::UnavailableService)?,
        }));
    }
    Ok(json!({
        "project_id": project_id,
        "statuses": statuses,
        "issue_types": issue_types,
    }))
}

async fn search_issues(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(arguments, &["project_id", "query"])?;
    let project_id = required_string(arguments, "project_id")?;
    let query = required_string(arguments, "query")?;
    require_grant(&runtime.pool, user_id, false, Some(&project_id)).await?;
    let page = IssueStore::new(runtime.pool.clone())
        .search_project_issues(user_id, &project_id, &query, None, 50)
        .await
        .map_err(read_failure)?;
    let mut items = Vec::new();
    for issue in page.items {
        items.push(summary_json(&issue));
    }
    Ok(json!({"issues": items}))
}

async fn get_issue(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(arguments, &["project_id", "issue_number"])?;
    let project_id = required_string(arguments, "project_id")?;
    let issue_number = required_number(arguments, "issue_number")?;
    require_grant(&runtime.pool, user_id, false, Some(&project_id)).await?;
    let details = IssueStore::new(runtime.pool.clone())
        .get_project_issue(user_id, &project_id, issue_number)
        .await
        .map_err(read_failure)?;
    let mut value = record_json(&details.issue);
    value["project_id"] = json!(project_id);
    Ok(value)
}

async fn create_issue(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(arguments, &["project_id", "title", "description"])?;
    let project_id = required_string(arguments, "project_id")?;
    let title = required_string(arguments, "title")?;
    let description = optional_string(arguments, "description")?;
    require_grant(&runtime.pool, user_id, true, Some(&project_id)).await?;
    let created = IssueStore::new(runtime.pool.clone())
        .create(
            user_id,
            &project_id,
            NewIssue {
                title: &title,
                description: description.as_deref(),
                assignee_user_id: None,
            },
            Utc::now(),
        )
        .await
        .map_err(|error| match error {
            crate::issues::IssueCreationError::InvalidTitle
            | crate::issues::IssueCreationError::DescriptionTooLong => {
                ToolFailure::Invalid("the issue text was rejected")
            }
            crate::issues::IssueCreationError::Forbidden => ToolFailure::Forbidden,
            crate::issues::IssueCreationError::Database(_)
            | crate::issues::IssueCreationError::AccessState(_) => ToolFailure::UnavailableService,
            _ => ToolFailure::Unavailable,
        })?;
    Ok(record_json(&created))
}

async fn update_issue(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(
        arguments,
        &[
            "project_id",
            "issue_number",
            "expected_version",
            "title",
            "description",
            "idempotency_key",
        ],
    )?;
    let project_id = required_string(arguments, "project_id")?;
    let issue_number = required_number(arguments, "issue_number")?;
    let expected_version = required_number(arguments, "expected_version")?;
    let title = required_string(arguments, "title")?;
    let description = optional_string(arguments, "description")?;
    let key = required_string(arguments, "idempotency_key")?;
    require_grant(&runtime.pool, user_id, true, Some(&project_id)).await?;
    let outcome = SessionIssueTextWriter::new(
        runtime.pool.clone(),
        runtime.policy,
        runtime.master_key.clone(),
    )
    .replace_by_number_for_user(
        user_id,
        &project_id,
        issue_number,
        &key,
        ReplaceIssueText {
            expected_version,
            title: &title,
            description: description.as_deref(),
        },
        Utc::now(),
    )
    .await
    .map_err(text_failure)?;
    Ok(json!({
        "issue_number": outcome.receipt.issue_number,
        "issue_key": outcome.receipt.issue_key,
        "version": outcome.receipt.result_version,
        "changed": outcome.receipt.changed,
        "replayed": outcome.replayed,
    }))
}

async fn move_issue(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(
        arguments,
        &[
            "project_id",
            "issue_number",
            "expected_version",
            "status_id",
            "idempotency_key",
        ],
    )?;
    let project_id = required_string(arguments, "project_id")?;
    let issue_number = required_number(arguments, "issue_number")?;
    let expected_version = required_number(arguments, "expected_version")?;
    let status_id = required_string(arguments, "status_id")?;
    let key = required_string(arguments, "idempotency_key")?;
    require_grant(&runtime.pool, user_id, true, Some(&project_id)).await?;
    let outcome = SessionIssueMover::new(
        runtime.pool.clone(),
        runtime.policy,
        runtime.master_key.clone(),
    )
    .move_issue_for_user(
        user_id,
        &project_id,
        issue_number,
        &key,
        MoveIssue {
            expected_version,
            target_status_id: &status_id,
            previous_issue_id: None,
            next_issue_id: None,
        },
        Utc::now(),
    )
    .await
    .map_err(move_failure)?;
    Ok(json!({
        "issue_number": outcome.receipt.issue_number,
        "issue_key": outcome.receipt.issue_key,
        "status_id": outcome.receipt.status_id,
        "version": outcome.receipt.result_version,
        "changed": outcome.receipt.changed,
        "replayed": outcome.replayed,
    }))
}

async fn add_comment(
    runtime: &McpRuntime,
    user_id: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    reject_unknown(
        arguments,
        &["project_id", "issue_number", "body", "idempotency_key"],
    )?;
    let project_id = required_string(arguments, "project_id")?;
    let issue_number = required_number(arguments, "issue_number")?;
    let body = required_string(arguments, "body")?;
    let key = required_string(arguments, "idempotency_key")?;
    require_grant(&runtime.pool, user_id, true, Some(&project_id)).await?;
    let receipt = CommentStore::new(runtime.pool.clone())
        .create(user_id, &project_id, issue_number, &key, &body, Utc::now())
        .await
        .map_err(|error| match error {
            crate::issues::comments::CommentError::InvalidBody
            | crate::issues::comments::CommentError::InvalidIdempotencyKey => {
                ToolFailure::Invalid("the comment was rejected")
            }
            crate::issues::comments::CommentError::Forbidden => ToolFailure::Forbidden,
            crate::issues::comments::CommentError::IdempotencyConflict
            | crate::issues::comments::CommentError::VersionConflict => {
                ToolFailure::Conflict("the comment was not saved")
            }
            crate::issues::comments::CommentError::Database(_)
            | crate::issues::comments::CommentError::Access(_) => ToolFailure::UnavailableService,
            crate::issues::comments::CommentError::Unavailable => ToolFailure::Unavailable,
        })?;
    Ok(json!({
        "comment_id": receipt.comment_id,
        "changed": receipt.changed,
        "replayed": receipt.replayed,
    }))
}

async fn require_grant(
    pool: &SqlitePool,
    user_id: &str,
    write: bool,
    project_id: Option<&str>,
) -> Result<Grant, ToolFailure> {
    let grant = load_grant(pool, user_id)
        .await
        .map_err(|_| ToolFailure::UnavailableService)?
        .ok_or(ToolFailure::Forbidden)?;
    if write && !grant.scope.allows_write() {
        return Err(ToolFailure::Forbidden);
    }
    if let Some(project_id) = project_id
        && !grant.all_projects
        && !grant.projects.iter().any(|id| id == project_id)
    {
        return Err(ToolFailure::Forbidden);
    }
    Ok(grant)
}

async fn load_grant(pool: &SqlitePool, user_id: &str) -> Result<Option<Grant>, GrantError> {
    let row = sqlx::query("SELECT scope, all_projects FROM mcp_grants WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| GrantError::Database)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let scope = GrantScope::parse(
        row.try_get::<String, _>("scope")
            .map_err(|_| GrantError::Database)?
            .as_str(),
    )
    .ok_or(GrantError::Database)?;
    let all_projects = match row
        .try_get::<i64, _>("all_projects")
        .map_err(|_| GrantError::Database)?
    {
        0 => false,
        1 => true,
        _ => return Err(GrantError::Database),
    };
    let projects = sqlx::query_scalar(
        "SELECT project_id FROM mcp_grant_projects WHERE user_id = ? ORDER BY project_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|_| GrantError::Database)?;
    Ok(Some(Grant {
        scope,
        all_projects,
        projects,
    }))
}

async fn userinfo_subject(issuer: &str, token: &str) -> Result<String, McpAccessError> {
    let origin = allowed_issuer(issuer).ok_or(McpAccessError::LoginUnavailable)?;
    let url = format!("{origin}/oauth/userinfo");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| McpAccessError::Upstream)?;
    let response = client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|_| McpAccessError::Upstream)?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| McpAccessError::Upstream)?
    {
        if bytes.len().saturating_add(chunk.len()) > BODY_LIMIT {
            return Err(McpAccessError::Upstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    if status == 401 || status == 403 {
        return Err(McpAccessError::IdentityRejected);
    }
    if status != 200 {
        return Err(McpAccessError::Upstream);
    }
    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|_| McpAccessError::IdentityRejected)?;
    let subject = payload
        .get("sub")
        .and_then(Value::as_str)
        .ok_or(McpAccessError::IdentityRejected)?;
    if subject.is_empty() || subject.len() > 256 || subject.chars().any(char::is_control) {
        return Err(McpAccessError::IdentityRejected);
    }
    Ok(subject.to_owned())
}

fn allowed_issuer(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if url.path() != "/" && !url.path().is_empty() {
        return None;
    }
    let host = url.host_str()?;
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme_ok {
        return None;
    }
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

fn acceptable_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 4096
        && token
            .chars()
            .all(|character| !character.is_whitespace() && !character.is_control())
}

fn reject_unknown(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolFailure> {
    for key in arguments.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ToolFailure::Invalid("unexpected argument"));
        }
    }
    Ok(())
}

fn required_string(arguments: &Map<String, Value>, name: &str) -> Result<String, ToolFailure> {
    match arguments.get(name) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(ToolFailure::Invalid("a required argument is missing")),
    }
}

fn optional_string(
    arguments: &Map<String, Value>,
    name: &str,
) -> Result<Option<String>, ToolFailure> {
    match arguments.get(name) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(ToolFailure::Invalid("an optional argument is invalid")),
    }
}

fn required_number(arguments: &Map<String, Value>, name: &str) -> Result<i64, ToolFailure> {
    match arguments.get(name) {
        Some(Value::Number(value)) => value
            .as_i64()
            .filter(|number| *number > 0)
            .ok_or(ToolFailure::Invalid("a number argument is invalid")),
        _ => Err(ToolFailure::Invalid("a number argument is missing")),
    }
}

fn read_failure(error: crate::issues::IssueReadError) -> ToolFailure {
    match error {
        crate::issues::IssueReadError::InvalidIssueNumber
        | crate::issues::IssueReadError::InvalidSearchQuery
        | crate::issues::IssueReadError::InvalidPageSize
        | crate::issues::IssueReadError::InvalidCursor
        | crate::issues::IssueReadError::InvalidSearchCursor => {
            ToolFailure::Invalid("the query was rejected")
        }
        crate::issues::IssueReadError::Database(_)
        | crate::issues::IssueReadError::AccessState(_)
        | crate::issues::IssueReadError::CorruptData => ToolFailure::UnavailableService,
        _ => ToolFailure::Unavailable,
    }
}

fn text_failure(error: crate::issues::text::IssueTextError) -> ToolFailure {
    match error {
        crate::issues::text::IssueTextError::InvalidIdempotencyKey
        | crate::issues::text::IssueTextError::InvalidVersion
        | crate::issues::text::IssueTextError::Validation(_) => {
            ToolFailure::Invalid("the issue text was rejected")
        }
        crate::issues::text::IssueTextError::VersionConflict
        | crate::issues::text::IssueTextError::IdempotencyConflict => {
            ToolFailure::Conflict("the issue text was not replaced")
        }
        crate::issues::text::IssueTextError::Database(_)
        | crate::issues::text::IssueTextError::Access(_)
        | crate::issues::text::IssueTextError::Session(_)
        | crate::issues::text::IssueTextError::Encoding(_) => ToolFailure::UnavailableService,
        _ => ToolFailure::Unavailable,
    }
}

fn move_failure(error: crate::issues::movement::IssueMoveError) -> ToolFailure {
    match error {
        crate::issues::movement::IssueMoveError::InvalidIdempotencyKey
        | crate::issues::movement::IssueMoveError::InvalidVersion => {
            ToolFailure::Invalid("the move was rejected")
        }
        crate::issues::movement::IssueMoveError::VersionConflict
        | crate::issues::movement::IssueMoveError::OrderConflict
        | crate::issues::movement::IssueMoveError::IdempotencyConflict => {
            ToolFailure::Conflict("the issue was not moved")
        }
        crate::issues::movement::IssueMoveError::Database(_)
        | crate::issues::movement::IssueMoveError::Access(_)
        | crate::issues::movement::IssueMoveError::Session(_)
        | crate::issues::movement::IssueMoveError::Encoding(_) => ToolFailure::UnavailableService,
        _ => ToolFailure::Unavailable,
    }
}

fn summary_json(issue: &crate::issues::IssueSummary) -> Value {
    json!({
        "id": issue.id,
        "issue_number": issue.issue_number,
        "issue_key": issue.issue_key,
        "title": issue.title,
        "status_id": issue.status_id,
        "version": issue.version,
    })
}

fn record_json(issue: &crate::issues::IssueRecord) -> Value {
    let mut value = json!({
        "id": issue.id,
        "issue_number": issue.issue_number,
        "issue_key": issue.issue_key,
        "title": issue.title,
        "status_id": issue.status_id,
        "version": issue.version,
    });
    if let Some(description) = &issue.description {
        value["description"] = json!(description);
    }
    value
}

fn role_name(role: ProjectRole) -> &'static str {
    match role {
        ProjectRole::Manager => "manager",
        ProjectRole::Member => "member",
        ProjectRole::Viewer => "viewer",
    }
}
