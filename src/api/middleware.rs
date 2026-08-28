use std::time::Instant;

use axum::{
    body::Body,
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use tracing::info;
use uuid::Uuid;

use crate::error::RequestId;

pub async fn request_context(mut request: Request<Body>, next: Next) -> Response {
    let request_id = RequestId(Uuid::new_v4());
    request.extensions_mut().insert(request_id);
    let method = request.method().clone();
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
