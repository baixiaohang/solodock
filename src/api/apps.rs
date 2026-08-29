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
    let catalog = state
        .observer
        .catalog
        .get(app_id)
        .ok_or_else(|| ApiError::app_not_found(request_id))?;
    let snapshot = state.observer.snapshot().await;
    let app = snapshot
        .apps
        .into_iter()
        .find(|app| app.id == app_id)
        .ok_or_else(|| ApiError::app_not_found(request_id))?;
    let nonterminal = if let Some(m4) = state.m4.as_ref() {
        m4.ledger
            .list(app_id, 1)
            .await
            .ok()
            .and_then(|values| values.into_iter().next())
            .is_some_and(|value| !value.status.is_terminal())
    } else {
        false
    };
    let actual_release = app.actual_release_id;
    let available_actions = if nonterminal {
        vec!["deletion_preview"]
    } else if let Some(pending) = catalog.pending_release_id {
        if actual_release == Some(pending) {
            vec!["stop", "deletion_preview"]
        } else {
            vec!["deploy", "deletion_preview"]
        }
    } else if let Some(active) = catalog.active_release_id {
        if actual_release == Some(active) {
            vec!["start", "stop", "restart", "deploy", "deletion_preview"]
        } else {
            vec!["deploy", "deletion_preview"]
        }
    } else {
        vec!["deploy", "deletion_preview"]
    };
    Ok(Json(AppDetailResponse {
        observation: app,
        draft: catalog.draft,
        draft_revision: catalog.draft_revision,
        draft_config_sha256: catalog.draft_config_sha256,
        active_config_revision: catalog.active_config_revision,
        pending_release_id: catalog.pending_release_id,
        pending_image_ref: catalog.pending_image_ref,
        desired_state: catalog.desired_state,
        deployment_status: if nonterminal {
            "RUNNING"
        } else if catalog.pending_release_id.is_some() {
            "PENDING"
        } else if catalog.active_release_id.is_some() {
            "ACTIVE"
        } else {
            "DEPLOY_REQUIRED"
        },
        available_actions,
        compose_available: state.m3.as_ref().is_some_and(|m3| {
            m3.compose_capability.current() == crate::compose::ComposeStatus::Ready
        }),
    }))
}

#[derive(Serialize)]
struct AppDetailResponse {
    #[serde(flatten)]
    observation: AppObservation,
    draft: Option<crate::domain::dto::DraftResponse>,
    draft_revision: Option<Uuid>,
    draft_config_sha256: Option<String>,
    active_config_revision: Option<Uuid>,
    pending_release_id: Option<Uuid>,
    pending_image_ref: Option<String>,
    desired_state: crate::domain::DesiredState,
    deployment_status: &'static str,
    available_actions: Vec<&'static str>,
    compose_available: bool,
}
