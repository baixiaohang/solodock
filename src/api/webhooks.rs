use axum::{
    Json,
    extract::{Extension, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

use super::{
    AppState,
    auth::{Authenticated, MutationAuthenticated},
};
use crate::{
    app_store::StoreError,
    error::{ApiError, RequestId},
    mutation::{ClaimResult, IdempotencyError},
    security::secret::SecretValue,
    webhook::{WebhookMetadata, WebhookStatus},
};

#[derive(Serialize)]
pub struct WebhookResponse {
    configured: bool,
    degraded: bool,
    metadata_revision: Option<Uuid>,
    secret_revision: Option<Uuid>,
    algorithm: &'static str,
    public_origin: String,
    public_path: String,
    #[serde(with = "time::serde::rfc3339::option")]
    created_at: Option<time::OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    rotated_at: Option<time::OffsetDateTime>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureWebhook {
    expected_metadata_revision: Option<Uuid>,
    secret: SecretValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeWebhook {
    expected_metadata_revision: Uuid,
}

pub async fn status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    _authenticated: Authenticated,
) -> Result<Json<WebhookResponse>, ApiError> {
    let services = services(&state, request_id)?;
    if state.observer.catalog.get(app_id).is_none() {
        return Err(ApiError::app_not_found(request_id));
    }
    let status = services.store.status(app_id).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "WEBHOOK_STORE_DEGRADED",
            "The webhook store is unavailable",
            request_id,
        )
    })?;
    Ok(Json(response(services, app_id, status)))
}

