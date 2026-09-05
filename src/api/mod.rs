pub mod apps;
pub mod assets;
pub mod auth;
pub mod deployments;
pub mod image_inspection;
pub mod middleware;
pub mod mutations;
pub mod presets;
pub mod settings;
pub mod storage_cleanup;
pub mod streams;
pub mod system;
pub mod webhooks;

use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    middleware as axum_middleware,
    routing::{delete, get, post, put},
};
use serde::Serialize;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    auth::AuthService,
    config::{CanonicalAuthority, origin_authority},
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
    pub management_authority: CanonicalAuthority,
    pub webhook_authority: Option<CanonicalAuthority>,
    pub local_probe_authority: CanonicalAuthority,
    pub observer: DockerObserver,
    pub events: DockerEventHub,
    pub stats: StatsHub,
    pub stream_gate: streams::StreamGate,
    pub redactor: SecretRedactor,
    pub state_directory: PathBuf,
    pub shutdown: CancellationToken,
    pub stream_tasks: TaskTracker,
    pub m3: Option<Arc<mutations::M3Services>>,
    pub m4: Option<Arc<deployments::M4Services>>,
    pub webhooks: Option<Arc<crate::webhook::WebhookServices>>,
}

impl AppState {
    pub fn control_plane(
        auth: AuthService,
        public_origin: String,
        state_directory: PathBuf,
    ) -> Self {
        let management_authority = CanonicalAuthority::parse_http(
            &origin_authority(&public_origin).expect("test public origin must be canonical"),
        )
        .expect("test public authority must be canonical");
        let api = Arc::new(UnavailableDocker);
        let supervisor = DockerSupervisor::new();
        let shutdown = CancellationToken::new();
        let stream_tasks = TaskTracker::new();
        Self {
            auth,
            public_origin,
            local_probe_authority: management_authority.clone(),
            management_authority,
            webhook_authority: None,
            observer: DockerObserver::new(api.clone(), AppCatalog::default(), supervisor),
            events: DockerEventHub::new(),
            stats: StatsHub::new(api, shutdown.clone(), stream_tasks.clone()),
            stream_gate: streams::StreamGate::default(),
            redactor: SecretRedactor::new(&EmptySecretProvider),
            state_directory,
            shutdown,
            stream_tasks,
            m3: None,
            m4: None,
            webhooks: None,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let auth = Router::new()
        .route("/api/v1/auth/bootstrap", post(auth::bootstrap))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/me/sessions/revoke-all", post(auth::revoke_all))
        .route("/api/v1/me/password", put(auth::change_password))
        .layer(DefaultBodyLimit::max(16 * 1024));
    let draft_mutations = Router::new()
        .route("/api/v1/apps", post(mutations::create))
        .route("/api/v1/apps/{id}/draft", put(mutations::update_draft))
        .route("/api/v1/apps/{id}/validate", post(mutations::validate))
        .layer(DefaultBodyLimit::max(crate::domain::MAX_BODY_BYTES));
    let lifecycle_mutations = Router::new()
        .route("/api/v1/apps/{id}/actions/start", post(mutations::start))
        .route("/api/v1/apps/{id}/actions/stop", post(mutations::stop))
        .route(
            "/api/v1/apps/{id}/actions/restart",
            post(mutations::restart),
        )
        .layer(DefaultBodyLimit::max(0));
    let delete_mutations = Router::new()
        .route(
            "/api/v1/apps/{id}/deletion-preview",
            post(mutations::deletion_preview),
        )
        .route("/api/v1/apps/{id}", delete(mutations::delete_app))
        .layer(DefaultBodyLimit::max(16 * 1024));
    let m4_mutations = Router::new()
        .route(
            "/api/v1/registry-credentials",
            post(deployments::create_credential),
        )
        .route(
            "/api/v1/registry-credentials/{id}",
            put(deployments::update_credential).delete(deployments::delete_credential),
        )
        .route(
            "/api/v1/apps/{id}/deployments",
            post(deployments::schedule).get(deployments::list),
        )
        .route("/api/v1/deployments/{id}", get(deployments::detail))
        .route(
            "/api/v1/deployments/{id}/rollback",
            post(deployments::rollback),
        )
        .layer(DefaultBodyLimit::max(16 * 1024));
    let webhook_admin = Router::new()
        .route(
            "/api/v1/apps/{id}/webhook",
            get(webhooks::status)
                .put(webhooks::configure)
                .delete(webhooks::revoke),
        )
        .layer(DefaultBodyLimit::max(16 * 1024));
    let settings = Router::new()
        .route("/api/v1/settings", get(settings::get).put(settings::update))
        .layer(DefaultBodyLimit::max(16 * 1024));
    let presets = Router::new()
        .route("/api/v1/app-presets", get(presets::list))
        .route("/api/v1/apps/from-preset", post(presets::create))
        .layer(DefaultBodyLimit::max(16 * 1024));
    let image_inspection = Router::new()
        .route(
            "/api/v1/images/inspect-config",
            post(image_inspection::inspect),
        )
        .layer(DefaultBodyLimit::max(16 * 1024));
    let storage_cleanup = Router::new()
        .route(
            "/api/v1/system/storage-cleanup/preview",
            post(storage_cleanup::preview),
        )
        .route(
            "/api/v1/system/storage-cleanup/apply",
            post(storage_cleanup::apply),
        )
        .layer(DefaultBodyLimit::max(16 * 1024));
    let public_webhook = Router::new()
        .route(
            "/hooks/v1/apps/{id}/registry",
            post(crate::webhook::ingress::receive),
        )
        .layer(DefaultBodyLimit::max(
            crate::webhook::protocol::MAX_BODY_BYTES,
        ));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/me", get(auth::me))
        .route("/api/v1/system/health", get(system::health))
        .route(
            "/api/v1/system/installation",
            get(system::installation_identity),
        )
        .route("/api/v1/system/drift", get(system::drift))
        .route("/api/v1/apps", get(apps::list))
        .route("/api/v1/apps/{id}", get(apps::detail))
        .route("/api/v1/apps/{id}/events", get(streams::events))
        .route("/api/v1/apps/{id}/logs", get(streams::logs))
        .route("/api/v1/apps/{id}/stats", get(streams::stats))
        .route(
            "/api/v1/registry-credentials",
            get(deployments::list_credentials),
        )
        .merge(auth)
        .merge(draft_mutations)
        .merge(lifecycle_mutations)
        .merge(delete_mutations)
        .merge(m4_mutations)
        .merge(webhook_admin)
        .merge(settings)
        .merge(presets)
        .merge(image_inspection)
        .merge(storage_cleanup)
        .merge(public_webhook)
        .fallback(assets::serve)
        .with_state(state.clone())
        .layer(axum_middleware::from_fn_with_state(
            state,
            middleware::host_isolation,
        ))
        .layer(axum_middleware::from_fn(middleware::request_context))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}
