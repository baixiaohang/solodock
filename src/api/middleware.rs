use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue, header},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use tracing::info;
use uuid::Uuid;

use super::AppState;
use crate::{config::CanonicalAuthority, error::RequestId};

pub async fn host_isolation(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Ok(authority) = effective_authority(&request) else {
        return isolated_not_found();
    };
    let path = request.uri().path();
    let allowed = if authority == state.management_authority {
        path != "/hooks" && !path.starts_with("/hooks/")
    } else if state.webhook_authority.as_ref() == Some(&authority) {
        request.method() == Method::POST
            && request.uri().query().is_none()
            && canonical_webhook_path(path)
    } else if authority == state.local_probe_authority
        && state.local_probe_authority != state.management_authority
    {
        request.method() == Method::GET
            && request.uri().query().is_none()
            && matches!(path, "/healthz" | "/favicon.svg")
    } else {
        false
    };
    if allowed {
        next.run(request).await
    } else {
        isolated_not_found()
    }
}

fn effective_authority(request: &Request<Body>) -> Result<CanonicalAuthority, ()> {
    let uri = request
        .uri()
        .authority()
        .map(|value| CanonicalAuthority::parse_http(value.as_str()).map_err(|_| ()))
        .transpose()?;
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts
        .next()
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ())
                .and_then(|value| CanonicalAuthority::parse_http(value).map_err(|_| ()))
        })
        .transpose()?;
    if hosts.next().is_some() {
        return Err(());
    }
    match (uri, host) {
        (Some(uri), Some(host)) if uri == host => Ok(uri),
        (Some(_), Some(_)) | (None, None) => Err(()),
        (Some(authority), None) | (None, Some(authority)) => Ok(authority),
    }
}

fn isolated_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .expect("static isolation response is valid")
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
