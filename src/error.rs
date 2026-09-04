use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::error;
use uuid::Uuid;

use crate::auth::AuthError;
use crate::{
    domain::DomainError,
    mutation::{CoordinatorError, IdempotencyError},
};

#[derive(Clone, Copy)]
pub struct RequestId(pub Uuid);

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: Uuid,
    retry_after: Option<i64>,
    issues: Vec<crate::domain::ValidationIssue>,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: Uuid,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    issues: Vec<crate::domain::ValidationIssue>,
}

impl ApiError {
    pub fn code_and_status(&self) -> (&'static str, StatusCode) {
        (self.code, self.status)
    }
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        request_id: RequestId,
    ) -> Self {
        Self {
            status,
            code,
            message,
            request_id: request_id.0,
            retry_after: None,
            issues: Vec::new(),
        }
    }

    pub fn validation(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_FAILED",
            "The request is invalid",
            request_id,
        )
    }

    pub fn origin(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "ORIGIN_INVALID",
            "The request origin is invalid",
            request_id,
        )
    }

    pub fn csrf(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "CSRF_INVALID",
            "The CSRF token is invalid",
            request_id,
        )
    }

    pub fn bootstrap_local_only(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "BOOTSTRAP_LOCAL_ONLY",
            "Bootstrap is only available from a loopback peer",
            request_id,
        )
    }

    pub fn app_not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "APP_NOT_FOUND",
            "The application was not found",
            request_id,
        )
    }

    pub fn invalid_query(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_QUERY",
            "The query is invalid",
            request_id,
        )
    }

    pub fn stream_limit(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "STREAM_LIMIT_REACHED",
            "The stream limit has been reached",
            request_id,
        )
    }

    pub fn docker(request_id: RequestId, code: &'static str) -> Self {
        let (status, message) = match code {
            "APP_CONTAINER_NOT_FOUND" => (
                StatusCode::CONFLICT,
                "The application container is not available",
            ),
            "APP_CONTAINER_AMBIGUOUS" => (
                StatusCode::CONFLICT,
                "The application has multiple containers",
            ),
            "APP_CONTAINER_INVALID" => (
                StatusCode::CONFLICT,
                "The application container identity is invalid",
            ),
            "DOCKER_PERMISSION_DENIED" => {
                (StatusCode::SERVICE_UNAVAILABLE, "Docker access is denied")
            }
            "DOCKER_API_INCOMPATIBLE" => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The Docker Engine is incompatible",
            ),
            _ => (StatusCode::SERVICE_UNAVAILABLE, "Docker is unavailable"),
        };
        let mut error = Self::new(status, code, message, request_id);
        if status == StatusCode::SERVICE_UNAVAILABLE {
            error.retry_after = Some(3);
        }
        error
    }

    pub fn domain(error: DomainError, request_id: RequestId) -> Self {
        let code = match error {
            DomainError::ConfigQuotaExceeded => "CONFIG_QUOTA_EXCEEDED",
            DomainError::EnvDuplicate => "ENV_DUPLICATE",
            DomainError::SecretOperationRequired => "SECRET_OPERATION_REQUIRED",
            DomainError::FileTargetConflict => "FILE_TARGET_CONFLICT",
            DomainError::BindDisabled => "BIND_DISABLED",
            DomainError::BindOutsideAllowedRoot => "BIND_OUTSIDE_ALLOWED_ROOT",
            DomainError::BindSymlink => "BIND_SYMLINK",
            DomainError::BindChanged => "BIND_CHANGED",
            DomainError::BindSourceAncestorConflict => "BIND_SOURCE_ANCESTOR_CONFLICT",
            DomainError::BindRwAckRequired => "BIND_RW_ACK_REQUIRED",
            DomainError::PortConflict => "PORT_CONFLICT",
            DomainError::FeatureNotAvailable => "FEATURE_NOT_AVAILABLE",
            DomainError::ConfigInvalid => "CONFIG_INVALID",
            DomainError::Internal => return Self::internal(request_id),
        };
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            code,
            "The application configuration is invalid",
            request_id,
        )
    }

    pub fn draft_validation(
        error: crate::domain::DraftValidationError,
        request_id: RequestId,
    ) -> Self {
        let mut response = Self::domain(error.error, request_id);
        response.issues = error.issues;
        response
    }

    pub fn idempotency(error: IdempotencyError, request_id: RequestId) -> Self {
        match error {
            IdempotencyError::KeyRequired => Self::new(
                StatusCode::BAD_REQUEST,
                "IDEMPOTENCY_KEY_REQUIRED",
                "An idempotency key is required",
                request_id,
            ),
            IdempotencyError::KeyInvalid => Self::new(
                StatusCode::BAD_REQUEST,
                "IDEMPOTENCY_KEY_INVALID",
                "The idempotency key is invalid",
                request_id,
            ),
            IdempotencyError::Reused => Self::new(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_KEY_REUSED",
                "The idempotency key was used for a different request",
                request_id,
            ),
            IdempotencyError::InProgress => Self::new(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_IN_PROGRESS",
                "The operation is in progress",
                request_id,
            ),
            _ => Self::internal(request_id),
        }
    }

    pub fn coordinator(error: CoordinatorError, request_id: RequestId) -> Self {
        match error {
            CoordinatorError::Busy => Self::new(
                StatusCode::CONFLICT,
                "APP_BUSY",
                "The application is busy",
                request_id,
            ),
            _ => Self::internal(request_id),
        }
    }

    pub fn conflict(code: &'static str, request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            code,
            "The operation conflicts with current application state",
            request_id,
        )
    }

    pub fn compose(code: &'static str, request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            code,
            "Docker Compose is unavailable",
            request_id,
        )
    }

    pub fn registry(code: &'static str, request_id: RequestId) -> Self {
        let status = match code {
            "REGISTRY_CREDENTIAL_REQUIRED"
            | "REGISTRY_CREDENTIAL_INVALID"
            | "REGISTRY_FORBIDDEN"
            | "REGISTRY_CREDENTIAL_MISMATCH" => StatusCode::UNPROCESSABLE_ENTITY,
            "REGISTRY_TAG_NOT_FOUND" => StatusCode::NOT_FOUND,
            "REGISTRY_RATE_LIMITED"
            | "REGISTRY_UNAVAILABLE"
            | "REGISTRY_TIMEOUT"
            | "REGISTRY_TLS_ERROR" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Self::new(
            status,
            code,
            "The image registry request failed",
            request_id,
        )
    }

    pub fn internal(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "An internal error occurred",
            request_id,
        )
    }

    pub fn from_auth(error_value: AuthError, request_id: RequestId) -> Self {
        match error_value {
            AuthError::SetupRequired => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "SETUP_REQUIRED",
                "Administrator setup is required",
                request_id,
            ),
            AuthError::AlreadyBootstrapped => Self::new(
                StatusCode::CONFLICT,
                "ALREADY_BOOTSTRAPPED",
                "Administrator setup is already complete",
                request_id,
            ),
            AuthError::BootstrapTokenInvalid => Self::new(
                StatusCode::UNAUTHORIZED,
                "BOOTSTRAP_TOKEN_INVALID",
                "The bootstrap token is invalid",
                request_id,
            ),
            AuthError::InvalidCredentials => Self::new(
                StatusCode::UNAUTHORIZED,
                "AUTH_INVALID",
                "The username or password is invalid",
                request_id,
            ),
            AuthError::Cooldown(seconds) => {
                let mut error = Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "AUTH_COOLDOWN",
                    "Authentication is temporarily unavailable",
                    request_id,
                );
                error.retry_after = Some(seconds);
                error
            }
            AuthError::SessionRequired => Self::new(
                StatusCode::UNAUTHORIZED,
                "SESSION_REQUIRED",
                "Authentication is required",
                request_id,
            ),
            AuthError::SessionExpired => Self::new(
                StatusCode::UNAUTHORIZED,
                "SESSION_EXPIRED",
                "The session has expired",
                request_id,
            ),
            AuthError::Password(crate::auth::password::PasswordError::Policy) => {
                Self::validation(request_id)
            }
            internal => {
                error!(request_id = %request_id.0, error = %internal, "request failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "An internal error occurred",
                    request_id,
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                code: self.code,
                message: self.message,
                request_id: self.request_id,
                issues: self.issues,
            }),
        )
            .into_response();
        if let Some(seconds) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(RETRY_AFTER, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, response::IntoResponse};

    use super::*;

    #[tokio::test]
    async fn draft_validation_response_adds_safe_optional_issues() {
        let error = ApiError::draft_validation(
            crate::domain::DraftValidationError {
                error: DomainError::ConfigInvalid,
                issues: vec![crate::domain::ValidationIssue {
                    path: "health.http.retries".into(),
                    code: "OUT_OF_RANGE",
                    message: "Must be between 1 and 10".to_owned(),
                }],
            },
            RequestId(Uuid::nil()),
        );
        let response = error.into_response();
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "CONFIG_INVALID");
        assert_eq!(body["message"], "The application configuration is invalid");
        assert_eq!(body["issues"][0]["path"], "health.http.retries");
        assert_eq!(body["issues"][0]["code"], "OUT_OF_RANGE");
    }
}
