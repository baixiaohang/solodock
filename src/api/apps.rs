use axum::{
    Json,
    extract::{Extension, Path, State},
    response::IntoResponse,
};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{AppState, auth::Authenticated};
use crate::{
    docker::{
        ActiveRelease, AppObservation, DriftCode,
        models::{ContainerStatus, HealthStatus, ProbeStatus},
    },
    error::{ApiError, RequestId},
};

#[derive(Serialize)]
pub struct AppsResponse {
    docker_status: ProbeStatus,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    apps: Vec<AppListItem>,
}

#[derive(Serialize)]
struct AppListItem {
    id: Uuid,
    slug: String,
    display_name: String,
    active_release: Option<ActiveRelease>,
    actual: Option<ActualSummary>,
    drift_codes: Vec<DriftCode>,
}

#[derive(Serialize)]
struct ActualSummary {
    status: ContainerStatus,
    health: HealthStatus,
    container_id: String,
    image_ref: Option<String>,
}

impl From<AppObservation> for AppListItem {
    fn from(value: AppObservation) -> Self {
        let actual = value.actual.map(|container| ActualSummary {
            status: container.status,
            health: container.health,
            container_id: container.id,
            image_ref: container.configured_image_ref,
        });
        Self {
            id: value.id,
            slug: value.slug,
            display_name: value.display_name,
            active_release: value.active_release,
            actual,
            drift_codes: value.drift_codes,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    _authenticated: Authenticated,
) -> impl IntoResponse {
    let snapshot = state.observer.snapshot().await;
    Json(AppsResponse {
        docker_status: snapshot.docker_status,
        observed_at: snapshot.observed_at,
        apps: snapshot.apps.into_iter().map(AppListItem::from).collect(),
    })
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    _authenticated: Authenticated,
) -> Result<impl IntoResponse, ApiError> {
    if state.observer.catalog.get(app_id).is_none() {
        return Err(ApiError::app_not_found(request_id));
    }
    let snapshot = state.observer.snapshot().await;
    let app = snapshot
        .apps
        .into_iter()
        .find(|app| app.id == app_id)
        .expect("catalog app is in snapshot");
    Ok(Json(app))
}
