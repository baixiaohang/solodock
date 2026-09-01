use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
};
use serde::Deserialize;
use uuid::Uuid;

use super::{AppState, auth::Authenticated};
use crate::{
    error::{ApiError, RequestId},
    registry::{ImageConfigSuggestion, ImageReference, Platform},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectImageRequest {
    discovery_image_ref: String,
    #[serde(default)]
    credential_ref: Option<Uuid>,
}

pub async fn inspect(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    _authenticated: Authenticated,
    payload: Result<Json<InspectImageRequest>, JsonRejection>,
) -> Result<Json<ImageConfigSuggestion>, ApiError> {
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let image = ImageReference::parse(&payload.discovery_image_ref)
        .map_err(|error| ApiError::registry(error.public_code(), request_id))?;
    let m4 = state
        .m4
        .as_deref()
        .ok_or_else(|| ApiError::compose("FEATURE_NOT_AVAILABLE", request_id))?;
    let probe = state.observer.supervisor.current().await;
    let platform = Platform::canonical(
        probe
            .os
            .as_deref()
            .ok_or_else(|| ApiError::compose("DOCKER_API_INCOMPATIBLE", request_id))?,
        probe
            .architecture
            .as_deref()
            .ok_or_else(|| ApiError::compose("DOCKER_API_INCOMPATIBLE", request_id))?,
        None,
    )
    .map_err(|_| ApiError::compose("DOCKER_API_INCOMPATIBLE", request_id))?;
    let credential = payload
        .credential_ref
        .map(|id| m4.credentials.load(id))
        .transpose()
        .map_err(|_| ApiError::registry("REGISTRY_CREDENTIAL_INVALID", request_id))?;
    if credential
        .as_ref()
        .is_some_and(|value| value.metadata.registry != image.logical_registry)
    {
        return Err(ApiError::registry(
            "REGISTRY_CREDENTIAL_MISMATCH",
            request_id,
        ));
    }
    let suggestion = m4
        .engine
        .resolver
        .inspect_config(
            &image,
            &platform,
            credential
                .as_ref()
                .map(|value| (value.metadata.username.as_str(), &value.secret)),
        )
        .await
        .map_err(|error| ApiError::registry(error.public_code(), request_id))?;
    Ok(Json(suggestion))
}
