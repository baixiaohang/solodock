use std::{collections::HashMap, path::Path};

use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use time::OffsetDateTime;

use super::{AppState, auth::Authenticated};
use crate::{
    docker::{DriftIssue, models::ProbeStatus},
    system::disk::{DiskSnapshot, DiskStatus, probe as disk_probe},
};

#[derive(Serialize)]
pub struct SystemHealthResponse {
    status: &'static str,
    docker: crate::docker::models::ProbeSnapshot,
    recovery: RecoveryHealth,
    disk: DiskHealth,
    streams: StreamHealth,
    projection: ProjectionHealth,
    deployments: DeploymentHealth,
    registry_credentials: CredentialHealth,
}

#[derive(Serialize)]
struct RecoveryHealth {
    status: &'static str,
    issue_count: usize,
    issues_by_code: HashMap<&'static str, usize>,
}

#[derive(Serialize)]
struct DiskHealth {
    state: DiskSnapshot,
    docker: Option<DiskSnapshot>,
}

#[derive(Serialize)]
struct StreamHealth {
    active: usize,
    limit: usize,
}

#[derive(Serialize)]
struct ProjectionHealth {
    status: &'static str,
}
#[derive(Serialize)]
struct DeploymentHealth {
    active: i64,
    interrupted: i64,
    needs_attention: i64,
    limit: usize,
}
#[derive(Serialize)]
struct CredentialHealth {
    status: &'static str,
    count: usize,
}

#[derive(Serialize)]
pub struct DriftResponse {
    complete: bool,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    issues: Vec<DriftIssue>,
}

pub async fn health(
    State(state): State<AppState>,
    _authenticated: Authenticated,
) -> impl IntoResponse {
    let docker = state.observer.supervisor.current().await;
    let state_disk = disk_probe(&state.state_directory);
    let docker_disk = docker
        .docker_root_directory
        .as_deref()
        .map(Path::new)
        .filter(|path| path.exists())
        .map(disk_probe);
    let issues = state.observer.catalog.recovery_issues();
    let recovery_degraded = !issues.is_empty();
    let disk_degraded = matches!(
        state_disk.status,
        DiskStatus::Warning | DiskStatus::Critical | DiskStatus::Unknown
    ) || docker_disk.as_ref().is_some_and(|disk| {
        matches!(
            disk.status,
            DiskStatus::Warning | DiskStatus::Critical | DiskStatus::Unknown
        )
    });
    let projection_degraded = state.m3.as_ref().is_some_and(|services| {
        services
            .projection_degraded
            .load(std::sync::atomic::Ordering::Acquire)
    });
    let (credential_status, credential_count) =
        state.m4.as_ref().map_or(("unavailable", 0), |services| {
            services
                .credentials
                .list()
                .map(|values| ("ok", values.len()))
                .unwrap_or(("degraded", 0))
        });
    let deployment_active = if let Some(services) = state.m4.as_ref() {
        services.ledger.active_count().await.unwrap_or(0)
    } else {
        0
    };
    let (deployment_interrupted, deployment_needs_attention) =
        if let Some(services) = state.m4.as_ref() {
            services.ledger.attention_counts().await.unwrap_or((0, 0))
        } else {
            (0, 0)
        };
    let degraded = docker.status != ProbeStatus::Ready
        || recovery_degraded
        || disk_degraded
        || projection_degraded
        || deployment_interrupted > 0
        || deployment_needs_attention > 0;
    Json(SystemHealthResponse {
        status: if degraded { "degraded" } else { "ok" },
        docker,
        recovery: RecoveryHealth {
            status: if recovery_degraded { "degraded" } else { "ok" },
            issue_count: issues.values().sum(),
            issues_by_code: issues,
        },
        disk: DiskHealth {
            state: state_disk,
            docker: docker_disk,
        },
        streams: StreamHealth {
            active: state.stream_gate.active(),
            limit: crate::api::streams::StreamGate::GLOBAL_LIMIT,
        },
        projection: ProjectionHealth {
            status: if projection_degraded {
                "degraded"
            } else {
                "ok"
            },
        },
        deployments: DeploymentHealth {
            active: deployment_active,
            interrupted: deployment_interrupted,
            needs_attention: deployment_needs_attention,
            limit: 1,
        },
        registry_credentials: CredentialHealth {
            status: credential_status,
            count: credential_count,
        },
    })
}

pub async fn drift(
    State(state): State<AppState>,
    _authenticated: Authenticated,
) -> impl IntoResponse {
    let snapshot = state.observer.snapshot().await;
    Json(DriftResponse {
        complete: snapshot.complete,
        observed_at: snapshot.observed_at,
        issues: snapshot.issues,
    })
}
