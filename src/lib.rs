use std::{fmt, net::SocketAddr};

use axum::{Json, Router, routing::get};
use serde::Serialize;

pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:8080";

#[derive(Debug, PartialEq, Eq)]
pub enum ListenAddressError {
    Invalid(String),
    NonLoopback(SocketAddr),
}

impl fmt::Display for ListenAddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(value) => write!(formatter, "invalid listen address: {value}"),
            Self::NonLoopback(address) => {
                write!(formatter, "listen address must be loopback: {address}")
            }
        }
    }
}

impl std::error::Error for ListenAddressError {}

pub fn parse_listen_address(value: Option<&str>) -> Result<SocketAddr, ListenAddressError> {
    let value = value.unwrap_or(DEFAULT_LISTEN_ADDRESS);
    let address = value
        .parse::<SocketAddr>()
        .map_err(|_| ListenAddressError::Invalid(value.to_owned()))?;

    if !address.ip().is_loopback() {
        return Err(ListenAddressError::NonLoopback(address));
    }

    Ok(address)
}

pub fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn defaults_to_loopback() {
        assert_eq!(
            parse_listen_address(None).unwrap(),
            DEFAULT_LISTEN_ADDRESS.parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn accepts_loopback_override() {
        assert_eq!(
            parse_listen_address(Some("127.0.0.1:9090")).unwrap(),
            "127.0.0.1:9090".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn rejects_non_loopback_override() {
        let address = "0.0.0.0:8080".parse::<SocketAddr>().unwrap();

        assert_eq!(
            parse_listen_address(Some("0.0.0.0:8080")),
            Err(ListenAddressError::NonLoopback(address))
        );
    }

    #[tokio::test]
    async fn healthz_returns_json_without_binding_a_port() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            br#"{"status":"ok"}"#
        );
    }
}
