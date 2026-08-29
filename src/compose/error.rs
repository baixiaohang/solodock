use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ComposeError {
    #[error("Docker Compose is unavailable")]
    Unavailable,
    #[error("Docker Compose is incompatible")]
    Incompatible,
    #[error("Docker Compose permission was denied")]
    PermissionDenied,
    #[error("Docker Compose validation failed")]
    ValidationFailed,
    #[error("Docker Compose operation timed out")]
    Timeout,
    #[error("Docker Compose operation was cancelled")]
    Cancelled,
    #[error("Docker Compose output was invalid")]
    OutputInvalid,
    #[error("Docker Compose temporary path is unsafe")]
    UnsafePath,
}

impl ComposeError {
    pub const fn public_code(self) -> &'static str {
        match self {
            Self::Unavailable => "COMPOSE_UNAVAILABLE",
            Self::Incompatible => "COMPOSE_INCOMPATIBLE",
            Self::PermissionDenied => "COMPOSE_PERMISSION_DENIED",
            Self::ValidationFailed | Self::OutputInvalid | Self::UnsafePath => {
                "COMPOSE_VALIDATION_FAILED"
            }
            Self::Timeout => "COMPOSE_TIMEOUT",
            Self::Cancelled => "OPERATION_INTERRUPTED",
        }
    }
}
