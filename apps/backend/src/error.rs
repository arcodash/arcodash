//! `ApiError` → error JSON: `{ "message": "...", "error_code"? }`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{Map, Value};

/// A REST API error.
///
/// Carries the HTTP status to return plus a user-facing `message` and an optional machine-readable
/// `error_code`. `extra` merges additional top-level fields into the JSON body (e.g. a conflict's
/// `working_version`) without disturbing the `message`/`error_code` shape every other error uses.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub error_code: Option<String>,
    pub extra: Option<Map<String, Value>>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            error_code: None,
            extra: None,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// A 400 carrying a machine-readable `error_code`, for a client that needs to branch on
    /// something more specific than "bad request" without string-matching `message`.
    pub fn bad_request_with_code(
        message: impl Into<String>,
        error_code: impl Into<String>,
    ) -> Self {
        Self::bad_request(message).with_code(error_code)
    }

    /// A 429 carrying a machine-readable `error_code`.
    pub fn too_many_requests_with_code(
        message: impl Into<String>,
        error_code: impl Into<String>,
    ) -> Self {
        Self::too_many_requests(message).with_code(error_code)
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// A 409 with a machine-readable `error_code` the client can branch on.
    pub fn conflict_with_code(message: impl Into<String>, error_code: impl Into<String>) -> Self {
        Self::conflict(message).with_code(error_code)
    }

    /// A 409 whose body carries extra machine-readable fields (e.g. `working_version`) beyond
    /// `message`/`error_code`, so a client can branch without string-matching the message.
    pub fn conflict_with(message: impl Into<String>, extra: Value) -> Self {
        Self::conflict_with_code(message, "conflict").with_extra(extra)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// A 402 with extra body fields, like `conflict_with`; clients branch on `error_code`.
    pub fn payment_required_with(
        message: impl Into<String>,
        error_code: impl Into<String>,
        extra: Value,
    ) -> Self {
        Self::new(StatusCode::PAYMENT_REQUIRED, message)
            .with_code(error_code)
            .with_extra(extra)
    }

    fn with_code(self, error_code: impl Into<String>) -> Self {
        Self {
            error_code: Some(error_code.into()),
            ..self
        }
    }

    fn with_extra(self, extra: Value) -> Self {
        Self {
            extra: match extra {
                Value::Object(map) => Some(map),
                _ => None,
            },
            ..self
        }
    }
}

#[derive(Serialize)]
struct ApiErrorBody {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    extra: Option<Map<String, Value>>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Every 5xx gets logged with its real detail — the client never sees this message, so
        // the log is the only place it survives for debugging.
        if self.status.is_server_error() {
            tracing::error!(
                status = %self.status,
                error_code = self.error_code.as_deref().unwrap_or_default(),
                message = %self.message,
                "internal error response"
            );
        }
        // Only a bare 500 is genericized — a 503 like "LDAP is not configured" is a deliberate,
        // actionable operator message, not a leak, and must survive untouched. Matches
        // `From<DbErr>`/`From<anyhow::Error>` below, which already return this exact string.
        let message = if self.status == StatusCode::INTERNAL_SERVER_ERROR {
            "internal server error".to_string()
        } else {
            self.message
        };
        let body = ApiErrorBody {
            message,
            error_code: self.error_code,
            extra: self.extra,
        };
        (self.status, Json(body)).into_response()
    }
}

/// Postgres unique-violation (SQLSTATE 23505) detection via the error string — sea-orm doesn't
/// expose the SQLSTATE code directly through `DbErr`.
#[must_use]
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    let msg = err.to_string();
    msg.contains("duplicate key") || msg.contains("23505") || msg.contains("unique constraint")
}

impl From<sea_orm::DbErr> for ApiError {
    fn from(err: sea_orm::DbErr) -> Self {
        tracing::error!(error = %err, "database error");
        Self::internal("internal server error")
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::internal("internal server error")
    }
}

impl From<arcodash_core::ai::narrative::NarrativeError> for ApiError {
    fn from(err: arcodash_core::ai::narrative::NarrativeError) -> Self {
        use arcodash_core::ai::narrative::NarrativeError;
        match err {
            NarrativeError::Db(e) => e.into(),
            NarrativeError::Storage(e) => e.into(),
        }
    }
}

impl From<arcodash_core::org_storage::StorageError> for ApiError {
    fn from(err: arcodash_core::org_storage::StorageError) -> Self {
        use arcodash_core::org_storage::StorageError;
        match err {
            StorageError::Unreachable { .. } => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Can't reach the storage bucket",
            )
            .with_code("storage_unreachable"),
            StorageError::Switching => {
                Self::conflict_with_code("Storage switch in progress", "storage_switching")
            }
            StorageError::NotFound => Self::new(StatusCode::NOT_FOUND, "result not found"),
            StorageError::Other(e) => {
                tracing::error!(error = %e, "storage error");
                Self::internal("internal server error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(status: StatusCode, message: &str) -> serde_json::Value {
        let response = ApiError::new(status, message).into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn storage_error_maps_to_api_status() {
        use arcodash_core::org_storage::StorageError;

        let unreachable = ApiError::from(StorageError::Unreachable {
            detail: "AccessDenied".into(),
        });
        assert_eq!(unreachable.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            unreachable.error_code.as_deref(),
            Some("storage_unreachable")
        );

        let switching = ApiError::from(StorageError::Switching);
        assert_eq!(switching.status, StatusCode::CONFLICT);
        assert_eq!(switching.error_code.as_deref(), Some("storage_switching"));

        let other = ApiError::from(StorageError::Other(anyhow::anyhow!("x-secret-detail")));
        assert_eq!(other.status, StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(other.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("x-secret-detail"));
    }

    #[tokio::test]
    async fn a_500_renders_the_generic_body_not_the_real_message() {
        let body = body_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "db host db-1.internal down",
        )
        .await;
        assert_eq!(body["message"], "internal server error");
    }

    #[tokio::test]
    async fn a_400_renders_its_own_message_unchanged() {
        let body = body_json(StatusCode::BAD_REQUEST, "invalid or expired reset token").await;
        assert_eq!(body["message"], "invalid or expired reset token");
    }

    #[tokio::test]
    async fn a_503_renders_its_own_message_unchanged() {
        // 503 is still a server error (logged), but not a bare 500 — a
        // deliberate operator-facing message like "LDAP is not configured" must survive.
        let body = body_json(StatusCode::SERVICE_UNAVAILABLE, "LDAP is not configured").await;
        assert_eq!(body["message"], "LDAP is not configured");
    }

    #[test]
    fn is_unique_violation_matches_every_form_the_driver_produces() {
        assert!(is_unique_violation(&sea_orm::DbErr::Custom(
            "duplicate key value violates unique constraint \"uq_favorites\"".to_string()
        )));
        assert!(is_unique_violation(&sea_orm::DbErr::Custom(
            "unique constraint failed".to_string()
        )));
        // The bare SQLSTATE with neither English phrase — e.g. a driver that only surfaces the
        // code.
        assert!(is_unique_violation(&sea_orm::DbErr::Custom(
            "ERROR: 23505".to_string()
        )));
        assert!(!is_unique_violation(&sea_orm::DbErr::Custom(
            "connection refused".to_string()
        )));
    }
}
