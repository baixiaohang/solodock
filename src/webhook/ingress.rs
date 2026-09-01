use axum::{
    body::{Body, to_bytes},
    extract::{Extension, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    api::AppState,
    error::RequestId,
    registry::{WebhookAccept, poller::poll_generation},
};

use super::protocol::{WebhookPayload, parse_headers, verify};

#[derive(Serialize)]
struct Accepted {
    accepted: bool,
    request_id: Uuid,
}

pub async fn receive(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_text): Path<String>,
    request: Request<Body>,
) -> Response {
    let Some(webhooks) = state.webhooks.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let headers = request.headers().clone();
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > super::protocol::MAX_BODY_BYTES)
    {
        return webhook_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WEBHOOK_PAYLOAD_INVALID",
            request_id,
        );
    }
    let permit = match webhooks.permits.clone().try_acquire_owned() {
        Ok(value) => value,
        Err(_) => {
            return webhook_error(
                StatusCode::TOO_MANY_REQUESTS,
                "WEBHOOK_RATE_LIMITED",
                request_id,
            );
        }
    };
    let body = match to_bytes(request.into_body(), super::protocol::MAX_BODY_BYTES).await {
        Ok(value) => value,
        Err(_) => {
            return webhook_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "WEBHOOK_PAYLOAD_INVALID",
                request_id,
            );
        }
    };
    let app_id = match app_text.parse::<Uuid>() {
        Ok(value) if app_text == value.to_string() => value,
        _ => {
            if !webhooks.limiter.check(None, false).await {
                return webhook_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "WEBHOOK_RATE_LIMITED",
                    request_id,
                );
            }
            return unauthorized(request_id);
        }
    };
    let catalog_app = state.observer.catalog.get(app_id);
    let known_app = catalog_app.is_some();
    if !webhooks.limiter.check(Some(app_id), known_app).await {
        drop(permit);
        return webhook_error(
            StatusCode::TOO_MANY_REQUESTS,
            "WEBHOOK_RATE_LIMITED",
            request_id,
        );
    }
    if catalog_app.is_some_and(|app| app.draft_revision.is_none()) {
        return webhook_error(StatusCode::CONFLICT, "APP_UNCONFIGURED", request_id);
    }
    if one_header(&headers, header::CONTENT_TYPE.as_str()) != Ok("application/json") {
        return webhook_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WEBHOOK_PAYLOAD_INVALID",
            request_id,
        );
    }
    let parsed = match required_headers(&headers) {
        Ok(value) => value,
        Err(()) => return unauthorized(request_id),
    };
    let loaded = match webhooks.store.load_current(app_id) {
        Ok(value) => value,
        Err(_) => return unauthorized(request_id),
    };
    if verify(
        app_id,
        &body,
        &parsed,
        &loaded.secret,
        OffsetDateTime::now_utc(),
    )
    .is_err()
    {
        return unauthorized(request_id);
    }
    if serde_json::from_slice::<WebhookPayload>(&body).is_err() {
        return webhook_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WEBHOOK_PAYLOAD_INVALID",
            request_id,
        );
    }
    let Some(m3) = state.m3.as_ref() else {
        return webhook_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WEBHOOK_UNAVAILABLE",
            request_id,
        );
    };
    let Some(m4) = state.m4.as_ref() else {
        return webhook_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WEBHOOK_UNAVAILABLE",
            request_id,
        );
    };
    // Serialize the final secret/app/generation observation with every app,
    // credential, webhook and deletion mutation. Once one of those mutations
    // commits, a request authenticated by the previous secret can no longer
    // create a durable wake.
    let _catalog = m3.coordinator.catalog_lock().await;
    let current = match webhooks.store.load_current(app_id) {
        Ok(value) if value.metadata.secret_revision == loaded.metadata.secret_revision => value,
        _ => return unauthorized(request_id),
    };
    let metadata = match m3.store.read_metadata(app_id) {
        Ok(value) => value,
        Err(_) => return unauthorized(request_id),
    };
    if !metadata.is_configured() {
        return webhook_error(StatusCode::CONFLICT, "APP_UNCONFIGURED", request_id);
    }
    let generation = match poll_generation(&m3.store, &m4.credentials, &metadata) {
        Ok(value) => value,
        Err(_) => {
            return webhook_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "WEBHOOK_STORE_DEGRADED",
                request_id,
            );
        }
    };
    let accepted = match webhooks
        .poll_states
        .accept_webhook(
            app_id,
            current
                .metadata
                .secret_revision
                .expect("enabled webhook has secret revision"),
            &parsed.nonce_sha256,
            request_id.0,
            &generation,
            metadata.auto_deploy_enabled,
        )
        .await
    {
        Ok(WebhookAccept::Replay) => {
            return webhook_error(StatusCode::CONFLICT, "WEBHOOK_REPLAYED", request_id);
        }
        Ok(_) => true,
        Err(_) => {
            return webhook_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "WEBHOOK_STORE_DEGRADED",
                request_id,
            );
        }
    };
    webhooks.notify.notify_one();
    drop(permit);
    (
        StatusCode::ACCEPTED,
        axum::Json(Accepted {
            accepted,
            request_id: request_id.0,
        }),
    )
        .into_response()
}

fn required_headers(headers: &HeaderMap) -> Result<super::protocol::VerifiedHeaders, ()> {
    parse_headers(
        one_header(headers, "x-solodock-timestamp")?,
        one_header(headers, "x-solodock-nonce")?,
        one_header(headers, "x-solodock-signature")?,
    )
    .map_err(|_| ())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(())?.to_str().map_err(|_| ())?;
    if values.next().is_some() {
        return Err(());
    }
    Ok(value)
}

fn unauthorized(request_id: RequestId) -> Response {
    webhook_error(StatusCode::UNAUTHORIZED, "WEBHOOK_UNAUTHORIZED", request_id)
}

fn webhook_error(status: StatusCode, code: &'static str, request_id: RequestId) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "code": code,
            "message": "The webhook request was not accepted",
            "request_id": request_id.0,
        })),
    )
        .into_response()
}
