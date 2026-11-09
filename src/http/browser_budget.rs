use std::{error::Error as _, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::Semaphore;

use super::error::ApiError;

const MAX_URI_BYTES: usize = 2 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_ENTRIES: usize = 64;
const MAX_JSON_BODY: usize = 512 * 1024;

pub(super) struct BrowserBudget {
    slots: Semaphore,
    body_deadline: Duration,
    security: Option<super::browser_session::BrowserSessionSecurity>,
}

impl Default for BrowserBudget {
    fn default() -> Self {
        Self {
            slots: Semaphore::new(8),
            body_deadline: Duration::from_secs(1),
            security: None,
        }
    }
}

impl BrowserBudget {
    pub(super) fn with_security(security: super::browser_session::BrowserSessionSecurity) -> Self {
        Self {
            security: Some(security),
            ..Self::default()
        }
    }
}

pub(super) async fn admit(
    State(budget): State<Arc<BrowserBudget>>,
    request: Request,
    next: Next,
) -> Response {
    let head = request.method() == axum::http::Method::HEAD;
    let mut response = admit_request(&budget, request, next).await;
    if head {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn admit_request(budget: &BrowserBudget, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    if parts
        .uri
        .path_and_query()
        .is_some_and(|uri| uri.as_str().len() > MAX_URI_BYTES)
    {
        return rejection(
            StatusCode::URI_TOO_LONG,
            "uri_too_long",
            "Request URI is too long",
            false,
        );
    }
    let mut bytes = 0_usize;
    for (index, (name, value)) in parts.headers.iter().enumerate() {
        bytes = bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len());
        if index >= MAX_HEADER_ENTRIES || bytes > MAX_HEADER_BYTES {
            return rejection(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "headers_too_large",
                "Request headers are too large",
                false,
            );
        }
    }
    let admission = parts
        .extensions
        .get::<axum::extract::MatchedPath>()
        .map(|path| json_admission(&parts.method, path.as_str()))
        .unwrap_or(JsonAdmission::None);
    let anonymous_json = match admission {
        JsonAdmission::None => None,
        JsonAdmission::Anonymous => Some(true),
        JsonAdmission::Authenticated => Some(false),
    };
    if anonymous_json.is_some() && budget.security.is_none() {
        return rejection(
            StatusCode::FORBIDDEN,
            "request_origin_denied",
            "Request origin denied",
            false,
        );
    }
    let limit = if anonymous_json.is_none() {
        0
    } else {
        MAX_JSON_BODY
    };
    let declared = match content_length(&parts.headers, limit) {
        Err(error) => {
            return match error {
                FramingError::Malformed => rejection(
                    StatusCode::BAD_REQUEST,
                    "malformed_request",
                    "Request framing is invalid",
                    false,
                ),
                FramingError::NonEmpty => rejection(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "Request body is not supported",
                    false,
                ),
            };
        }
        Ok(declared) => declared,
    };
    if let Some(anonymous) = anonymous_json {
        let Some(security) = &budget.security else {
            return rejection(
                StatusCode::FORBIDDEN,
                "request_origin_denied",
                "Request origin denied",
                false,
            );
        };
        let authorized = if anonymous {
            security.authorize_anonymous_mutation(&parts.method, &parts.uri, &parts.headers)
        } else {
            security
                .authenticate_request(&parts.method, &parts.uri, &parts.headers)
                .map(|_| ())
        };
        if let Err(error) = authorized {
            return super::browser_api::transport_error(error).into_response();
        }
        if !json_content_type(&parts.headers)
            || parts.headers.contains_key(header::CONTENT_ENCODING)
        {
            return rejection(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Use an uncompressed application/json request",
                false,
            );
        }
    }
    let Ok(_permit) = budget.slots.try_acquire() else {
        let mut response = rejection(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_busy",
            "Service temporarily busy",
            true,
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    match tokio::time::timeout(budget.body_deadline, to_bytes(body, limit)).await {
        Err(_) => rejection(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "Request body did not finish in time",
            true,
        ),
        Ok(Err(error)) => {
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                rejection(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "Request body is not supported",
                    false,
                )
            } else {
                rejection(
                    StatusCode::BAD_REQUEST,
                    "malformed_request",
                    "Request body could not be read",
                    false,
                )
            }
        }
        Ok(Ok(bytes)) => {
            if declared.is_some_and(|declared| declared != bytes.len() as u64) {
                return rejection(
                    StatusCode::BAD_REQUEST,
                    "malformed_request",
                    "Request framing is invalid",
                    false,
                );
            }
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
    }
}

enum JsonAdmission {
    None,
    Authenticated,
    Anonymous,
}

fn json_admission(method: &axum::http::Method, path: &str) -> JsonAdmission {
    if *method == axum::http::Method::POST
        && matches!(
            path,
            "/api/v1/setup/puxiri"
                | "/api/v1/setup/muira"
                | "/api/v1/auth/muira/login"
                | "/api/v1/auth/puxiri/login"
        )
    {
        return JsonAdmission::Anonymous;
    }
    if matches!(
        (method, path),
        (&axum::http::Method::POST, "/api/v1/projects")
            | (
                &axum::http::Method::POST,
                "/api/v1/projects/{project_id}/issues"
            )
            | (
                &axum::http::Method::PUT,
                "/api/v1/projects/{project_id}/issues/{issue_number}/text"
            )
            | (
                &axum::http::Method::POST,
                "/api/v1/projects/{project_id}/issues/{issue_number}/move"
            )
            | (
                &axum::http::Method::PUT,
                "/api/v1/projects/{project_id}/issues/{issue_number}/priority"
            )
            | (
                &axum::http::Method::PUT,
                "/api/v1/projects/{project_id}/issues/{issue_number}/due-date"
            )
            | (
                &axum::http::Method::PUT,
                "/api/v1/projects/{project_id}/issues/{issue_number}/assignee"
            )
            | (
                &axum::http::Method::POST,
                "/api/v1/projects/{project_id}/issues/{issue_number}/comments"
            )
            | (
                &axum::http::Method::PUT,
                "/api/v1/projects/{project_id}/issues/{issue_number}/comments/{comment_id}"
            )
            | (&axum::http::Method::POST, "/api/v1/invitations")
            | (&axum::http::Method::PUT, "/api/v1/mail/sender")
            | (&axum::http::Method::PUT, "/api/v1/mcp/grant")
            | (
                &axum::http::Method::POST,
                "/api/v1/admin/people/{person_id}/ban"
            )
            | (
                &axum::http::Method::POST,
                "/api/v1/admin/people/{person_id}/unban"
            )
            | (
                &axum::http::Method::POST,
                "/api/v1/admin/replication/capture"
            )
            | (
                &axum::http::Method::POST,
                "/api/v1/admin/replication/restore"
            )
    ) {
        return JsonAdmission::Authenticated;
    }
    JsonAdmission::None
}

enum FramingError {
    Malformed,
    NonEmpty,
}

fn content_length(headers: &HeaderMap, limit: usize) -> Result<Option<u64>, FramingError> {
    let mut transfers = headers.get_all(header::TRANSFER_ENCODING).iter();
    if let Some(transfer) = transfers.next()
        && (transfers.next().is_some()
            || !transfer
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("chunked")))
    {
        return Err(FramingError::Malformed);
    }
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    let Some(length) = lengths.next() else {
        return Ok(None);
    };
    let parsed = length
        .to_str()
        .ok()
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok());
    if lengths.next().is_some()
        || headers.contains_key(header::TRANSFER_ENCODING)
        || parsed.is_none()
    {
        return Err(FramingError::Malformed);
    }
    if parsed.is_some_and(|length| length > limit as u64) {
        return Err(FramingError::NonEmpty);
    }
    Ok(parsed)
}

fn json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    if let Some(parameter) = parts.next() {
        let parameter = parameter.trim();
        if !parameter.eq_ignore_ascii_case("charset=utf-8")
            && !parameter.eq_ignore_ascii_case("charset=\"utf-8\"")
        {
            return false;
        }
    }
    parts.next().is_none()
}

fn rejection(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retryable: bool,
) -> Response {
    ApiError::new(status, code, message, retryable).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, middleware, routing::get};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    fn app(budget: Arc<BrowserBudget>, entered: Arc<Notify>, release: Arc<Notify>) -> Router {
        Router::new()
            .route(
                "/hold",
                get(move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        StatusCode::OK
                    }
                }),
            )
            .route("/ok", get(|| async { StatusCode::OK }))
            .route("/fail", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(middleware::from_fn_with_state(budget, admit))
    }

    fn request(path: &str) -> Request {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn saturation_rejects_without_waiting_and_releases_on_cancel_error_and_success() {
        let budget = Arc::new(BrowserBudget {
            slots: Semaphore::new(1),
            ..BrowserBudget::default()
        });
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = app(budget.clone(), entered.clone(), release.clone());
        let held = tokio::spawn(app.clone().oneshot(request("/hold")));
        entered.notified().await;
        assert_eq!(budget.slots.available_permits(), 0);
        let rejected = app.clone().oneshot(request("/ok")).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejected.headers()[header::RETRY_AFTER], "1");
        held.abort();
        assert!(held.await.unwrap_err().is_cancelled());
        assert_eq!(budget.slots.available_permits(), 1);
        for (path, expected) in [
            ("/fail", StatusCode::INTERNAL_SERVER_ERROR),
            ("/ok", StatusCode::OK),
        ] {
            assert_eq!(
                app.clone().oneshot(request(path)).await.unwrap().status(),
                expected
            );
            assert_eq!(budget.slots.available_permits(), 1);
        }
        let held = tokio::spawn(app.clone().oneshot(request("/hold")));
        entered.notified().await;
        release.notify_one();
        assert_eq!(held.await.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(budget.slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn envelope_exact_boundaries_and_empty_body_release_capacity() {
        let budget = Arc::new(BrowserBudget::default());
        let app = app(
            budget.clone(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        );
        for (count, expected) in [
            (64, StatusCode::OK),
            (65, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE),
        ] {
            let mut req = request("/ok");
            for index in 0..count {
                req.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(format!("x-{index}").as_bytes()).unwrap(),
                    HeaderValue::from_static("v"),
                );
            }
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
        }
        for (extra, expected) in [
            (0, StatusCode::OK),
            (1, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE),
        ] {
            let mut req = request("/ok");
            req.headers_mut().insert(
                "x",
                HeaderValue::from_str(&"a".repeat(MAX_HEADER_BYTES - 1 + extra)).unwrap(),
            );
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
        }
        for (extra, expected) in [(0, StatusCode::OK), (1, StatusCode::URI_TOO_LONG)] {
            let req = request(&format!("/ok?q={}", "a".repeat(MAX_URI_BYTES - 6 + extra)));
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
        }
        for (body, expected) in [
            (Body::empty(), StatusCode::OK),
            (Body::from("x"), StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let req = Request::builder().uri("/ok").body(body).unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
        }
        assert_eq!(budget.slots.available_permits(), 8);
    }
}
