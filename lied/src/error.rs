//! The single top-level application error type.
//!
//! `AppError` implements [`axum::response::IntoResponse`]. For the `/v1`
//! JSON tree it serializes as RFC 7807 Problem Details
//! (`application/problem+json`). The HTMX `/admin` tree is expected to
//! catch its own errors and render HTML fragments instead — this type is
//! the `/v1` contract.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("resource not found")]
    NotFound,

    #[error("precondition failed")]
    PreconditionFailed,

    #[error("validation failed")]
    Validation(garde::Report),

    #[error("database error")]
    Database(#[from] sqlx::Error),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),

    #[error("authentication required")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("too many requests")]
    TooManyRequests,

    /// A write would violate a business invariant that isn't a simple
    /// validation failure on the request body — e.g. demoting/removing the
    /// last `owner` of an organization (CLAUDE.md: "every org keeps >=1
    /// owner; demoting the last owner is rejected"). `409 Conflict` (rather
    /// than `422`) was chosen because the request is syntactically and
    /// semantically valid on its own; it only conflicts with the *current
    /// state* of the membership set — the textbook 409 case per RFC 9110
    /// ("the request could not be completed due to a conflict with the
    /// current state of the target resource").
    #[error("conflict: {0}")]
    Conflict(String),
}

/// RFC 7807 Problem Details body.
#[derive(Serialize)]
struct ProblemDetails {
    #[serde(rename = "type")]
    type_: String,
    title: String,
    status: u16,
    detail: String,
    instance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<serde_json::Value>,
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            AppError::Validation(_) => StatusCode::BAD_REQUEST,
            AppError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            AppError::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    fn type_uri(&self) -> &'static str {
        match self {
            AppError::NotFound => "https://lied/errors/not-found",
            AppError::PreconditionFailed => "https://lied/errors/precondition-failed",
            AppError::Validation(_) => "https://lied/errors/validation-failed",
            AppError::Database(_) => "https://lied/errors/internal",
            AppError::Internal(_) => "https://lied/errors/internal",
            AppError::Unauthorized => "https://lied/errors/unauthorized",
            AppError::Forbidden => "https://lied/errors/forbidden",
            AppError::TooManyRequests => "https://lied/errors/too-many-requests",
            AppError::Conflict(_) => "https://lied/errors/conflict",
        }
    }

    fn title(&self) -> &'static str {
        match self {
            AppError::NotFound => "Not Found",
            AppError::PreconditionFailed => "Precondition Failed",
            AppError::Validation(_) => "Validation Failed",
            AppError::Database(_) => "Internal Server Error",
            AppError::Internal(_) => "Internal Server Error",
            AppError::Unauthorized => "Unauthorized",
            AppError::Forbidden => "Forbidden",
            AppError::TooManyRequests => "Too Many Requests",
            AppError::Conflict(_) => "Conflict",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();

        // Log server-side errors with full detail; client errors at a lower level.
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::warn!(error = %self, "request rejected");
        }

        let errors = match &self {
            AppError::Validation(report) => Some(garde_report_to_json(report)),
            _ => None,
        };

        // request-ID correlation: `instance` is populated by middleware in the
        // future; left empty here since this skeleton has no request-ID
        // extension wired into AppError yet.
        let body = ProblemDetails {
            type_: self.type_uri().to_string(),
            title: self.title().to_string(),
            status: status.as_u16(),
            detail: self.to_string(),
            instance: String::new(),
            errors,
        };

        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

fn garde_report_to_json(report: &garde::Report) -> serde_json::Value {
    let mut errors = serde_json::Map::new();
    for (path, error) in report.iter() {
        let field = path.to_string();
        let entry = errors
            .entry(field)
            .or_insert_with(|| serde_json::Value::Array(vec![]));
        if let serde_json::Value::Array(messages) = entry {
            messages.push(serde_json::Value::String(error.to_string()));
        }
    }
    serde_json::Value::Object(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn not_found_serializes_as_problem_details() {
        let response = AppError::NotFound.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/problem+json"
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 404);
        assert_eq!(json["type"], "https://lied/errors/not-found");
        assert!(json["title"].is_string());
        assert!(json["detail"].is_string());
    }

    #[tokio::test]
    async fn precondition_failed_maps_to_412() {
        let response = AppError::PreconditionFailed.into_response();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn conflict_maps_to_409() {
        let response = AppError::Conflict("last owner".to_string()).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
