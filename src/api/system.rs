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
    memory: crate::system::memory::MemorySnapshot,
    disk: DiskHealth,
    streams: StreamHealth,
    projection: ProjectionHealth,
    storage_cleanup: StorageCleanupHealth,
    deployments: DeploymentHealth,
    registry_credentials: CredentialHealth,
    polling: PollingHealth,
    webhooks: WebhookHealth,
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
struct StorageCleanupHealth {
    status: &'static str,
    pending_operations: usize,
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
struct PollingHealth {
    coordinator: crate::registry::PollHealthSnapshot,
    store_status: &'static str,
    enabled: i64,
    suppressed: i64,
    app_errors: i64,
}

#[derive(Serialize)]
struct WebhookHealth {
    status: &'static str,
    configured: usize,
    replay_records: i64,
}

#[derive(Serialize)]
pub struct DriftResponse {
    complete: bool,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    issues: Vec<DriftIssue>,
}

pub async fn installation_identity(_authenticated: Authenticated) -> impl IntoResponse {
    Json(crate::system::installation::read())
}

pub async fn health(
    State(state): State<AppState>,
    _authenticated: Authenticated,
) -> impl IntoResponse {
    let docker = state.observer.supervisor.current().await;
    let state_disk = disk_probe(&state.state_directory);
    let memory = crate::system::memory::probe();
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
    let memory_degraded = memory.available_bytes.is_none();
    let projection_degraded = state.m3.as_ref().is_some_and(|services| {
        services
            .projection_degraded
            .load(std::sync::atomic::Ordering::Acquire)
    });
    let (storage_cleanup_status, storage_cleanup_pending) = match state.m3.as_ref() {
        Some(services) => match async {
            let artifacts = crate::storage_cleanup::pending_operation_count(
                &services.store,
                &services.database,
            )
            .await?;
            let images = crate::image_cleanup::pending_operation_count(&services.database).await?;
            Ok::<_, crate::storage_cleanup::CleanupError>(artifacts + images)
        }
        .await
        {
            Ok(0) => ("ok", 0),
            Ok(pending) => ("pending", pending),
            Err(_) => ("degraded", 0),
        },
        None => ("unavailable", 0),
    };
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
    let (poll_snapshot, poll_counts, poll_store_degraded) =
        if let Some(services) = state.m4.as_ref() {
            match services.poller.store.counts().await {
                Ok(counts) => (services.poller.health.snapshot(), counts, false),
                Err(_) => (services.poller.health.snapshot(), (0, 0, 0), true),
            }
        } else {
            (
                crate::registry::PollHealth::default().snapshot(),
                (0, 0, 0),
                true,
            )
        };
    let degraded = docker.status != ProbeStatus::Ready
        || recovery_degraded
        || memory_degraded
        || disk_degraded
        || projection_degraded
        || matches!(storage_cleanup_status, "pending" | "degraded")
        || deployment_interrupted > 0
        || deployment_needs_attention > 0;
    let degraded = degraded || poll_snapshot.status == "degraded" || poll_store_degraded;
    let (webhook_status, webhook_configured, webhook_replays) = match state.webhooks.as_ref() {
        None => ("disabled", 0, 0),
        Some(services) if services.origin.is_empty() => match services.store.configured_count() {
            Ok(configured) => ("disabled", configured, 0),
            Err(_) => ("degraded", 0, 0),
        },
        Some(services) => match (
            services.store.configured_count(),
            services.poll_states.webhook_replay_count().await,
        ) {
            (Ok(configured), Ok(replays)) => ("ok", configured, replays),
            _ => ("degraded", 0, 0),
        },
    };
    let degraded = degraded || webhook_status == "degraded";
    Json(SystemHealthResponse {
        status: if degraded { "degraded" } else { "ok" },
        docker,
        recovery: RecoveryHealth {
            status: if recovery_degraded { "degraded" } else { "ok" },
            issue_count: issues.values().sum(),
            issues_by_code: issues,
        },
        memory,
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
        storage_cleanup: StorageCleanupHealth {
            status: storage_cleanup_status,
            pending_operations: storage_cleanup_pending,
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
        polling: PollingHealth {
            coordinator: poll_snapshot,
            store_status: if poll_store_degraded {
                "degraded"
            } else {
                "ok"
            },
            enabled: poll_counts.0,
            suppressed: poll_counts.1,
            app_errors: poll_counts.2,
        },
        webhooks: WebhookHealth {
            status: webhook_status,
            configured: webhook_configured,
            replay_records: webhook_replays,
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
