use std::collections::BTreeMap;

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    request_id: String,
    field_errors: BTreeMap<&'static str, &'static str>,
    retryable: bool,
}

pub(super) struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    pub(super) fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                code,
                message,
                request_id: Uuid::now_v7().to_string(),
                field_errors: BTreeMap::new(),
                retryable,
            },
        }
    }

    pub(super) fn invalid_field(field: &'static str, message: &'static str) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_params",
            "Request parameters are invalid",
            false,
        );
        error.body.field_errors.insert(field, message);
        error
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CACHE_CONTROL, "no-store")],
            Json(self.body),
        )
            .into_response()
    }
}