pub async fn configure(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<ConfigureWebhook>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let m3 = state
        .m3
        .as_ref()
        .ok_or_else(|| ApiError::internal(request_id))?;
    let Json(payload) = payload.map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WEBHOOK_SECRET_INVALID",
            "The webhook secret is invalid",
            request_id,
        )
    })?;
    let route = format!("/api/v1/apps/{app_id}/webhook");
    let key = idempotency_key(&headers, request_id)?;
    let mut canonical = serde_json::to_vec(&serde_json::json!({
        "method":"PUT","route":route,"expected_metadata_revision":payload.expected_metadata_revision,
        "secret_sha256":format!("{:x}", Sha256::digest(payload.secret.expose().as_bytes()))
    })).map_err(|_| ApiError::internal(request_id))?;
    let fingerprint = m3.idempotency.fingerprint(&canonical);
    canonical.zeroize();
    let claim = m3
        .idempotency
        .claim(&route, key, &fingerprint, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body);
    }
    let operation = match claim {
        ClaimResult::New(id) | ClaimResult::Resume(id) => id,
        _ => unreachable!(),
    };
    let _catalog = m3.coordinator.catalog_lock().await;
    // Publish the new secret pattern under the same mutex used by complete
    // inventory replacement. A reconciler that collected the old inventory
    // can therefore never overwrite this first fail-closed publication after
    // the filesystem metadata becomes visible.
    let publication = m3.publication_lock.lock().await;
    let metadata = match services.store.configure(
        app_id,
        payload.expected_metadata_revision,
        operation,
        &payload.secret,
    ) {
        Ok(value) => value,
        Err(StoreError::RevisionStale) => {
            return finish_error(
                &state,
                &route,
                key,
                "WEBHOOK_REVISION_CONFLICT",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Err(StoreError::ContentInvalid) => {
            return finish_error(
                &state,
                &route,
                key,
                "WEBHOOK_SECRET_INVALID",
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt(&state, &route, key, request_id).await,
    };
    let decoded = match crate::webhook::protocol::decode_secret(&payload.secret) {
        Ok(value) => value,
        Err(_) => return interrupt(&state, &route, key, request_id).await,
    };
    state.redactor.extend([
        payload.secret.expose().as_bytes().to_vec(),
        decoded.to_vec(),
    ]);
    drop(publication);
    let value = response_from_metadata(services, metadata);
    let completed = finish_json(&state, &route, key, StatusCode::OK, &value, request_id).await?;
    cleanup_or_degrade(&state, app_id);
    let _ = crate::api::mutations::refresh(&state, m3).await;
    Ok(completed)
}

pub async fn revoke(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<RevokeWebhook>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let m3 = state
        .m3
        .as_ref()
        .ok_or_else(|| ApiError::internal(request_id))?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let route = format!("/api/v1/apps/{app_id}/webhook");
    let key = idempotency_key(&headers, request_id)?;
    let fingerprint = m3
        .idempotency
        .fingerprint(format!("DELETE\0{route}\0{}", payload.expected_metadata_revision).as_bytes());
    let claim = m3
        .idempotency
        .claim(&route, key, &fingerprint, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body);
    }
    let operation = match claim {
        ClaimResult::New(id) | ClaimResult::Resume(id) => id,
        _ => unreachable!(),
    };
    let _catalog = m3.coordinator.catalog_lock().await;
    let metadata =
        match services
            .store
            .revoke(app_id, payload.expected_metadata_revision, operation)
        {
            Ok(value) => value,
            Err(StoreError::RevisionStale) => {
                return finish_error(
                    &state,
                    &route,
                    key,
                    "WEBHOOK_REVISION_CONFLICT",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
            Err(_) => return interrupt(&state, &route, key, request_id).await,
        };
    let value = response_from_metadata(services, metadata);
    let completed = finish_json(&state, &route, key, StatusCode::OK, &value, request_id).await?;
    cleanup_or_degrade(&state, app_id);
    let _ = crate::api::mutations::refresh(&state, m3).await;
    Ok(completed)
}

fn response(
    services: &crate::webhook::WebhookServices,
    app_id: Uuid,
    value: WebhookStatus,
) -> WebhookResponse {
    WebhookResponse {
        configured: value.configured,
        degraded: value.degraded,
        metadata_revision: value.metadata_revision,
        secret_revision: value.secret_revision,
        algorithm: "hmac-sha256-v1",
        public_origin: services.origin.clone(),
        public_path: format!("/hooks/v1/apps/{app_id}/registry"),
        created_at: value.created_at,
        rotated_at: value.rotated_at,
    }
}

fn response_from_metadata(
    services: &crate::webhook::WebhookServices,
    value: WebhookMetadata,
) -> WebhookResponse {
    response(
        services,
        value.app_id,
        WebhookStatus {
            configured: value.enabled,
            degraded: false,
            metadata_revision: Some(value.metadata_revision),
            secret_revision: value.secret_revision,
            created_at: Some(value.created_at),
            rotated_at: value.rotated_at,
        },
    )
}

fn services(
    state: &AppState,
    request_id: RequestId,
) -> Result<&crate::webhook::WebhookServices, ApiError> {
    state
        .webhooks
        .as_deref()
        .filter(|services| !services.origin.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "WEBHOOK_UNAVAILABLE",
                "The webhook endpoint is not configured",
                request_id,
            )
        })
}

fn idempotency_key(headers: &HeaderMap, request_id: RequestId) -> Result<&str, ApiError> {
    headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::idempotency(IdempotencyError::KeyRequired, request_id))?
        .to_str()
        .map_err(|_| ApiError::idempotency(IdempotencyError::KeyInvalid, request_id))
}

fn replay(status: u16, body: String) -> Result<Response, ApiError> {
    let status =
        StatusCode::from_u16(status).map_err(|_| ApiError::internal(RequestId(Uuid::nil())))?;
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn finish_json<T: Serialize>(
    state: &AppState,
    route: &str,
    key: &str,
    status: StatusCode,
    value: &T,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::to_string(value).map_err(|_| ApiError::internal(request_id))?;
    let m3 = state
        .m3
        .as_ref()
        .ok_or_else(|| ApiError::internal(request_id))?;
    if m3
        .idempotency
        .finish(route, key, status.as_u16(), &body, None, request_id.0)
        .await
        .is_err()
    {
        let _ = m3
            .idempotency
            .mark_interrupted(route, key, request_id.0)
            .await;
        return Err(ApiError::internal(request_id));
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn finish_error(
    state: &AppState,
    route: &str,
    key: &str,
    code: &'static str,
    status: StatusCode,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let value = serde_json::json!({"code":code,"message":"The webhook operation could not be completed","request_id":request_id.0});
    let body = value.to_string();
    let m3 = state
        .m3
        .as_ref()
        .ok_or_else(|| ApiError::internal(request_id))?;
    if m3
        .idempotency
        .finish(route, key, status.as_u16(), &body, Some(code), request_id.0)
        .await
        .is_err()
    {
        let _ = m3
            .idempotency
            .mark_interrupted(route, key, request_id.0)
            .await;
        return Err(ApiError::internal(request_id));
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn interrupt(
    state: &AppState,
    route: &str,
    key: &str,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    if let Some(m3) = state.m3.as_ref() {
        let _ = m3
            .idempotency
            .mark_interrupted(route, key, request_id.0)
            .await;
    }
    Err(ApiError::internal(request_id))
}

fn cleanup_or_degrade(state: &AppState, app_id: Uuid) {
    if state
        .webhooks
        .as_ref()
        .is_some_and(|services| services.store.cleanup_unreferenced(app_id).is_err())
        && let Some(m3) = state.m3.as_ref()
    {
        m3.projection_degraded
            .store(true, std::sync::atomic::Ordering::Release);
        m3.reconcile_notify.notify_one();
    }
}
