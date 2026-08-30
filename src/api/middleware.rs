use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue, header},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::info;
use uuid::Uuid;

use super::AppState;
use crate::error::RequestId;

pub async fn host_isolation(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path_is_hook = request.uri().path().starts_with("/hooks/");
    let host = request.headers().get_all(header::HOST);
    let mut values = host.iter();
    let host = values.next().and_then(|value| value.to_str().ok());
    if values.next().is_some() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let webhook_host = state.webhooks.as_ref().is_some_and(|services| {
        !services.authority.is_empty() && host == Some(services.authority.as_str())
    });
    if path_is_hook != webhook_host
        || (path_is_hook
            && (request.method() != Method::POST || !canonical_webhook_path(request.uri().path())))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

fn canonical_webhook_path(path: &str) -> bool {
    let Some(app) = path
        .strip_prefix("/hooks/v1/apps/")
        .and_then(|value| value.strip_suffix("/registry"))
    else {
        return false;
    };
    app.parse::<Uuid>()
        .is_ok_and(|app_id| app == app_id.to_string())
}

pub async fn request_context(mut request: Request<Body>, next: Next) -> Response {
    let request_id = RequestId(Uuid::new_v4());
    request.extensions_mut().insert(request_id);
    let method = request.method().clone();
    let is_api_response = request.uri().path().starts_with("/api/v1/")
        || request.uri().path().starts_with("/hooks/v1/");
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let started = Instant::now();
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        HeaderName::from_static("x-request-id"),
        HeaderValue::from_str(&request_id.0.to_string()).expect("UUID is a valid header value"),
    );
    if is_api_response && !response.headers().contains_key(header::CACHE_CONTROL) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    info!(
        request_id = %request_id.0,
        method = %method,
        matched_route = %route,
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request completed"
    );
    response
}
