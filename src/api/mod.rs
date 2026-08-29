pub mod apps;
pub mod auth;
pub mod middleware;
pub mod streams;
pub mod system;

use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    middleware as axum_middleware,
    routing::{get, post},
};
use serde::Serialize;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    auth::AuthService,
    docker::{
        AppCatalog, DockerObserver,
        events::DockerEventHub,
        logs::{EmptySecretProvider, SecretRedactor},
        models::UnavailableDocker,
        probe::DockerSupervisor,
        stats::StatsHub,
    },
};

#[derive(Clone)]
pub struct AppState {
    pub auth: AuthService,
    pub public_origin: String,
    pub observer: DockerObserver,
    pub events: DockerEventHub,
    pub stats: StatsHub,
    pub stream_gate: streams::StreamGate,
    pub redactor: SecretRedactor,
    pub state_directory: PathBuf,
    pub shutdown: CancellationToken,
    pub stream_tasks: TaskTracker,
}

impl AppState {
    pub fn control_plane(
        auth: AuthService,
        public_origin: String,
        state_directory: PathBuf,
    ) -> Self {
        let api = Arc::new(UnavailableDocker);
        let supervisor = DockerSupervisor::new();
        let shutdown = CancellationToken::new();
        let stream_tasks = TaskTracker::new();
        Self {
            auth,
            public_origin,
            observer: DockerObserver::new(api.clone(), AppCatalog::default(), supervisor),
            events: DockerEventHub::new(),
            stats: StatsHub::new(api, shutdown.clone(), stream_tasks.clone()),
            stream_gate: streams::StreamGate::default(),
            redactor: SecretRedactor::new(&EmptySecretProvider),
            state_directory,
            shutdown,
            stream_tasks,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/auth/bootstrap", post(auth::bootstrap))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/me", get(auth::me))
        .route("/api/v1/me/sessions/revoke-all", post(auth::revoke_all))
        .route("/api/v1/system/health", get(system::health))
        .route("/api/v1/system/drift", get(system::drift))
        .route("/api/v1/apps", get(apps::list))
        .route("/api/v1/apps/{id}", get(apps::detail))
        .route("/api/v1/apps/{id}/events", get(streams::events))
        .route("/api/v1/apps/{id}/logs", get(streams::logs))
        .route("/api/v1/apps/{id}/stats", get(streams::stats))
        .with_state(state)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum_middleware::from_fn(middleware::request_context))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}
