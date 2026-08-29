#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry reference is invalid")]
    ReferenceInvalid,
    #[error("registry credential is required")]
    CredentialRequired,
    #[error("registry credential is invalid")]
    CredentialInvalid,
    #[error("registry access is forbidden")]
    Forbidden,
    #[error("registry tag was not found")]
    TagNotFound,
    #[error("registry rate limit exceeded")]
    RateLimited,
    #[error("registry is unavailable")]
    Unavailable,
    #[error("registry request timed out")]
    Timeout,
    #[error("registry TLS failed")]
    Tls,
    #[error("registry protocol is invalid")]
    Protocol,
    #[error("registry manifest is unsupported")]
    ManifestUnsupported,
    #[error("registry digest mismatch")]
    DigestMismatch,
    #[error("registry platform was not found")]
    PlatformNotFound,
    #[error("registry authentication is unsupported")]
    AuthUnsupported,
}

impl RegistryError {
    pub const fn public_code(&self) -> &'static str {
        match self {
            Self::CredentialRequired => "REGISTRY_CREDENTIAL_REQUIRED",
            Self::CredentialInvalid => "REGISTRY_CREDENTIAL_INVALID",
            Self::Forbidden => "REGISTRY_FORBIDDEN",
            Self::TagNotFound => "REGISTRY_TAG_NOT_FOUND",
            Self::RateLimited => "REGISTRY_RATE_LIMITED",
            Self::Unavailable => "REGISTRY_UNAVAILABLE",
            Self::Timeout => "REGISTRY_TIMEOUT",
            Self::Tls => "REGISTRY_TLS_ERROR",
            Self::Protocol | Self::ReferenceInvalid => "REGISTRY_PROTOCOL_ERROR",
            Self::ManifestUnsupported => "REGISTRY_MANIFEST_UNSUPPORTED",
            Self::DigestMismatch => "REGISTRY_DIGEST_MISMATCH",
            Self::PlatformNotFound => "REGISTRY_PLATFORM_NOT_FOUND",
            Self::AuthUnsupported => "REGISTRY_AUTH_UNSUPPORTED",
        }
    }
}
