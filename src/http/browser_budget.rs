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

pub(super) struct BrowserBudget {
    slots: Semaphore,
    empty_body_deadline: Duration,
}

impl Default for BrowserBudget {
    fn default() -> Self {
        Self {
            slots: Semaphore::new(8),
            empty_body_deadline: Duration::from_secs(1),
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
    if let Err(error) = empty_content_length(&parts.headers) {
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
    match tokio::time::timeout(budget.empty_body_deadline, to_bytes(body, 0)).await {
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
        Ok(Ok(_)) => next.run(Request::from_parts(parts, Body::empty())).await,
    }
}

enum FramingError {
    Malformed,
    NonEmpty,
}

fn empty_content_length(headers: &HeaderMap) -> Result<(), FramingError> {
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    let Some(length) = lengths.next() else {
        return Ok(());
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
    if parsed != Some(0) {
        return Err(FramingError::NonEmpty);
    }
    Ok(())
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
