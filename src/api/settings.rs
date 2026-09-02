use std::{path::PathBuf, sync::Arc};

use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    AppState,
    auth::{Authenticated, MutationAuthenticated},
    mutations::M3Services,
};
use crate::{
    domain::{HealthConfigurationLimits, health_configuration_limits},
    error::{ApiError, RequestId},
    mutation::{ClaimResult, IdempotencyError},
    settings::{
        GlobalSettings, SettingsError, SettingsStore, supported_timezones, validate_timezone,
    },
};

#[derive(Serialize)]
struct ConfigurationLimits {
    health: HealthConfigurationLimits,
}

const ROUTE: &str = "/api/v1/settings";

#[derive(Serialize)]
pub struct SettingsResponse {
    revision: Uuid,
    display_timezone: String,
    supported_timezones: Vec<&'static str>,
    allowed_bind_roots: Vec<PathBuf>,
    slug_max_length: usize,
    supported_mount_types: [&'static str; 3],
    configuration_limits: ConfigurationLimits,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_replayed: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettingsRequest {
    expected_revision: Uuid,
    display_timezone: String,
    allowed_bind_roots: Vec<PathBuf>,
}

pub async fn get(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    _authenticated: Authenticated,
) -> Result<Json<SettingsResponse>, ApiError> {
    let services = m3(&state, request_id)?;
    let settings = SettingsStore::new(services.database.clone())
        .load()
        .await
        .map_err(|_| ApiError::internal(request_id))?;
    Ok(Json(response(settings, None)))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<UpdateSettingsRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = m3(&state, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    validate_timezone(&payload.display_timezone).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "DISPLAY_TIMEZONE_INVALID",
            "The display timezone is not supported",
            request_id,
        )
    })?;
    let mut allowed_bind_roots = payload.allowed_bind_roots.clone();
    allowed_bind_roots.sort();
    allowed_bind_roots.dedup();
    let key = idempotency_key(&headers, request_id)?;
    let canonical = serde_json::to_vec(&serde_json::json!({
        "method": "PUT",
        "route": ROUTE,
        "expected_revision": payload.expected_revision,
        "display_timezone": payload.display_timezone,
        "allowed_bind_roots": allowed_bind_roots,
    }))
    .map_err(|_| ApiError::internal(request_id))?;
    let fingerprint = services.idempotency.fingerprint(&canonical);
    let claim = services
        .idempotency
        .claim(ROUTE, key, &fingerprint, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body, request_id);
    }
    let resumed = matches!(claim, ClaimResult::Resume(_));
    let _guard = services.coordinator.catalog_lock().await;
    let store = SettingsStore::new(services.database.clone());
    let current = match store.load().await {
        Ok(value) => value,
        Err(_) => return interrupt(services, key, request_id).await,
    };
    if current.revision != payload.expected_revision {
        if resumed
            && current.display_timezone == payload.display_timezone
            && current.allowed_bind_roots == allowed_bind_roots
        {
            services
                .store
                .replace_allowed_bind_roots(current.allowed_bind_roots.clone());
            return finish_json(services, key, &response(current, Some(false)), request_id).await;
        }
        return finish_error(
            services,
            key,
            "SETTINGS_REVISION_STALE",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    if current.allowed_bind_roots != allowed_bind_roots {
        let docker = state.observer.api();
        let probe = match docker.probe().await {
            Ok(value) => value,
            Err(error) => {
                return finish_error(
                    services,
                    key,
                    error.public_code(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    request_id,
                )
                .await;
            }
        };
        let Some(docker_root) = probe.docker_root_directory.as_deref() else {
            return finish_error(
                services,
                key,
                "DOCKER_OBSERVATION_FAILED",
                StatusCode::SERVICE_UNAVAILABLE,
                request_id,
            )
            .await;
        };
        allowed_bind_roots = match crate::config::validate_bind_roots(
            &allowed_bind_roots,
            &state.state_directory,
            &services.runtime_directory,
            Some(docker_root),
        ) {
            Ok(value) => value,
            Err(_) => {
                return finish_error(
                    services,
                    key,
                    "BIND_ROOT_INVALID",
                    StatusCode::UNPROCESSABLE_ENTITY,
                    request_id,
                )
                .await;
            }
        };
    }
    let referenced =
        match removed_root_references(services, &current.allowed_bind_roots, &allowed_bind_roots) {
            Ok(value) => value,
            Err(()) => return interrupt(services, key, request_id).await,
        };
    if let Some(apps) = referenced {
        return finish_error_with_apps(
            services,
            key,
            "BIND_ROOT_IN_USE",
            StatusCode::CONFLICT,
            &apps,
            request_id,
        )
        .await;
    }
    let settings = match store
        .update(
            payload.expected_revision,
            &payload.display_timezone,
            &allowed_bind_roots,
        )
        .await
    {
        Ok(value) => value,
        Err(SettingsError::RevisionStale) => {
            return finish_error(
                services,
                key,
                "SETTINGS_REVISION_STALE",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt(services, key, request_id).await,
    };
    services
        .store
        .replace_allowed_bind_roots(settings.allowed_bind_roots.clone());
    finish_json(services, key, &response(settings, Some(false)), request_id).await
}

fn response(settings: GlobalSettings, replayed: Option<bool>) -> SettingsResponse {
    SettingsResponse {
        revision: settings.revision,
        display_timezone: settings.display_timezone,
        supported_timezones: supported_timezones(),
        allowed_bind_roots: settings.allowed_bind_roots,
        slug_max_length: 20,
        supported_mount_types: ["owned_volume", "external_volume", "bind"],
        configuration_limits: ConfigurationLimits {
            health: health_configuration_limits(),
        },
        idempotency_replayed: replayed,
    }
}

fn removed_root_references(
    services: &M3Services,
    current: &[PathBuf],
    next: &[PathBuf],
) -> Result<Option<Vec<String>>, ()> {
    let removed = current
        .iter()
        .filter(|root| !next.contains(root))
        .collect::<Vec<_>>();
    if removed.is_empty() {
        return Ok(None);
    }
    let report = services.store.scan_read_only().map_err(|_| ())?;
    if !report.issues.is_empty() {
        return Err(());
    }
    let mut apps = Vec::new();
    for app in report.valid_apps {
        let revisions = [
            app.draft_revision,
            app.active_config_revision,
            app.pending_config_revision,
        ];
        let referenced = revisions.into_iter().flatten().any(|revision| {
            crate::app_store::config_revision::load_verified(
                &services.store.app_directory(app.app_id),
                revision,
                services.store.integrity_key().unwrap_or(&[]),
            )
            .map(|loaded| {
                loaded.metadata.binds.iter().any(|bind| {
                    let source = std::path::Path::new(&bind.source);
                    removed.iter().any(|root| source.starts_with(root))
                })
            })
            .unwrap_or(true)
        });
        if referenced {
            apps.push(app.slug);
        }
    }
    apps.sort();
    apps.dedup();
    Ok((!apps.is_empty()).then_some(apps))
}

async fn finish_error_with_apps(
    services: &M3Services,
    key: &str,
    code: &'static str,
    status: StatusCode,
    apps: &[String],
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::json!({
        "code": code,
        "message": "The bind root is referenced by application configuration",
        "referenced_by_apps": apps,
        "request_id": request_id.0,
    })
    .to_string();
    if services
        .idempotency
        .finish(ROUTE, key, status.as_u16(), &body, Some(code), request_id.0)
        .await
        .is_err()
    {
        return interrupt(services, key, request_id).await;
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

fn m3(state: &AppState, request_id: RequestId) -> Result<&Arc<M3Services>, ApiError> {
    state.m3.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "FEATURE_NOT_AVAILABLE",
            "The feature is not available",
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

fn replay(status: u16, body: String, request_id: RequestId) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(status).map_err(|_| ApiError::internal(request_id))?;
    let body = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("idempotency_replayed".into(), serde_json::Value::Bool(true));
            }
            value.to_string()
        }
        Err(_) => body,
    };
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn finish_json(
    services: &M3Services,
    key: &str,
    value: &SettingsResponse,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::to_string(value).map_err(|_| ApiError::internal(request_id))?;
    if services
        .idempotency
        .finish(ROUTE, key, 200, &body, None, request_id.0)
        .await
        .is_err()
    {
        return interrupt(services, key, request_id).await;
    }
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
}

async fn finish_error(
    services: &M3Services,
    key: &str,
    code: &'static str,
    status: StatusCode,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::json!({
        "code": code,
        "message": "The global settings changed concurrently",
        "request_id": request_id.0,
    })
    .to_string();
    if services
        .idempotency
        .finish(ROUTE, key, status.as_u16(), &body, Some(code), request_id.0)
        .await
        .is_err()
    {
        return interrupt(services, key, request_id).await;
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn interrupt(
    services: &M3Services,
    key: &str,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let _ = services
        .idempotency
        .mark_interrupted(ROUTE, key, request_id.0)
        .await;
    Err(ApiError::internal(request_id))
}
