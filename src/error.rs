use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::error;
use uuid::Uuid;

use crate::auth::AuthError;

#[derive(Clone, Copy)]
pub struct RequestId(pub Uuid);

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: Uuid,
    retry_after: Option<i64>,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: Uuid,
}

impl ApiError {
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
