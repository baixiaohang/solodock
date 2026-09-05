mod support;

use std::{
    collections::{HashMap, VecDeque},
    fs,
    os::unix::fs::PermissionsExt,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solodock::{
    AppState,
    api::{deployments::M4Services, mutations::M3Services},
    app_store::AppStore,
    compose::{ComposeAction, ComposeCapability, ComposeOutput, ComposeRunner, RunContext},
    db::Database,
    deploy::{
        DeploymentEngine, DeploymentLedger, DeploymentScheduler, FixedImagePuller, HealthVerifier,
        ImagePuller, PullError,
    },
    docker::{
        AppCatalog, DockerObserver,
        models::{
            ContainerRecord, ContainerStatus, DockerError, DockerReadApi, DockerStream,
            HealthStatus, LogChunk, LogRequest, ProbeSnapshot, ProbeStatus, RawDockerEvent,
            RawStats,
        },
        ownership::{
            APP_ID_LABEL, MANAGED_LABEL, ONEOFF_LABEL, PROJECT_LABEL, RELEASE_ID_LABEL,
            SCHEMA_LABEL, SERVICE_LABEL,
        },
        probe::DockerSupervisor,
    },
    mutation::{AppMutationCoordinator, IdempotencyService},
    registry::{CredentialStore, PollCoordinator, PollStateStore, RegistryResolver},
    router,
    security::secret::SecretValue,
    webhook::{WebhookRateLimiter, WebhookServices, WebhookStore},
};
use tempfile::TempDir;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Default)]
struct FakeCompose {
    validated_yaml: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    actions: Arc<std::sync::Mutex<Vec<ComposeAction>>>,
    contexts: Arc<std::sync::Mutex<Vec<(ComposeAction, u16)>>>,
    stop_scenario: Option<Arc<StopScenario>>,
}

struct StopScenario {
    source: std::path::PathBuf,
    replacement: Option<std::path::PathBuf>,
    mutate_on_stop: usize,
    stop_calls: std::sync::atomic::AtomicUsize,
    phase: std::sync::atomic::AtomicUsize,
}

impl StopScenario {
    fn apply(&self, action: ComposeAction) {
        use std::sync::atomic::Ordering;
        match action {
            ComposeAction::Stop => {
                let stop = self.stop_calls.fetch_add(1, Ordering::SeqCst) + 1;
                self.phase
                    .store(if stop == 1 { 1 } else { 3 }, Ordering::SeqCst);
                if stop == self.mutate_on_stop {
                    let displaced = self.source.with_extension(format!("stopped-{stop}"));
                    fs::rename(&self.source, displaced).unwrap();
                    if let Some(target) = &self.replacement {
                        std::os::unix::fs::symlink(target, &self.source).unwrap();
                    } else {
                        fs::create_dir(&self.source).unwrap();
                        fs::set_permissions(&self.source, fs::Permissions::from_mode(0o700))
                            .unwrap();
                    }
                }
            }
            ComposeAction::DeployCandidate => self.phase.store(2, Ordering::SeqCst),
            ComposeAction::Start => self.phase.store(4, Ordering::SeqCst),
            _ => {}
        }
    }
}

struct MissingDocker;

struct SettingsDocker {
    docker_root_directory: Option<String>,
}

#[derive(Default)]
struct ScriptedDocker {
    candidates: std::sync::Mutex<VecDeque<Vec<ContainerRecord>>>,
    resource_inspects: std::sync::atomic::AtomicUsize,
    block_resource_inspect_at: std::sync::atomic::AtomicUsize,
    resource_inspect_entered: tokio::sync::Notify,
    resource_inspect_release: tokio::sync::Notify,
}

impl ScriptedDocker {
    fn set(&self, states: Vec<Vec<ContainerRecord>>) {
        *self.candidates.lock().unwrap() = states.into();
    }

    fn next(&self) -> Vec<ContainerRecord> {
        let mut states = self.candidates.lock().unwrap();
        if states.len() > 1 {
            states.pop_front().unwrap()
        } else {
            states.front().cloned().unwrap_or_default()
        }
    }

    fn block_resource_inspect_after(&self, additional_calls: usize) {
        let current = self
            .resource_inspects
            .load(std::sync::atomic::Ordering::SeqCst);
        self.block_resource_inspect_at.store(
            current + additional_calls,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    async fn wait_for_blocked_resource_inspect(&self) {
        self.resource_inspect_entered.notified().await;
    }

    fn release_resource_inspect(&self) {
        self.resource_inspect_release.notify_one();
    }

    async fn resource_inspect_gate(&self) {
        let call = self
            .resource_inspects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if call
            == self
                .block_resource_inspect_at
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.resource_inspect_entered.notify_one();
            self.resource_inspect_release.notified().await;
        }
    }
}

#[async_trait]
impl DockerReadApi for MissingDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        Ok(ready_probe())
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(Vec::new())
    }
    async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
        Err(DockerError::new(
            solodock::docker::models::DockerErrorKind::ContainerChanged,
        ))
    }
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn inspect_volume(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerResource>, DockerError> {
        Ok(None)
    }
    async fn inspect_network(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerNetworkResource>, DockerError> {
        Ok(None)
    }
}

#[async_trait]
impl DockerReadApi for SettingsDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        let mut probe = ready_probe();
        probe.docker_root_directory = self.docker_root_directory.clone();
        Ok(probe)
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(Vec::new())
    }
    async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
        Err(DockerError::new(
            solodock::docker::models::DockerErrorKind::ContainerChanged,
        ))
    }
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

#[async_trait]
impl DockerReadApi for ScriptedDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        Ok(ready_probe())
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(self.next())
    }
    async fn list_compose_app_containers(
        &self,
        _project_name: &str,
    ) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(self.next())
    }
    async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
        Err(DockerError::new(
            solodock::docker::models::DockerErrorKind::ContainerChanged,
        ))
    }
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn inspect_volume(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerResource>, DockerError> {
        self.resource_inspect_gate().await;
        Ok(None)
    }
    async fn inspect_network(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerNetworkResource>, DockerError> {
        self.resource_inspect_gate().await;
        Ok(None)
    }
}

fn owned_container(id: char, app_id: Uuid, release_id: Uuid, managed: bool) -> ContainerRecord {
    ContainerRecord {
        id: id.to_string().repeat(64),
        name: "app".into(),
        labels: HashMap::from([
            (MANAGED_LABEL.into(), managed.to_string()),
            (SCHEMA_LABEL.into(), "1".into()),
            (APP_ID_LABEL.into(), app_id.to_string()),
            (RELEASE_ID_LABEL.into(), release_id.to_string()),
            (PROJECT_LABEL.into(), "solodock-example".into()),
            (SERVICE_LABEL.into(), "app".into()),
            (ONEOFF_LABEL.into(), "False".into()),
        ]),
        status: ContainerStatus::Running,
        health: HealthStatus::None,
        exit_code: None,
        restart_count: Some(0),
        started_at: Some("2026-08-29T00:00:00Z".into()),
        finished_at: None,
        configured_image_ref: None,
        image_id: None,
        manifest_descriptor: None,
        ports: vec![],
        mounts: vec![],
        networks: vec![],
    }
}

fn ready_probe() -> ProbeSnapshot {
    ProbeSnapshot {
        status: ProbeStatus::Ready,
        error_code: None,
        server_version: Some("27.0".into()),
        api_version: Some("1.46".into()),
        os: Some("linux".into()),
        architecture: Some("amd64".into()),
        observed_at: time::OffsetDateTime::now_utc(),
        docker_root_directory: None,
    }
}

#[async_trait]
impl ComposeRunner for FakeCompose {
    async fn run(
        &self,
        action: ComposeAction,
        context: RunContext,
    ) -> Result<ComposeOutput, solodock::compose::ComposeError> {
        self.actions.lock().unwrap().push(action);
        self.contexts
            .lock()
            .unwrap()
            .push((action, context.stop_grace_period_seconds));
        if let Some(scenario) = &self.stop_scenario {
            scenario.apply(action);
        }
        match action {
            ComposeAction::Version => Ok(ComposeOutput {
                stdout: b"2.24.0\n".to_vec(),
                stderr: Vec::new(),
            }),
            ComposeAction::Validate => {
                self.validated_yaml
                    .lock()
                    .unwrap()
                    .replace(fs::read(context.compose_file).unwrap());
                Ok(ComposeOutput {
                    stdout: vec![],
                    stderr: vec![],
                })
            }
            _ => Ok(ComposeOutput {
                stdout: vec![],
                stderr: vec![],
            }),
        }
    }
}

#[derive(Default)]
struct NoopPuller;

#[async_trait]
impl ImagePuller for NoopPuller {
    async fn pull(
        &self,
        _deployment_id: Uuid,
        _resolved: &solodock::registry::ResolvedImage,
        _credential: Option<&solodock::registry::LoadedCredential>,
        _redaction: Vec<Vec<u8>>,
    ) -> Result<(), PullError> {
        Ok(())
    }
}

#[derive(Default)]
struct ScenarioDocker {
    scenario: Option<Arc<StopScenario>>,
    records: std::sync::Mutex<Option<(ContainerRecord, ContainerRecord)>>,
}

impl ScenarioDocker {
    fn install(&self, active: ContainerRecord, candidate: ContainerRecord) {
        *self.records.lock().unwrap() = Some((active, candidate));
    }

    fn observed(&self) -> Vec<ContainerRecord> {
        let Some((active, candidate)) = self.records.lock().unwrap().clone() else {
            return Vec::new();
        };
        let phase = self
            .scenario
            .as_ref()
            .map(|scenario| scenario.phase.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or_default();
        let mut value = match phase {
            0 | 4 => active,
            1 => active,
            2 | 3 => candidate,
            _ => return Vec::new(),
        };
        if matches!(phase, 1 | 3) {
            value.status = ContainerStatus::Exited;
        }
        vec![value]
    }
}

#[async_trait]
impl DockerReadApi for ScenarioDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        Ok(ready_probe())
    }

    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(self.observed())
    }

    async fn list_compose_app_containers(
        &self,
        _project_name: &str,
    ) -> Result<Vec<ContainerRecord>, DockerError> {
        Ok(self.observed())
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerRecord, DockerError> {
        self.observed()
            .into_iter()
            .find(|container| container.id == id)
            .ok_or_else(|| {
                DockerError::new(solodock::docker::models::DockerErrorKind::ContainerChanged)
            })
    }

    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }

    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }

    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }

    async fn inspect_volume(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerResource>, DockerError> {
        Ok(None)
    }

    async fn inspect_network(
        &self,
        _name: &str,
    ) -> Result<Option<solodock::docker::models::DockerNetworkResource>, DockerError> {
        Ok(None)
    }
}

struct Harness {
    _root: TempDir,
    app: axum::Router,
    database: Database,
    cookie: String,
    csrf: String,
    apps: std::path::PathBuf,
    store: AppStore,
    idempotency: IdempotencyService,
    catalog: AppCatalog,
    state: AppState,
    validated_yaml: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    compose_actions: Arc<std::sync::Mutex<Vec<ComposeAction>>>,
    compose_contexts: Arc<std::sync::Mutex<Vec<(ComposeAction, u16)>>>,
}

impl Harness {
    async fn restart_cleanup_router(&mut self) {
        let old = self.state.m3.as_ref().unwrap();
        let database = Database::open(&self.state.state_directory.join("state.sqlite3"))
            .await
            .unwrap();
        let idempotency =
            IdempotencyService::initialize(database.clone(), &self.state.state_directory).unwrap();
        idempotency.interrupt_pending().await.unwrap();
        let store = AppStore::initialize_managed(
            self.apps.clone(),
            idempotency.integrity_key(),
            self.store.allowed_bind_roots(),
        )
        .unwrap();
        solodock::storage_cleanup::finalize_succeeded(&store, &database)
            .await
            .unwrap();
        let report = store.scan().unwrap();
        database.refresh_app_index(&report).await.unwrap();
        self.state.m3 = Some(Arc::new(M3Services {
            store: store.clone(),
            database: database.clone(),
            allowed_bind_roots: old.allowed_bind_roots.clone(),
            runtime_directory: old.runtime_directory.clone(),
            idempotency: idempotency.clone(),
            coordinator: AppMutationCoordinator::new(old.runtime_directory.clone()).unwrap(),
            compose: old.compose.clone(),
            compose_capability: old.compose_capability.clone(),
            projection_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_notify: Arc::new(tokio::sync::Notify::new()),
            publication_lock: Arc::new(tokio::sync::Mutex::new(())),
        }));
        self.database = database;
        self.idempotency = idempotency;
        self.store = store;
        self.app = router(self.state.clone());
    }

    async fn new() -> Self {
        Self::new_with_docker(Arc::new(MissingDocker)).await
    }

    async fn new_with_docker(docker: Arc<dyn DockerReadApi>) -> Self {
        Self::new_with_components(docker, None, None).await
    }

    async fn new_with_components(
        docker: Arc<dyn DockerReadApi>,
        stop_scenario: Option<Arc<StopScenario>>,
        puller: Option<Arc<dyn ImagePuller>>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state_directory = root.path().join("state");
        let runtime_directory = root.path().join("runtime");
        let apps = state_directory.join("apps");
        for path in [&state_directory, &runtime_directory, &apps] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let database = Database::open(&state_directory.join("state.sqlite3"))
            .await
            .unwrap();
        let test_auth = support::seed_authenticated_session(
            &database,
            runtime_directory.join("bootstrap.token"),
        )
        .await;
        let auth = test_auth.service;
        let cookie = test_auth.cookie;
        let csrf = test_auth.csrf;
        let idempotency =
            IdempotencyService::initialize(database.clone(), &state_directory).unwrap();
        let store =
            AppStore::initialize_verified(apps.clone(), idempotency.integrity_key()).unwrap();
        let fake_compose = Arc::new(FakeCompose {
            stop_scenario,
            ..FakeCompose::default()
        });
        let validated_yaml = fake_compose.validated_yaml.clone();
        let compose_actions = fake_compose.actions.clone();
        let compose_contexts = fake_compose.contexts.clone();
        let compose: Arc<dyn ComposeRunner> = fake_compose;
        let capability = ComposeCapability::default();
        capability.probe(compose.as_ref()).await;
        let mut state = AppState::control_plane(
            auth,
            "https://solodock.example.com".into(),
            state_directory.clone(),
        );
        let catalog = AppCatalog::default();
        state.observer = DockerObserver::new(
            docker.clone(),
            catalog.clone(),
            DockerSupervisor::from_snapshot(ready_probe()),
        );
        state.m3 = Some(Arc::new(M3Services {
            store: store.clone(),
            database: database.clone(),
            allowed_bind_roots: Vec::new(),
            runtime_directory: runtime_directory.clone(),
            idempotency: idempotency.clone(),
            coordinator: AppMutationCoordinator::new(runtime_directory.clone()).unwrap(),
            compose,
            compose_capability: capability,
            projection_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_notify: Arc::new(tokio::sync::Notify::new()),
            publication_lock: Arc::new(tokio::sync::Mutex::new(())),
        }));
        let credentials = CredentialStore::initialize(
            state_directory.join("registry-credentials"),
            idempotency.integrity_key(),
        )
        .unwrap();
        let ledger = DeploymentLedger::new(database.clone());
        let puller: Arc<dyn ImagePuller> = match puller {
            Some(puller) => puller,
            None => Arc::new(
                FixedImagePuller::new(
                    state_directory.clone(),
                    runtime_directory.join("pull"),
                    docker.clone(),
                    state.shutdown.clone(),
                    state.stream_tasks.clone(),
                )
                .unwrap(),
            ),
        };
        let engine = DeploymentEngine {
            store: store.clone(),
            credentials: credentials.clone(),
            resolver: RegistryResolver::production().unwrap(),
            ledger: ledger.clone(),
            puller,
            compose: state.m3.as_ref().unwrap().compose.clone(),
            docker,
            health: HealthVerifier::new(state.observer.api(), state.shutdown.clone()),
            shutdown: state.shutdown.clone(),
            tasks: state.stream_tasks.clone(),
            #[cfg(feature = "docker-e2e")]
            test_effect_gate: None,
            #[cfg(feature = "docker-e2e")]
            test_candidate_gate: None,
        };
        let poller = PollCoordinator::new(
            PollStateStore::new(database.clone()),
            state.shutdown.clone(),
            TaskTracker::new(),
        );
        state.m4 = Some(Arc::new(M4Services {
            credentials,
            ledger,
            scheduler: DeploymentScheduler::new(engine.clone()),
            engine,
            poller: poller.clone(),
        }));
        state.webhooks = Some(Arc::new(WebhookServices {
            origin: "https://hooks.example.com".into(),
            store: WebhookStore::new(store.clone(), idempotency.integrity_key()),
            poll_states: poller.store.clone(),
            database: database.clone(),
            notify: poller.notify.clone(),
            limiter: WebhookRateLimiter::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(16)),
        }));
        state.webhook_authority =
            Some(solodock::config::CanonicalAuthority::parse_http("hooks.example.com").unwrap());
        let app = router(state.clone());
        Self {
            _root: root,
            app,
            database,
            cookie,
            csrf,
            apps,
            store,
            idempotency,
            catalog,
            state,
            validated_yaml,
            compose_actions,
            compose_contexts,
        }
    }

    async fn create(&self, key: Option<&str>, input: &Value) -> axum::response::Response {
        let slug = input.get("slug").cloned().unwrap_or(Value::Null);
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/apps")
            .header(header::HOST, "solodock.example.com")
            .header(header::ORIGIN, "https://solodock.example.com")
            .header(header::COOKIE, &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", format!("create-{key}"));
        }
        let created = self
            .app
            .clone()
            .oneshot(
                request
                    .body(Body::from(json!({"slug": slug}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let Some(key) = key else { return created };
        let (status, response) = body(created).await;
        if status != StatusCode::CREATED {
            return axum::response::Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(response))
                .unwrap();
        }
        let created: Value = serde_json::from_str(&response).unwrap();
        let app_id = created["app"]["id"].as_str().unwrap();
        let updated = self
            .mutate(
                "PUT",
                &format!("/api/v1/apps/{app_id}/draft"),
                Some(key),
                &json!({"expected_revision": null, "draft": mutable(input.clone())}),
            )
            .await;
        let (status, response) = body(updated).await;
        axum::response::Response::builder()
            .status(if status == StatusCode::OK {
                StatusCode::CREATED
            } else {
                status
            })
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(response))
            .unwrap()
    }

    async fn mutate(
        &self,
        method: &str,
        uri: &str,
        key: Option<&str>,
        value: &Value,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "solodock.example.com")
            .header(header::ORIGIN, "https://solodock.example.com")
            .header(header::COOKIE, &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap()
    }

    fn publish_active(&self, app_id: Uuid, release_id: Uuid, image: &str) {
        let metadata = self.store.read_metadata(app_id).unwrap();
        let digest = image.split('@').nth(1).unwrap().to_owned();
        self.store
            .publish_v2_release(
                &metadata,
                release_id,
                &solodock::registry::ResolvedImage {
                    source_image_ref: metadata
                        .discovery_image_ref
                        .clone()
                        .expect("configured app"),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: image.into(),
                    platform: solodock::registry::Platform::canonical("linux", "amd64", None)
                        .unwrap(),
                    local_image_id: digest,
                },
                solodock::app_store::releases::ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        solodock::app_store::atomic::AtomicWriter::switch_release_link(
            &self.store.app_directory(app_id),
            "active",
            release_id,
        )
        .unwrap();
        self.catalog.replace(&self.store.scan_read_only().unwrap());
    }

    fn publish_pending(
        &self,
        app_id: Uuid,
        release_id: Uuid,
        image: &str,
        source_release_id: Option<Uuid>,
    ) -> solodock::app_store::releases::ReleaseV2 {
        let metadata = self.store.read_metadata(app_id).unwrap();
        let digest = image.split('@').nth(1).unwrap().to_owned();
        let release = self
            .store
            .publish_v2_release(
                &metadata,
                release_id,
                &solodock::registry::ResolvedImage {
                    source_image_ref: metadata
                        .discovery_image_ref
                        .clone()
                        .expect("configured app"),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: image.into(),
                    platform: solodock::registry::Platform::canonical("linux", "amd64", None)
                        .unwrap(),
                    local_image_id: digest,
                },
                solodock::app_store::releases::ReleaseTrigger::Manual,
                source_release_id,
            )
            .unwrap();
        self.store.set_pending(app_id, release_id).unwrap();
        self.catalog.replace(&self.store.scan_read_only().unwrap());
        release
    }

    fn install_legacy_signed_binds(
        &self,
        app_id: Uuid,
        revision_id: Uuid,
        binds: Vec<solodock::domain::BindMountInput>,
    ) {
        #[derive(serde::Serialize)]
        struct Canonical<'a> {
            metadata: &'a solodock::domain::ConfigMetadata,
            public_environment: &'a [solodock::domain::PublicEnvInput],
            public_files: &'a std::collections::BTreeMap<String, String>,
        }

        let loaded = solodock::app_store::config_revision::load(
            &self.store.app_directory(app_id),
            revision_id,
        )
        .unwrap();
        let mut metadata = loaded.metadata;
        let mut binds = binds;
        binds.sort_by(|left, right| left.target_path.cmp(&right.target_path));
        metadata.binds = binds;
        metadata.config_sha256.clear();
        let canonical = serde_json::to_vec(&Canonical {
            metadata: &metadata,
            public_environment: &loaded.public_environment,
            public_files: &loaded.public_files,
        })
        .unwrap();
        metadata.config_sha256 = Sha256::digest(canonical)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let config_sha256 = metadata.config_sha256.clone();
        let path = self
            .store
            .app_directory(app_id)
            .join("config-revisions")
            .join(revision_id.to_string())
            .join("config.toml");
        fs::write(&path, toml::to_string(&metadata).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut app = self.store.read_metadata(app_id).unwrap();
        app.draft_config_sha256 = Some(config_sha256);
        solodock::app_store::atomic::AtomicWriter::write(
            &self.store.app_directory(app_id).join("app.toml"),
            toml::to_string(&app).unwrap().as_bytes(),
            0o600,
        )
        .unwrap();
        let verified = solodock::app_store::config_revision::load_verified(
            &self.store.app_directory(app_id),
            revision_id,
            &self.idempotency.integrity_key(),
        )
        .unwrap();
        verified
            .normalize_verified(
                app.display_name.clone(),
                app.discovery_image_ref.clone().unwrap(),
                app.credential_ref,
                app.auto_deploy_enabled,
                app.poll_interval_seconds,
                &self.idempotency.integrity_key(),
                &self.store.allowed_bind_roots(),
            )
            .unwrap();
        self.catalog.replace(&self.store.scan_read_only().unwrap());
    }
}

fn delete_fingerprint(harness: &Harness, route: &str, request: &Value) -> Vec<u8> {
    let token = request["confirmation_token"].as_str().unwrap();
    let canonical = json!({
        "actor":"admin",
        "method":"DELETE",
        "route":route,
        "slug":request["slug"],
        "revision":request["expected_revision"],
        "remove_container":request["remove_container"],
        "token_hmac":harness.idempotency.fingerprint(token.as_bytes()),
    });
    harness
        .idempotency
        .fingerprint(&serde_json::to_vec(&canonical).unwrap())
}

fn draft(secret: &str) -> Value {
    json!({
        "slug": "example",
        "display_name": "Example",
        "discovery_image_ref": "registry.example/app:stable",
        "credential_ref": null,
        "auto_deploy_enabled": false,
        "poll_interval_seconds": 300,
        "environment": {
            "public": [{"key":"MODE","value":"production"}],
            "secrets": [{"key":"TOKEN","operation":"replace","value":secret}]
        },
        "files": [], "ports": [], "volumes": [], "binds": [], "networks": [],
        "service_discovery_enabled": false,
        "health": {"policy":"running","stable_window_seconds":15}
    })
}

fn mutable_draft(secret: &str) -> Value {
    let mut value = draft(secret);
    value.as_object_mut().unwrap().remove("slug");
    value
}

fn mutable(value: Value) -> Value {
    let mut value = value;
    value.as_object_mut().unwrap().remove("slug");
    value
}

fn release_container(
    id: char,
    app_id: Uuid,
    release: &solodock::app_store::releases::ReleaseV2,
) -> ContainerRecord {
    let mut container = owned_container(id, app_id, release.id, true);
    container.configured_image_ref = Some(release.runnable_image_ref.clone());
    container.image_id = Some(release.local_image_id.clone());
    container
}

async fn wait_for_deployment(
    harness: &Harness,
    deployment_id: Uuid,
) -> solodock::deploy::DeploymentRecord {
    for _ in 0..500 {
        if let Some(record) = harness
            .state
            .m4
            .as_ref()
            .unwrap()
            .ledger
            .get(deployment_id)
            .await
            .unwrap()
            && record.status.is_terminal()
        {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("deployment did not become terminal");
}

fn draft_with_owned_volume(secret: &str, logical_name: &str) -> Value {
    let mut value = draft(secret);
    value["volumes"] = json!([{
        "kind":"owned",
        "logical_name":logical_name,
        "target_path":"/data"
    }]);
    value
}

fn external_only_draft(secret: &str, alias: &str) -> Value {
    let mut value = draft(secret);
    value["owned_default_network"] = json!(false);
    value["networks"] = json!([{
        "kind":"external",
        "name":"shared",
        "aliases":[alias]
    }]);
    value
}

async fn body(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn storage_cleanup_fixture(harness: &Harness) -> (Uuid, Uuid, Vec<Uuid>, Uuid) {
    let mut input = draft("cleanup-secret-canary");
    input["files"] = json!([
        {"logical_name":"public-config","target_path":"/app/config","sensitive":false,"readonly":true,"content":"cleanup-public-canary"},
        {"logical_name":"secret-config","target_path":"/app/secret","sensitive":true,"readonly":true,"operation":"replace","value":"cleanup-file-secret-canary"}
    ]);
    let (status, created) =
        body(harness.create(Some("storage-cleanup-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let metadata = harness.store.read_metadata(app_id).unwrap();
    let mut releases = Vec::new();
    for index in 0..12 {
        let release_id = Uuid::new_v4();
        let digest = format!("sha256:{}", format!("{index:x}").repeat(64));
        harness
            .store
            .publish_v2_release(
                &metadata,
                release_id,
                &solodock::registry::ResolvedImage {
                    source_image_ref: metadata.discovery_image_ref.clone().unwrap(),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: format!("registry.example/app@{digest}"),
                    platform: solodock::registry::Platform::canonical("linux", "amd64", None)
                        .unwrap(),
                    local_image_id: digest,
                },
                solodock::app_store::releases::ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        releases.push(release_id);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    solodock::app_store::atomic::AtomicWriter::switch_release_link(
        &harness.store.app_directory(app_id),
        "active",
        releases[11],
    )
    .unwrap();
    solodock::app_store::atomic::AtomicWriter::switch_release_link(
        &harness.store.app_directory(app_id),
        "pending",
        releases[10],
    )
    .unwrap();
    let base = time::OffsetDateTime::now_utc();
    let mut source_deployment = Uuid::nil();
    for (order, index) in [0usize, 7, 8, 9].into_iter().enumerate() {
        let release_id = releases[index];
        let deployment_id = Uuid::new_v4();
        if index == 0 {
            source_deployment = deployment_id;
        }
        let created =
            solodock::db::format_time(base + time::Duration::seconds(order as i64)).unwrap();
        sqlx::query("INSERT INTO deployments (id,app_id,trigger,requested_revision,candidate_release_id,status,phase,request_id,created_at,updated_at) VALUES (?,?,'manual',?,?,'succeeded','terminal',?,?,?)")
            .bind(deployment_id.to_string())
            .bind(app_id.to_string())
            .bind(revision.to_string())
            .bind(release_id.to_string())
            .bind(Uuid::new_v4().to_string())
            .bind(&created)
            .bind(&created)
            .execute(harness.database.pool())
            .await
            .unwrap();
    }
    let created = solodock::db::format_time(base + time::Duration::seconds(100)).unwrap();
    sqlx::query("INSERT INTO deployments (id,app_id,trigger,requested_revision,from_release_id,expected_pending_release_id,expected_actual_release_id,predecessor_runtime_release_id,candidate_release_id,rollback_target_release_id,status,phase,request_id,created_at,updated_at) VALUES (?,?,'manual',?,?,?,?,?,?,?,'queued','queued',?,?,?)")
        .bind(Uuid::new_v4().to_string())
        .bind(app_id.to_string())
        .bind(revision.to_string())
        .bind(releases[1].to_string())
        .bind(releases[2].to_string())
        .bind(releases[3].to_string())
        .bind(releases[4].to_string())
        .bind(releases[5].to_string())
        .bind(releases[6].to_string())
        .bind(Uuid::new_v4().to_string())
        .bind(&created)
        .bind(&created)
        .execute(harness.database.pool())
        .await
        .unwrap();
    (app_id, revision, releases, source_deployment)
}

async fn cleanup_request(harness: &Harness) -> Value {
    let (status, response) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/preview",
                None,
                &json!({}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let preview: Value = serde_json::from_str(&response).unwrap();
    json!({"confirmation_token": preview["confirmation_token"], "acknowledge_rollback_loss": true})
}

#[cfg(feature = "docker-e2e")]
async fn cleanup_exclusive_revision_fixture(harness: &Harness) -> (Uuid, Uuid, Uuid) {
    let mut input = draft("cleanup-exclusive-secret");
    input["files"] = json!([
        {"logical_name":"public-config","target_path":"/app/config","sensitive":false,"readonly":true,"content":"cleanup-public-canary"},
        {"logical_name":"secret-config","target_path":"/app/secret","sensitive":true,"readonly":true,"operation":"replace","value":"cleanup-file-secret-canary"}
    ]);
    let (status, response) = body(
        harness
            .create(Some("cleanup-exclusive-create"), &input)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{response}");
    let created: Value = serde_json::from_str(&response).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let old_revision: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let old_release = Uuid::new_v4();
    harness.publish_active(
        app_id,
        old_release,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let mut updated = mutable(input);
    updated["environment"] = mutable_draft("cleanup-new-secret")["environment"].clone();
    let (status, response) = body(
        harness
            .mutate(
                "PUT",
                &format!("/api/v1/apps/{app_id}/draft"),
                Some("cleanup-exclusive-update"),
                &json!({"expected_revision":old_revision, "draft": updated}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    harness.publish_active(
        app_id,
        Uuid::new_v4(),
        &format!("registry.example/app@sha256:{}", "b".repeat(64)),
    );
    (app_id, old_revision, old_release)
}

#[cfg(feature = "docker-e2e")]
struct CleanupPausedPuller {
    reached: tokio::sync::Semaphore,
    resume: tokio::sync::Semaphore,
}

#[cfg(feature = "docker-e2e")]
#[async_trait]
impl ImagePuller for CleanupPausedPuller {
    async fn pull(
        &self,
        _deployment_id: Uuid,
        _resolved: &solodock::registry::ResolvedImage,
        _credential: Option<&solodock::registry::LoadedCredential>,
        _redaction: Vec<Vec<u8>>,
    ) -> Result<(), PullError> {
        self.reached.add_permits(1);
        self.resume.acquire().await.unwrap().forget();
        Err(PullError::Interrupted)
    }
}

#[cfg(feature = "docker-e2e")]
#[tokio::test]
async fn storage_cleanup_resume_preserves_a_new_rollback_scheduler_reference() {
    use solodock::app_store::cleanup::CleanupFault;
    let docker = Arc::new(ScriptedDocker::default());
    let puller = Arc::new(CleanupPausedPuller {
        reached: tokio::sync::Semaphore::new(0),
        resume: tokio::sync::Semaphore::new(0),
    });
    let harness = Harness::new_with_components(docker.clone(), None, Some(puller.clone())).await;
    let (app_id, _, releases, source_deployment) = storage_cleanup_fixture(&harness).await;
    sqlx::query(
        "UPDATE deployments SET status='interrupted',phase='terminal' WHERE status='queued'",
    )
    .execute(harness.database.pool())
    .await
    .unwrap();
    harness
        .catalog
        .replace(&harness.store.scan_read_only().unwrap());
    let request = cleanup_request(&harness).await;
    let route = "/api/v1/system/storage-cleanup/apply";
    let key = "cleanup-rollback-interleaving";
    harness
        .store
        .fail_cleanup_once(CleanupFault::MarkerPublished);
    assert_eq!(
        harness
            .mutate("POST", route, Some(key), &request)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    docker.set(vec![Vec::new()]);
    let (status, scheduled) = body(harness.mutate("POST", &format!("/api/v1/deployments/{source_deployment}/rollback"), Some("rollback-after-cleanup-interruption"), &json!({
        "expected_active_release_id": releases[11], "expected_pending_release_id": releases[10],
        "expected_actual_release_id": null, "expected_actual_container_id": null,
        "acknowledge_non_rollbackable_data": true
    })).await).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{scheduled}");
    tokio::time::timeout(Duration::from_secs(5), puller.reached.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        harness.store.read_release_link(app_id, "pending").unwrap(),
        Some(releases[0])
    );
    assert_eq!(
        harness
            .mutate("POST", route, Some(key), &request)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    puller.resume.add_permits(1);
    harness.state.stream_tasks.close();
    tokio::time::timeout(Duration::from_secs(5), harness.state.stream_tasks.wait())
        .await
        .unwrap();
    let (status, result) = body(harness.mutate("POST", route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["status"], "completed_with_failures");
    assert_eq!(result["items"][0]["error_code"], "CLEANUP_ITEM_PROTECTED");
    assert!(harness.store.load_v2_release(app_id, releases[0]).is_ok());
    assert_eq!(
        harness.store.read_release_link(app_id, "pending").unwrap(),
        Some(releases[0])
    );
    assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
}

#[cfg(feature = "docker-e2e")]
#[tokio::test]
async fn storage_cleanup_managed_leaves_and_partial_response_resume_use_durable_item_codes() {
    use solodock::app_store::cleanup::CleanupFault;
    for fail_rename in [true, false] {
        let mut harness = Harness::new().await;
        let (app_id, old_revision, old_release) =
            cleanup_exclusive_revision_fixture(&harness).await;
        let request = cleanup_request(&harness).await;
        if fail_rename {
            harness.store.fail_cleanup_once(CleanupFault::Rename);
        }
        sqlx::query("CREATE TRIGGER reject_cleanup_response BEFORE UPDATE ON idempotency_records WHEN NEW.route='/api/v1/system/storage-cleanup/apply' AND NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected response commit failure'); END").execute(harness.database.pool()).await.unwrap();
        let route = "/api/v1/system/storage-cleanup/apply";
        let key = "cleanup-durable-item-codes";
        assert_eq!(
            harness
                .mutate("POST", route, Some(key), &request)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let directory = harness
            .store
            .app_directory(app_id)
            .join("config-revisions")
            .join(old_revision.to_string());
        assert_eq!(directory.exists(), fail_rename);
        assert_eq!(
            harness.store.load_v2_release(app_id, old_release).is_ok(),
            fail_rename
        );
        let operation: String =
            sqlx::query_scalar("SELECT operation_id FROM storage_cleanup_operations")
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        if fail_rename {
            let codes: Vec<String> =
                sqlx::query_scalar("SELECT error_code FROM storage_cleanup_items ORDER BY ordinal")
                    .fetch_all(harness.database.pool())
                    .await
                    .unwrap();
            assert_eq!(codes, ["CLEANUP_ITEM_RETAINED", "RELEASE_RETAINED"]);
        } else {
            let payload = harness
                .store
                .cleanup_tombstone_path(operation.parse().unwrap())
                .join("payload/1");
            assert_eq!(
                fs::read_to_string(payload.join("files/secret/secret-config")).unwrap(),
                "cleanup-file-secret-canary"
            );
            assert_eq!(
                fs::metadata(payload.join("files/public/public-config"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o444
            );
        }
        assert!(
            harness
                .store
                .scan()
                .unwrap()
                .valid_apps
                .iter()
                .any(|app| app.app_id == app_id)
        );
        assert_eq!(
            solodock::storage_cleanup::pending_operation_count(&harness.store, &harness.database)
                .await
                .unwrap(),
            1
        );
        solodock::storage_cleanup::finalize_succeeded(&harness.store, &harness.database)
            .await
            .unwrap();
        assert_eq!(harness.store.cleanup_tombstones().unwrap().len(), 1);
        harness.restart_cleanup_router().await;
        sqlx::query("DROP TRIGGER reject_cleanup_response")
            .execute(harness.database.pool())
            .await
            .unwrap();
        let (status, response) =
            body(harness.mutate("POST", route, Some(key), &request).await).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert!(!response.contains("cleanup-file-secret-canary"));
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response["status"],
            if fail_rename {
                "completed_with_failures"
            } else {
                "completed"
            }
        );
        if fail_rename {
            assert_eq!(response["items"][1]["error_code"], "RELEASE_RETAINED");
        }
        assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
        assert!(
            harness
                .store
                .scan()
                .unwrap()
                .valid_apps
                .iter()
                .any(|app| app.app_id == app_id)
        );
    }
}

#[cfg(feature = "docker-e2e")]
#[tokio::test]
async fn storage_cleanup_real_filesystem_interruptions_repeat_barriers_and_preserve_retry() {
    use solodock::app_store::cleanup::CleanupFault;
    for fault in [
        CleanupFault::MarkerPublished,
        CleanupFault::SourceSync,
        CleanupFault::DestinationSync,
    ] {
        let mut harness = Harness::new().await;
        let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
        let request = cleanup_request(&harness).await;
        let route = "/api/v1/system/storage-cleanup/apply";
        let key = "cleanup-real-filesystem-failure";
        harness.store.fail_cleanup_once(fault);
        assert_eq!(
            harness
                .mutate("POST", route, Some(key), &request)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let operation: String =
            sqlx::query_scalar("SELECT operation_id FROM storage_cleanup_operations")
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        let canonical = harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[0].to_string());
        assert_eq!(canonical.exists(), fault == CleanupFault::MarkerPublished);
        harness.restart_cleanup_router().await;
        let guard = harness
            .state
            .m3
            .as_ref()
            .unwrap()
            .coordinator
            .try_app(app_id)
            .unwrap();
        assert_eq!(
            harness
                .mutate("POST", route, Some(key), &request)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        drop(guard);
        // The same authenticated cookie resolves to a different valid session ID.
        let original_session: String = sqlx::query_scalar("SELECT id FROM sessions")
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET id=?")
            .bind(Uuid::new_v4().to_string())
            .execute(harness.database.pool())
            .await
            .unwrap();
        assert_eq!(
            harness
                .mutate("POST", route, Some(key), &request)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        sqlx::query("UPDATE sessions SET id=?")
            .bind(original_session)
            .execute(harness.database.pool())
            .await
            .unwrap();
        let proof: String =
            sqlx::query_scalar("SELECT status FROM idempotency_records WHERE operation_id=?")
                .bind(&operation)
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        assert_eq!(proof, "interrupted");
        if fault != CleanupFault::MarkerPublished {
            // This second injected failure can only fire if AlreadyDetached
            // repeats the durability barrier before recording progress.
            harness.store.fail_cleanup_once(fault);
            assert_eq!(
                harness
                    .mutate("POST", route, Some(key), &request)
                    .await
                    .status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
            let item: String =
                sqlx::query_scalar("SELECT status FROM storage_cleanup_items WHERE operation_id=?")
                    .bind(&operation)
                    .fetch_one(harness.database.pool())
                    .await
                    .unwrap();
            assert_eq!(item, "planned");
        }
        let (status, response) =
            body(harness.mutate("POST", route, Some(key), &request).await).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["status"],
            "completed"
        );
        assert!(!canonical.exists());
        assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
    }
}

#[cfg(feature = "docker-e2e")]
#[tokio::test]
async fn storage_cleanup_partial_finalization_restarts_with_exact_proof_and_catalog() {
    use solodock::app_store::cleanup::CleanupFault;
    for fault in [
        CleanupFault::PayloadRemoved,
        CleanupFault::MarkerRetired,
        CleanupFault::DirectoryRemoved,
    ] {
        let harness = Harness::new().await;
        let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
        let request = cleanup_request(&harness).await;
        harness.store.fail_cleanup_once(fault);
        let (status, response) = body(
            harness
                .mutate(
                    "POST",
                    "/api/v1/system/storage-cleanup/apply",
                    Some("cleanup-finalizer-real-partial"),
                    &request,
                )
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let response: Value = serde_json::from_str(&response).unwrap();
        let operation: Uuid = response["operation_id"].as_str().unwrap().parse().unwrap();
        assert!(
            !harness
                .store
                .cleanup_tombstone_path(operation)
                .join("payload")
                .exists()
        );
        let restarted = AppStore::initialize_verified(
            harness.apps.clone(),
            harness.idempotency.integrity_key(),
        )
        .unwrap();
        assert_eq!(restarted.cleanup_tombstones().unwrap(), vec![operation]);
        assert!(
            restarted
                .scan()
                .unwrap()
                .valid_apps
                .iter()
                .any(|app| app.app_id == app_id)
        );
        assert_eq!(
            restarted.read_release_link(app_id, "active").unwrap(),
            Some(releases[11])
        );
        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(operation.to_string())
        .execute(harness.database.pool())
        .await
        .unwrap();
        harness
            .idempotency
            .gc_with_artifact_inventory(
                &restarted,
                &harness.state.m4.as_ref().unwrap().credentials,
                &harness.state.webhooks.as_ref().unwrap().store,
            )
            .await
            .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(operation.to_string())
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
        solodock::storage_cleanup::finalize_succeeded(&restarted, &harness.database)
            .await
            .unwrap();
        assert!(restarted.cleanup_tombstones().unwrap().is_empty());
        assert!(
            restarted
                .scan()
                .unwrap()
                .valid_apps
                .iter()
                .any(|app| app.app_id == app_id)
        );
    }
}

#[tokio::test]
async fn storage_cleanup_plan_audit_failure_rolls_back_consumption_and_publication() {
    let harness = Harness::new().await;
    let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
    let request = cleanup_request(&harness).await;
    sqlx::query("CREATE TRIGGER reject_cleanup_audit BEFORE INSERT ON audit_events WHEN NEW.action='storage_cleanup_apply' BEGIN SELECT RAISE(ABORT,'injected cleanup audit failure'); END").execute(harness.database.pool()).await.unwrap();
    let response = harness
        .mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("cleanup-plan-audit-failure"),
            &request,
        )
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM storage_cleanup_previews WHERE consumed_at IS NOT NULL",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(count, 0);
    let operations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_cleanup_operations")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(operations, 0);
    assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
    assert!(harness.store.load_v2_release(app_id, releases[0]).is_ok());
    sqlx::query("DROP TRIGGER reject_cleanup_audit")
        .execute(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/apply",
                Some("cleanup-plan-audit-failure"),
                &request
            )
            .await
            .status(),
        StatusCode::OK
    );
}

#[cfg(feature = "docker-e2e")]
#[tokio::test]
async fn storage_cleanup_last_unlink_failure_retains_proof_until_sync_even_after_marker_resurrection()
 {
    use solodock::app_store::cleanup::CleanupFault;
    for resurrect in [false, true] {
        let harness = Harness::new().await;
        let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
        let request = cleanup_request(&harness).await;
        harness
            .store
            .fail_cleanup_once(CleanupFault::DirectoryRemoved);
        let (status, response) = body(
            harness
                .mutate(
                    "POST",
                    "/api/v1/system/storage-cleanup/apply",
                    Some("cleanup-last-unlink"),
                    &request,
                )
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let response: Value = serde_json::from_str(&response).unwrap();
        let operation: Uuid = response["operation_id"].as_str().unwrap().parse().unwrap();
        let retired = harness
            .apps
            .join(".cleanup-trash")
            .join(format!("{operation}.retired.toml"));
        let signed_marker = fs::read(&retired).unwrap();
        harness
            .store
            .fail_cleanup_once(CleanupFault::RetiredMarkerRemoved);
        assert!(
            solodock::storage_cleanup::finalize_succeeded(&harness.store, &harness.database)
                .await
                .is_err()
        );
        assert!(!retired.exists());
        assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
        assert_eq!(
            solodock::storage_cleanup::pending_operation_count(&harness.store, &harness.database)
                .await
                .unwrap(),
            1
        );
        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(operation.to_string())
        .execute(harness.database.pool())
        .await
        .unwrap();
        harness
            .idempotency
            .gc_with_artifact_inventory(
                &harness.store,
                &harness.state.m4.as_ref().unwrap().credentials,
                &harness.state.webhooks.as_ref().unwrap().store,
            )
            .await
            .unwrap();
        let proofs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(operation.to_string())
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        assert_eq!(proofs, 1);
        // A crash may roll back an unlink whose parent fsync never succeeded.
        if resurrect {
            fs::write(&retired, signed_marker).unwrap();
            fs::set_permissions(&retired, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let restarted = AppStore::initialize_verified(
            harness.apps.clone(),
            harness.idempotency.integrity_key(),
        )
        .unwrap();
        assert_eq!(
            solodock::storage_cleanup::pending_operation_count(&restarted, &harness.database)
                .await
                .unwrap(),
            1
        );
        solodock::storage_cleanup::finalize_succeeded(&restarted, &harness.database)
            .await
            .unwrap();
        assert_eq!(
            solodock::storage_cleanup::pending_operation_count(&restarted, &harness.database)
                .await
                .unwrap(),
            0
        );
        assert!(restarted.cleanup_tombstones().unwrap().is_empty());
        assert_eq!(
            restarted.read_release_link(app_id, "active").unwrap(),
            Some(releases[11])
        );
        assert!(
            restarted
                .scan()
                .unwrap()
                .valid_apps
                .iter()
                .any(|app| app.app_id == app_id)
        );
        harness
            .idempotency
            .gc_with_artifact_inventory(
                &restarted,
                &harness.state.m4.as_ref().unwrap().credentials,
                &harness.state.webhooks.as_ref().unwrap().store,
            )
            .await
            .unwrap();
        let proofs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(operation.to_string())
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
        assert_eq!(proofs, 0);
    }
}

#[tokio::test]
async fn storage_cleanup_application_trash_uses_exact_read_only_deletion_inventory() {
    for scenario in [
        "pending",
        "interrupted",
        "succeeded",
        "unknown",
        "marker",
        "missing-proof",
        "wrong-proof",
    ] {
        let harness = Harness::new().await;
        let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
        let mut input = draft("trash-canary");
        input["slug"] = json!("old-trash-app");
        let (status, created) = body(harness.create(Some("old-trash-create"), &input).await).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let old: Uuid = serde_json::from_str::<Value>(&created).unwrap()["app"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let route = format!("/api/v1/apps/{old}");
        let operation = match harness
            .idempotency
            .claim(&route, "old-trash-delete", &[1; 32], Uuid::new_v4())
            .await
            .unwrap()
        {
            solodock::mutation::ClaimResult::New(id) => id,
            _ => panic!("expected new deletion"),
        };
        harness.store.tombstone(old, operation).unwrap();
        if scenario == "interrupted" {
            harness
                .idempotency
                .mark_interrupted(&route, "old-trash-delete", Uuid::new_v4())
                .await
                .unwrap();
        } else if scenario == "succeeded" || scenario == "wrong-proof" {
            harness
                .idempotency
                .finish(
                    &route,
                    "old-trash-delete",
                    200,
                    &json!({"app_id":old,"unregistered":true}).to_string(),
                    None,
                    Uuid::new_v4(),
                )
                .await
                .unwrap();
        }
        // The legal pre-existing tombstone is compatible with preview, without
        // executing its deletion finalizer. Corruption arrives after this token.
        let request = cleanup_request(&harness).await;
        let path = harness.store.tombstone_path(old, operation);
        match scenario {
            "unknown" => {
                fs::create_dir(harness.apps.join(".trash/unknown")).unwrap();
            }
            "marker" => {
                fs::write(path.join("deletion.toml"), "invalid").unwrap();
            }
            "missing-proof" => {
                sqlx::query("DELETE FROM idempotency_records WHERE operation_id=?")
                    .bind(operation.to_string())
                    .execute(harness.database.pool())
                    .await
                    .unwrap();
            }
            "wrong-proof" => {
                sqlx::query(
                    "UPDATE idempotency_records SET response_body='{}' WHERE operation_id=?",
                )
                .bind(operation.to_string())
                .execute(harness.database.pool())
                .await
                .unwrap();
            }
            _ => {}
        }
        let valid = matches!(scenario, "pending" | "interrupted" | "succeeded");
        if !valid {
            let (status, error) = body(
                harness
                    .mutate(
                        "POST",
                        "/api/v1/system/storage-cleanup/preview",
                        None,
                        &json!({}),
                    )
                    .await,
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{scenario}: {error}");
            assert_eq!(
                serde_json::from_str::<Value>(&error).unwrap()["code"],
                "CLEANUP_INVENTORY_INCOMPLETE"
            );
            let previews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_cleanup_previews")
                .fetch_one(harness.database.pool())
                .await
                .unwrap();
            assert_eq!(previews, 1);
        }
        let (status, response) = body(
            harness
                .mutate(
                    "POST",
                    "/api/v1/system/storage-cleanup/apply",
                    Some("cleanup-old-trash"),
                    &request,
                )
                .await,
        )
        .await;
        assert_eq!(
            status,
            if valid {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            },
            "{scenario}: {response}"
        );
        assert!(path.exists(), "cleanup must not finalize app deletion");
        if !valid {
            let consumed: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM storage_cleanup_previews WHERE consumed_at IS NOT NULL",
            )
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
            assert_eq!(consumed, 0);
            let operations: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM storage_cleanup_operations")
                    .fetch_one(harness.database.pool())
                    .await
                    .unwrap();
            assert_eq!(operations, 0);
            assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
            assert!(harness.store.load_v2_release(app_id, releases[0]).is_ok());
        }
    }
}

#[tokio::test]
async fn storage_cleanup_progress_write_failure_resumes_a_real_rename() {
    let mut harness = Harness::new().await;
    let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
    let request = cleanup_request(&harness).await;
    sqlx::query("CREATE TRIGGER reject_cleanup_progress BEFORE UPDATE ON storage_cleanup_items WHEN NEW.status='detached' BEGIN SELECT RAISE(ABORT,'injected item progress failure'); END").execute(harness.database.pool()).await.unwrap();
    let route = "/api/v1/system/storage-cleanup/apply";
    let key = "cleanup-progress-failure";
    assert_eq!(
        harness
            .mutate("POST", route, Some(key), &request)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        !harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[0].to_string())
            .exists()
    );
    let item: String = sqlx::query_scalar("SELECT status FROM storage_cleanup_items")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(item, "planned");
    let cleaned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(cleaned, 0);
    let proof: String = sqlx::query_scalar("SELECT status FROM idempotency_records WHERE route=?")
        .bind(route)
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(proof, "interrupted");
    harness.restart_cleanup_router().await;
    sqlx::query("DROP TRIGGER reject_cleanup_progress")
        .execute(harness.database.pool())
        .await
        .unwrap();
    let (status, response) = body(harness.mutate("POST", route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let cleaned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(cleaned, 1);
    assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn storage_cleanup_each_current_fact_change_rejects_before_consumption() {
    for fact in ["active", "pending", "draft", "recovery"] {
        let harness = Harness::new().await;
        let (app_id, revision, releases, _) = storage_cleanup_fixture(&harness).await;
        let request = cleanup_request(&harness).await;
        match fact {
            "active" | "pending" => solodock::app_store::atomic::AtomicWriter::switch_release_link(
                &harness.store.app_directory(app_id),
                fact,
                releases[0],
            )
            .unwrap(),
            "draft" => {
                let metadata = harness.store.read_metadata(app_id).unwrap();
                let loaded = solodock::app_store::config_revision::load_verified(
                    &harness.store.app_directory(app_id),
                    revision,
                    harness.store.integrity_key().unwrap(),
                )
                .unwrap();
                let normalized = loaded
                    .normalize_verified(
                        metadata.display_name,
                        metadata.discovery_image_ref.unwrap(),
                        metadata.credential_ref,
                        metadata.auto_deploy_enabled,
                        metadata.poll_interval_seconds,
                        harness.store.integrity_key().unwrap(),
                        &[],
                    )
                    .unwrap();
                harness
                    .store
                    .update_draft(
                        app_id,
                        Some(revision),
                        Uuid::new_v4(),
                        Uuid::new_v4(),
                        &normalized,
                        time::OffsetDateTime::now_utc(),
                    )
                    .unwrap();
            }
            "recovery" => {
                sqlx::query("UPDATE deployments SET from_release_id=? WHERE status='queued'")
                    .bind(releases[0].to_string())
                    .execute(harness.database.pool())
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let (status, response) = body(
            harness
                .mutate(
                    "POST",
                    "/api/v1/system/storage-cleanup/apply",
                    Some("cleanup-fresh-facts"),
                    &request,
                )
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{fact}: {response}");
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["code"],
            "CLEANUP_PREVIEW_STALE"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage_cleanup_previews WHERE consumed_at IS NOT NULL",
        )
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
        assert_eq!(count, 0);
        assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
        assert!(harness.store.load_v2_release(app_id, releases[0]).is_ok());
    }
}

#[tokio::test]
async fn storage_cleanup_invalid_artifact_or_ledger_never_issues_a_token() {
    for damage in [
        "hmac",
        "missing",
        "mode",
        "managed-mode",
        "symlink",
        "type",
        "ledger",
        "owner",
    ] {
        if damage == "owner" && unsafe { libc::geteuid() } != 0 {
            continue;
        }
        let harness = Harness::new().await;
        let (app_id, revision, releases, _) = storage_cleanup_fixture(&harness).await;
        let app = harness.store.app_directory(app_id);
        let header = app
            .join("releases")
            .join(releases[0].to_string())
            .join("release.toml");
        let original = fs::read(&header).unwrap();
        match damage {
            "hmac" => {
                let mut value: toml::Value =
                    toml::from_str(std::str::from_utf8(&original).unwrap()).unwrap();
                value["integrity_hmac"] = toml::Value::String("0".repeat(64));
                fs::write(&header, toml::to_string(&value).unwrap()).unwrap();
            }
            "missing" => fs::remove_file(&header).unwrap(),
            "mode" => fs::set_permissions(&header, fs::Permissions::from_mode(0o444)).unwrap(),
            "managed-mode" => fs::set_permissions(
                app.join("config-revisions")
                    .join(revision.to_string())
                    .join("files/public/public-config"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap(),
            "symlink" => {
                fs::remove_file(&header).unwrap();
                std::os::unix::fs::symlink("/etc/passwd", &header).unwrap();
            }
            "type" => {
                fs::remove_file(&header).unwrap();
                fs::create_dir(&header).unwrap();
            }
            "ledger" => {
                sqlx::query("UPDATE deployments SET requested_revision='invalid-uuid' WHERE status='queued'").execute(harness.database.pool()).await.unwrap();
            }
            "owner" => {
                let path = std::ffi::CString::new(header.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);
            }
            _ => unreachable!(),
        }
        let (status, response) = body(
            harness
                .mutate(
                    "POST",
                    "/api/v1/system/storage-cleanup/preview",
                    None,
                    &json!({}),
                )
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{damage}: {response}");
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["code"],
            "CLEANUP_INVENTORY_INCOMPLETE"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_cleanup_previews")
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
        assert!(app.join("releases").join(releases[0].to_string()).is_dir());
    }
}

#[tokio::test]
async fn storage_cleanup_protects_every_recoverable_deployment_status() {
    let harness = Harness::new().await;
    let (app_id, revision, releases, _) = storage_cleanup_fixture(&harness).await;
    for (status, phase) in [
        ("queued", "queued"),
        ("running", "pulling"),
        ("interrupted", "terminal"),
        ("needs_attention", "terminal"),
    ] {
        sqlx::query("UPDATE deployments SET status=?,phase=? WHERE app_id=? AND status IN ('queued','running','interrupted','needs_attention')")
            .bind(status)
            .bind(phase)
            .bind(app_id.to_string())
            .execute(harness.database.pool())
            .await
            .unwrap();
        let plan = solodock::storage_cleanup::build_plan(&harness.store, &harness.database)
            .await
            .unwrap();
        assert!(plan.protected.iter().any(|item| {
            item.artifact_id == revision.to_string()
                && item.reason == solodock::storage_cleanup::ProtectionReason::DeploymentRecovery
        }));
        for release in &releases[1..=6] {
            assert!(plan.protected.iter().any(|item| {
                item.artifact_id == release.to_string()
                    && item.reason
                        == solodock::storage_cleanup::ProtectionReason::DeploymentRecovery
            }));
        }
    }
}

#[tokio::test]
async fn storage_cleanup_protects_current_and_three_rollbacks_then_applies_exact_plan() {
    let harness = Harness::new().await;
    let (app_id, revision, releases, source_deployment) = storage_cleanup_fixture(&harness).await;
    let plan = solodock::storage_cleanup::build_plan(&harness.store, &harness.database)
        .await
        .unwrap();
    assert!(plan.protected.iter().any(|item| {
        item.artifact_id == releases[11].to_string()
            && item.reason == solodock::storage_cleanup::ProtectionReason::Active
    }));
    assert!(plan.protected.iter().any(|item| {
        item.artifact_id == releases[10].to_string()
            && item.reason == solodock::storage_cleanup::ProtectionReason::Pending
    }));
    assert_eq!(
        plan.protected
            .iter()
            .filter(|item| {
                item.reason == solodock::storage_cleanup::ProtectionReason::RecentRollback
            })
            .count(),
        3
    );
    for release in &releases[1..=6] {
        assert!(plan.protected.iter().any(|item| {
            item.artifact_id == release.to_string()
                && item.reason == solodock::storage_cleanup::ProtectionReason::DeploymentRecovery
        }));
    }
    assert!(plan.protected.iter().any(|item| {
        item.artifact_id == revision.to_string()
            && item.reason == solodock::storage_cleanup::ProtectionReason::CurrentDraft
    }));
    harness.compose_actions.lock().unwrap().clear();
    let (status, preview) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/preview",
                None,
                &json!({}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let release_candidates = preview["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["artifact_kind"] == "release")
        .collect::<Vec<_>>();
    assert_eq!(release_candidates.len(), 1, "{preview:#}");
    assert_eq!(
        release_candidates[0]["artifact_id"],
        releases[0].to_string()
    );
    assert_eq!(
        preview["protected"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["reason"] == "recent_rollback")
            .unwrap()["count"],
        3
    );
    assert!(
        preview["protected"].as_array().unwrap().iter().any(|item| {
            item["reason"] == "current_draft" && item["count"].as_u64().unwrap() >= 1
        })
    );

    let request = json!({
        "confirmation_token": preview["confirmation_token"],
        "acknowledge_rollback_loss": true,
    });
    let audit_before = harness.database.audit_count().await.unwrap();
    let (status, applied) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/apply",
                Some("storage-cleanup-apply-0001"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert!(!applied.contains("cleanup-secret-canary"));
    let applied: Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(applied["status"], "completed");
    assert!(
        !harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[0].to_string())
            .exists()
    );
    assert!(
        harness
            .store
            .app_directory(app_id)
            .join("config-revisions")
            .join(revision.to_string())
            .exists()
    );
    let retained_config = solodock::app_store::config_revision::load_verified(
        &harness.store.app_directory(app_id),
        revision,
        harness.store.integrity_key().unwrap(),
    )
    .unwrap();
    assert!(
        retained_config
            .known_secrets()
            .iter()
            .any(|secret| secret == b"cleanup-secret-canary")
    );
    assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
    assert_eq!(
        harness.database.audit_count().await.unwrap(),
        audit_before + 3
    );
    let cleanup_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action='storage_cleanup_apply' AND result='planned'",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(cleanup_audits, 1);
    assert!(harness.compose_actions.lock().unwrap().is_empty());

    let replay = harness
        .mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("storage-cleanup-apply-0001"),
            &request,
        )
        .await;
    let (status, replay) = body(replay).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&replay).unwrap()["idempotency_replayed"],
        true
    );

    let detail = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/deployments/{source_deployment}"))
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, detail) = body(detail).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert!(detail["safe_release_id"].is_null());
    assert_eq!(detail["available_actions"], json!([]));
    assert!(
        detail["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("ROLLBACK_ARTIFACT_CLEANED"))
    );

    let health = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/system/health")
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, health) = body(health).await;
    assert_eq!(status, StatusCode::OK, "{health}");
    let health: Value = serde_json::from_str(&health).unwrap();
    assert_eq!(health["storage_cleanup"]["status"], "ok");
    assert_eq!(health["storage_cleanup"]["pending_operations"], 0);

    let ordinarily_missing_deployment: String =
        sqlx::query_scalar("SELECT id FROM deployments WHERE app_id=? AND candidate_release_id=?")
            .bind(app_id.to_string())
            .bind(releases[7].to_string())
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    fs::remove_dir_all(
        harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[7].to_string()),
    )
    .unwrap();
    let ordinary = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/deployments/{ordinarily_missing_deployment}"
                ))
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, ordinary) = body(ordinary).await;
    assert_eq!(status, StatusCode::OK, "{ordinary}");
    let ordinary: Value = serde_json::from_str(&ordinary).unwrap();
    assert!(ordinary["safe_release_id"].is_null());
    assert!(
        !ordinary["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("ROLLBACK_ARTIFACT_CLEANED"))
    );
}

#[tokio::test]
async fn storage_cleanup_exact_retry_resumes_after_plan_marker_and_rename() {
    let harness = Harness::new().await;
    let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
    let (_, preview_body) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/preview",
                None,
                &json!({}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview_body).unwrap();
    let token = preview["confirmation_token"].as_str().unwrap();
    let token_hmac = harness.idempotency.fingerprint(token.as_bytes());
    let (plan_hash, plan_json): (Vec<u8>, String) = sqlx::query_as(
        "SELECT facts_hash,preview_json FROM storage_cleanup_previews WHERE token_hmac=?",
    )
    .bind(&token_hmac)
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let plan: solodock::storage_cleanup::CleanupPlan = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    let request = json!({
        "confirmation_token": token,
        "acknowledge_rollback_loss": true,
    });
    let route = "/api/v1/system/storage-cleanup/apply";
    let canonical = serde_json::to_vec(&json!({
        "actor": "admin",
        "method": "POST",
        "route": route,
        "token_hmac": token_hmac,
        "acknowledge_rollback_loss": true,
    }))
    .unwrap();
    let request_hmac = harness.idempotency.fingerprint(&canonical);
    let key = "storage-cleanup-resume-0001";
    let operation = match harness
        .idempotency
        .claim(route, key, &request_hmac, Uuid::new_v4())
        .await
        .unwrap()
    {
        solodock::mutation::ClaimResult::New(operation) => operation,
        _ => panic!("new cleanup claim expected"),
    };
    let now = solodock::db::format_time(time::OffsetDateTime::now_utc()).unwrap();
    let candidate = &plan.candidates[0];
    let config_revision = match candidate.artifact {
        solodock::app_store::cleanup::CleanupArtifact::Release {
            config_revision_id, ..
        }
        | solodock::app_store::cleanup::CleanupArtifact::ConfigRevision {
            revision_id: config_revision_id,
            ..
        } => Some(config_revision_id.to_string()),
        solodock::app_store::cleanup::CleanupArtifact::Temporary { .. } => None,
    };
    let mut transaction = harness.database.pool().begin().await.unwrap();
    sqlx::query("UPDATE storage_cleanup_previews SET consumed_at=? WHERE token_hmac=?")
        .bind(&now)
        .bind(&token_hmac)
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO storage_cleanup_operations (operation_id,cleanup_kind,plan_hash,plan_json,status,created_at) VALUES (?,'artifacts',?,?,'planned',?)")
        .bind(operation.to_string())
        .bind(&plan_hash)
        .bind(&plan_json)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO storage_cleanup_items (operation_id,ordinal,app_id,artifact_kind,artifact_id,config_revision_id,status) VALUES (?,0,?,?,?,?,'planned')")
        .bind(operation.to_string())
        .bind(candidate.artifact.app_id().map(|id| id.to_string()))
        .bind(candidate.artifact.kind_name())
        .bind(candidate.artifact.public_id())
        .bind(config_revision)
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    harness
        .store
        .prepare_cleanup_tombstone(
            operation,
            &plan_hash,
            std::slice::from_ref(&candidate.artifact),
        )
        .unwrap();
    assert_eq!(
        harness
            .store
            .detach_cleanup_artifact(operation, 0, &candidate.artifact)
            .unwrap(),
        solodock::app_store::cleanup::DetachResult::Detached
    );
    harness
        .idempotency
        .mark_interrupted(route, key, Uuid::new_v4())
        .await
        .unwrap();

    let (status, response) = body(harness.mutate("POST", route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["status"],
        "completed"
    );
    assert!(
        !harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[0].to_string())
            .exists()
    );
    assert!(harness.store.cleanup_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn storage_cleanup_stale_and_busy_fail_before_consuming_or_renaming() {
    let harness = Harness::new().await;
    let (app_id, _, releases, _) = storage_cleanup_fixture(&harness).await;
    let (_, preview_body) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/system/storage-cleanup/preview",
                None,
                &json!({}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview_body).unwrap();
    let request = json!({
        "confirmation_token": preview["confirmation_token"],
        "acknowledge_rollback_loss": true,
    });
    let guard = harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .try_app(app_id)
        .unwrap();
    let busy = harness
        .mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("storage-cleanup-busy-0001"),
            &request,
        )
        .await;
    assert_eq!(busy.status(), StatusCode::CONFLICT);
    drop(guard);
    let token_hmac = harness
        .idempotency
        .fingerprint(preview["confirmation_token"].as_str().unwrap().as_bytes());
    let consumed: Option<String> =
        sqlx::query_scalar("SELECT consumed_at FROM storage_cleanup_previews WHERE token_hmac=?")
            .bind(&token_hmac)
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    assert!(consumed.is_none());

    solodock::app_store::atomic::AtomicWriter::switch_release_link(
        &harness.store.app_directory(app_id),
        "pending",
        releases[0],
    )
    .unwrap();
    let stale = harness
        .mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("storage-cleanup-stale-0001"),
            &request,
        )
        .await;
    let (status, stale) = body(stale).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        serde_json::from_str::<Value>(&stale).unwrap()["code"],
        "CLEANUP_PREVIEW_STALE"
    );
    assert!(
        harness
            .store
            .app_directory(app_id)
            .join("releases")
            .join(releases[0].to_string())
            .exists()
    );
    let consumed_after_stale: Option<String> =
        sqlx::query_scalar("SELECT consumed_at FROM storage_cleanup_previews WHERE token_hmac=?")
            .bind(token_hmac)
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    assert!(consumed_after_stale.is_none());
}

#[tokio::test]
async fn storage_cleanup_unknown_trash_fails_closed_without_issuing_a_preview() {
    let harness = Harness::new().await;
    let trash = harness.store.apps_directory().join(".cleanup-trash");
    fs::create_dir(&trash).unwrap();
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o700)).unwrap();
    let unknown = trash.join("unexpected-entry");
    fs::create_dir(&unknown).unwrap();
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o700)).unwrap();

    let response = harness
        .mutate(
            "POST",
            "/api/v1/system/storage-cleanup/preview",
            None,
            &json!({}),
        )
        .await;
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["code"],
        "CLEANUP_INVENTORY_INCOMPLETE"
    );
    let previews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_cleanup_previews")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(previews, 0);
    assert!(unknown.exists());
}

#[tokio::test]
async fn application_deletion_is_blocked_by_a_published_cleanup_plan() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let (app_id, revision, _, _) = storage_cleanup_fixture(&harness).await;
    docker.set(vec![Vec::new()]);
    let (status, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let preview: Value = serde_json::from_str(&preview).unwrap();

    let operation = Uuid::new_v4();
    let now = solodock::db::format_time(time::OffsetDateTime::now_utc()).unwrap();
    sqlx::query("INSERT INTO storage_cleanup_operations (operation_id,cleanup_kind,plan_hash,plan_json,status,created_at) VALUES (?,'artifacts',x'01','{}','planned',?)")
        .bind(operation.to_string())
        .bind(&now)
        .execute(harness.database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO storage_cleanup_items (operation_id,ordinal,app_id,artifact_kind,artifact_id,status) VALUES (?,0,?,'temporary','temporary-1','planned')")
        .bind(operation.to_string())
        .bind(app_id.to_string())
        .execute(harness.database.pool())
        .await
        .unwrap();

    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":false
    });
    let (status, response) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("cleanup-plan-blocks-app-delete"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["code"],
        "APP_BUSY"
    );
    assert!(harness.store.app_directory(app_id).exists());
}

#[tokio::test]
async fn create_requires_idempotency_and_replays_without_secret_disclosure() {
    let harness = Harness::new().await;
    let canary = "M3_SECRET_CANARY";
    serde_json::from_value::<solodock::domain::DraftInput>(mutable_draft(canary)).unwrap();
    let missing = harness.create(None, &draft(canary)).await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");

    let key = "m3-create-example-0001";
    let (status, first) = body(harness.create(Some(key), &draft(canary)).await).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!first.contains(canary));
    let first_json: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first_json["app"]["deployment_status"], "DEPLOY_REQUIRED");
    assert_eq!(first_json["app"]["stop_grace_period_seconds"], 10);
    assert_eq!(first_json["idempotency_replayed"], false);

    let (status, replay) = body(harness.create(Some(key), &draft(canary)).await).await;
    assert_eq!(status, StatusCode::CREATED);
    let replay_json: Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(replay_json["app"]["id"], first_json["app"]["id"]);
    assert_eq!(replay_json["idempotency_replayed"], true);
    assert!(!replay.contains(canary));
    assert_eq!(harness.database.audit_count().await.unwrap(), 6); // bootstrap, login, create/update attempt and success

    let database_bytes = fs::read(harness._root.path().join("state/state.sqlite3")).unwrap();
    assert!(
        !database_bytes
            .windows(canary.len())
            .any(|window| window == canary.as_bytes())
    );
    let app_id = first_json["app"]["id"].as_str().unwrap();
    let app_directory = harness.apps.join(app_id);
    let app_toml = fs::read(app_directory.join("app.toml")).unwrap();
    assert!(
        !app_toml
            .windows(canary.len())
            .any(|window| window == canary.as_bytes())
    );
}

#[tokio::test]
async fn deployment_preflight_reports_managed_file_permission_drift_before_compose() {
    let harness = Harness::new().await;
    let mut candidate = draft("managed-secret-canary");
    candidate["files"] = json!([
        {
            "logical_name":"public-config",
            "target_path":"/app/public",
            "sensitive":false,
            "readonly":true,
            "content":"public-canary"
        },
        {
            "logical_name":"secret-config",
            "target_path":"/app/secret",
            "sensitive":true,
            "readonly":true,
            "operation":"replace",
            "value":"managed-file-secret-canary"
        }
    ]);
    let (status, created) = body(
        harness
            .create(Some("managed-file-drift-create"), &candidate)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let revision = harness.store.read_metadata(app_id).unwrap().draft_revision;
    let drifted = harness
        .store
        .app_directory(app_id)
        .join("config-revisions")
        .join(revision.expect("configured app").to_string())
        .join("files/secret/secret-config");
    fs::set_permissions(&drifted, fs::Permissions::from_mode(0o600)).unwrap();
    harness.compose_actions.lock().unwrap().clear();

    let response = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/deployments"),
            Some("managed-file-drift-deploy"),
            &json!({
                "expected_draft_revision":revision,
                "expected_active_release_id":null,
                "expected_pending_release_id":null,
                "expected_actual_release_id":null,
                "expected_actual_container_id":null,
                "acknowledge_non_rollbackable_data":false
            }),
        )
        .await;
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["code"], "APP_CONFIG_INVALID");
    assert!(!response.to_string().contains("secret-config"));
    assert!(!response.to_string().contains(drifted.to_str().unwrap()));
    assert!(harness.compose_actions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn slug_is_short_unique_and_absent_from_mutable_draft() {
    let harness = Harness::new().await;
    let mut invalid = draft("secret");
    invalid["slug"] = json!("abcdefghijklmnopqrstu");
    assert_eq!(
        harness
            .create(Some("slug-too-long-0001"), &invalid)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let (status, created) = body(
        harness
            .create(Some("slug-create-0001"), &draft("secret"))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    assert_eq!(
        harness
            .create(Some("slug-duplicate-0001"), &draft("secret"))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let mut update = mutable_draft("secret");
    update["slug"] = json!("renamed");
    let response = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some("slug-update-0001"),
            &json!({
                "expected_revision": created["app"]["config_revision"],
                "draft": update,
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let header = fs::read_to_string(harness.apps.join(app_id).join("app.toml")).unwrap();
    assert!(header.contains("schema_version = 3"));
    assert!(header.contains("slug = \"example\""));
    assert!(!header.contains("project_name"));
}

#[tokio::test]
async fn create_requires_auto_deploy_ack_and_persists_enabled_state() {
    let harness = Harness::new().await;
    let mut enabled = draft("auto-secret");
    enabled["auto_deploy_enabled"] = json!(true);
    let (status, rejected) = body(
        harness
            .create(Some("m5-auto-create-without-ack"), &enabled)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");

    enabled["slug"] = json!("auto-enabled");
    enabled["auto_deploy_acknowledged"] = json!(true);
    let key = "m5-auto-create-with-ack";
    let (status, created) = body(harness.create(Some(key), &enabled).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let metadata = harness.store.read_metadata(app_id).unwrap();
    assert!(metadata.auto_deploy_enabled);
    assert_eq!(metadata.poll_interval_seconds, 300);

    let (status, replay) = body(harness.create(Some(key), &enabled).await).await;
    assert_eq!(status, StatusCode::CREATED, "{replay}");
    assert!(
        harness
            .store
            .read_metadata(app_id)
            .unwrap()
            .auto_deploy_enabled
    );
}

#[tokio::test]
async fn same_idempotency_key_with_different_secret_is_rejected() {
    let harness = Harness::new().await;
    let key = "m3-create-example-0002";
    assert_eq!(
        harness
            .create(Some(key), &draft("one-secret"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        harness
            .create(Some(key), &draft("different-secret"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn interrupted_create_reconciles_the_filesystem_commit_without_a_second_app() {
    let harness = Harness::new().await;
    let key = "m3-create-interrupted-0001";
    let request = json!({"slug":"unconfigured-resume"});
    let (status, first) = body(
        harness
            .mutate("POST", "/api/v1/apps", Some(key), &request)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["app"]["deployment_status"], "UNCONFIGURED");
    assert!(first["app"]["config_revision"].is_null());
    sqlx::query("UPDATE idempotency_records SET status='interrupted',response_status=NULL,response_body=NULL WHERE route='/api/v1/apps'")
        .execute(harness.database.pool())
        .await
        .unwrap();

    let (status, resumed) = body(
        harness
            .mutate("POST", "/api/v1/apps", Some(key), &request)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let resumed: Value = serde_json::from_str(&resumed).unwrap();
    assert_eq!(resumed["app"]["id"], first["app"]["id"]);
    assert_eq!(
        fs::read_dir(&harness.apps)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
            .count(),
        1
    );
}

#[tokio::test]
async fn unconfigured_app_first_revision_is_nullable_and_docker_actions_fail_closed() {
    let harness = Harness::new().await;
    let request = json!({"slug":"example-app"});
    let (status, created) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/apps",
                Some("unconfigured-create"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    assert_eq!(created["app"]["deployment_status"], "UNCONFIGURED");
    assert!(created["app"]["config_revision"].is_null());
    let app_id = created["app"]["id"].as_str().unwrap();
    let app_uuid: Uuid = app_id.parse().unwrap();
    harness.compose_actions.lock().unwrap().clear();

    let webhook_status = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/apps/{app_id}/webhook"))
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, webhook_status) = body(webhook_status).await;
    assert_eq!(status, StatusCode::CONFLICT, "{webhook_status}");
    assert_eq!(
        serde_json::from_str::<Value>(&webhook_status).unwrap()["code"],
        "APP_UNCONFIGURED"
    );
    let secret = URL_SAFE_NO_PAD.encode([5_u8; 32]);
    let webhook_configure = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/webhook"),
            Some("unconfigured-webhook"),
            &json!({"expected_metadata_revision":null,"secret":secret}),
        )
        .await;
    let (status, webhook_configure) = body(webhook_configure).await;
    assert_eq!(status, StatusCode::CONFLICT, "{webhook_configure}");
    assert_eq!(
        serde_json::from_str::<Value>(&webhook_configure).unwrap()["code"],
        "APP_UNCONFIGURED"
    );
    let ingress = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/hooks/v1/apps/{app_id}/registry"))
                .header(header::HOST, "hooks.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, ingress) = body(ingress).await;
    assert_eq!(status, StatusCode::CONFLICT, "{ingress}");
    assert_eq!(
        serde_json::from_str::<Value>(&ingress).unwrap()["code"],
        "APP_UNCONFIGURED"
    );
    let app_path = harness.apps.join(app_id);
    assert!(!app_path.join("webhook.toml").exists());
    assert!(!app_path.join("webhook-secret-revisions").exists());
    assert!(
        harness
            .state
            .webhooks
            .as_ref()
            .unwrap()
            .poll_states
            .get(app_uuid)
            .await
            .unwrap()
            .is_none()
    );

    let start = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{app_id}/actions/start"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "unconfigured-start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, start) = body(start).await;
    assert_eq!(status, StatusCode::CONFLICT, "{start}");
    assert_eq!(
        serde_json::from_str::<Value>(&start).unwrap()["code"],
        "APP_UNCONFIGURED"
    );
    assert!(harness.compose_actions.lock().unwrap().is_empty());

    let validate = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/validate"),
            None,
            &json!({"draft":mutable_draft("first-secret")}),
        )
        .await;
    let (status, validate) = body(validate).await;
    assert_eq!(status, StatusCode::OK, "{validate}");
    assert_eq!(
        harness.compose_actions.lock().unwrap().as_slice(),
        &[ComposeAction::Validate]
    );
    harness.compose_actions.lock().unwrap().clear();

    let first = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some("unconfigured-first-revision"),
            &json!({"expected_revision":null,"draft":mutable_draft("first-secret")}),
        )
        .await;
    let (status, first) = body(first).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first: Value = serde_json::from_str(&first).unwrap();
    assert!(first["app"]["config_revision"].is_string());

    let stale_null = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some("unconfigured-stale-null"),
            &json!({"expected_revision":null,"draft":mutable_draft("other-secret")}),
        )
        .await;
    assert_eq!(stale_null.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn postgresql_preset_is_versioned_idempotent_and_never_echoes_password() {
    let harness = Harness::new().await;
    let descriptors = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/app-presets")
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, descriptors) = body(descriptors).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&descriptors).unwrap()[0]["id"],
        "postgresql"
    );

    let password = "POSTGRES_PRESET_SECRET_CANARY";
    let request = json!({
        "slug":"postgres",
        "preset_id":"postgresql",
        "preset_schema_version":1,
        "variables":{"major":"18","username":"postgres","database":"postgres","password":password,"initdb_args":""}
    });
    let (status, created) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/apps/from-preset",
                Some("postgresql-preset-create"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(!created.contains(password));
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let loaded = solodock::app_store::config_revision::load_verified(
        &harness.store.app_directory(app_id),
        revision,
        harness.store.integrity_key().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        &loaded.metadata.volumes[0],
        solodock::domain::VolumeInput::Owned { target_path, .. } if target_path == "/var/lib/postgresql"
    ));
    assert!(loaded.metadata.service_discovery_enabled);
    assert!(loaded.metadata.ports.is_empty());

    let (status, replay) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/apps/from-preset",
                Some("postgresql-preset-create"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let replay: Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(replay["app"]["id"], created["app"]["id"]);
    assert_eq!(replay["idempotency_replayed"], true);
}

#[tokio::test]
async fn interrupted_update_resumes_while_old_revision_awaits_manual_cleanup() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(Some("m3-create-for-update-0001"), &draft("old-secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let old_revision = created["app"]["config_revision"].as_str().unwrap();
    let mut updated = mutable_draft("new-secret");
    updated["display_name"] = json!("Updated");
    let request = json!({"expected_revision":old_revision,"draft":updated});
    let route = format!("/api/v1/apps/{app_id}/draft");
    let key = "m3-update-interrupted-0001";
    let (status, first) = body(harness.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let new_revision = first["app"]["config_revision"].as_str().unwrap();
    let old_path = harness
        .apps
        .join(app_id)
        .join("config-revisions")
        .join(old_revision);
    assert!(old_path.exists(), "runtime refresh must be read-only");
    harness.store.scan().unwrap();
    assert!(
        old_path.exists(),
        "startup recovery must not bypass manual cleanup confirmation"
    );
    sqlx::query("UPDATE idempotency_records SET status='interrupted',response_status=NULL,response_body=NULL WHERE route=?")
        .bind(&route)
        .execute(harness.database.pool())
        .await
        .unwrap();

    let (status, resumed) = body(harness.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK);
    let resumed: Value = serde_json::from_str(&resumed).unwrap();
    assert_eq!(resumed["app"]["config_revision"], new_revision);
    assert_eq!(resumed["app"]["display_name"], "Updated");
}

#[tokio::test]
async fn unregister_is_two_stage_data_preserving_and_idempotently_replayed() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(Some("m3-create-for-delete-0001"), &draft("delete-secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let app_uuid = app_id.parse::<Uuid>().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let webhooks = harness.state.webhooks.as_ref().unwrap();
    let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned());
    let hook_metadata = webhooks
        .store
        .configure(app_uuid, None, Uuid::new_v4(), &secret)
        .unwrap();
    let hook_revision = hook_metadata.secret_revision.unwrap();
    webhooks
        .poll_states
        .accept_webhook(
            app_uuid,
            hook_revision,
            &[7; 32],
            Uuid::new_v4(),
            "deletion-generation",
            true,
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(
        harness
            .apps
            .join(app_id)
            .join("webhook-secret-revisions")
            .join(hook_revision.to_string()),
    )
    .unwrap();
    let missing_revision_status = webhooks.store.status(app_uuid).unwrap();
    assert!(missing_revision_status.configured);
    assert!(missing_revision_status.degraded);
    assert_eq!(
        missing_revision_status.metadata_revision,
        Some(hook_metadata.metadata_revision)
    );
    let (status, missing_revision_preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let missing_revision_preview: Value = serde_json::from_str(&missing_revision_preview).unwrap();
    assert_eq!(missing_revision_preview["webhook_configured"], true);
    webhooks
        .store
        .configure(
            app_uuid,
            Some(hook_metadata.metadata_revision),
            Uuid::new_v4(),
            &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".to_owned()),
        )
        .unwrap();
    std::fs::write(harness.apps.join(app_id).join("webhook.toml"), "corrupt").unwrap();
    let degraded_status = webhooks.store.status(app_uuid).unwrap();
    assert!(!degraded_status.configured);
    assert!(degraded_status.degraded);
    let (status, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let preview: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(preview["webhook_configured"], true);
    let request = json!({
        "confirmation_token": preview["confirmation_token"],
        "slug": "example",
        "expected_revision": revision,
        "remove_container": false
    });
    let route = format!("/api/v1/apps/{app_id}");
    let key = "m3-delete-example-0001";
    let (status, deleted) = body(harness.mutate("DELETE", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK);
    let deleted: Value = serde_json::from_str(&deleted).unwrap();
    assert_eq!(deleted["unregistered"], true);
    assert_eq!(deleted["container_removed"], false);
    assert_eq!(deleted["orphan_warning"], true);
    assert!(!harness.apps.join(app_id).exists());
    assert!(webhooks.poll_states.get(app_uuid).await.unwrap().is_none());
    assert_eq!(
        webhooks.poll_states.webhook_replay_count().await.unwrap(),
        0
    );

    let (status, replayed) =
        body(harness.mutate("DELETE", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK);
    let replayed: Value = serde_json::from_str(&replayed).unwrap();
    assert_eq!(replayed["idempotency_replayed"], true);
}

#[tokio::test]
async fn deletion_facts_ignore_stale_projection_union_active_and_draft_and_detect_changes() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-deletion-facts"),
                &draft_with_owned_volume("secret-a", "active-data"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let active_revision = created["app"]["config_revision"].as_str().unwrap();
    harness.publish_active(
        app_id.parse().unwrap(),
        Uuid::new_v4(),
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );

    let route = format!("/api/v1/apps/{app_id}/draft");
    let update = json!({
        "expected_revision":active_revision,
        "draft":mutable(draft_with_owned_volume("secret-b", "draft-data"))
    });
    let (status, updated) = body(
        harness
            .mutate("PUT", &route, Some("m3-update-deletion-facts"), &update)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let updated: Value = serde_json::from_str(&updated).unwrap();
    let draft_revision = updated["app"]["config_revision"].as_str().unwrap();

    // A degraded/stale in-memory projection cannot authorize deletion; the
    // endpoint derives its token facts from the filesystem instead.
    harness.catalog.replace(&Default::default());
    let (status, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let preview: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(preview["expected_revision"], draft_revision);
    assert_eq!(preview["active_config_revision"], active_revision);
    let volumes = preview["retained"]["owned_volumes"].as_array().unwrap();
    assert!(volumes.iter().any(|item| {
        item["name"].as_str().unwrap().ends_with(".active-data")
            && item["configured_in"] == "active"
            && item["exists"] == false
    }));
    assert!(volumes.iter().any(|item| {
        item["name"].as_str().unwrap().ends_with(".draft-data")
            && item["configured_in"] == "draft"
            && item["exists"] == false
    }));

    let changed = json!({
        "expected_revision":draft_revision,
        "draft":mutable(draft_with_owned_volume("secret-c", "changed-data"))
    });
    let (status, _) = body(
        harness
            .mutate("PUT", &route, Some("m3-change-after-preview"), &changed)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let delete = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":draft_revision,
        "remove_container":false
    });
    let (status, deleted) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("m3-delete-stale-facts"),
                &delete,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{deleted}");
    assert!(harness.apps.join(app_id).exists());
}

#[tokio::test]
async fn external_only_deletion_inventory_preserves_per_revision_alias_facts() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-external-only-facts"),
                &external_only_draft("secret-a", "postgres"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let active_revision = created["app"]["config_revision"].as_str().unwrap();
    harness.publish_active(
        app_id,
        Uuid::new_v4(),
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let update = json!({
        "expected_revision":active_revision,
        "draft":mutable(external_only_draft("secret-b", "database"))
    });
    let (status, updated) = body(
        harness
            .mutate(
                "PUT",
                &format!("/api/v1/apps/{app_id}/draft"),
                Some("m3-update-external-only-facts"),
                &update,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let catalog = harness.catalog.get(app_id).unwrap();
    assert_eq!(
        catalog.active_network_plan.unwrap().external[0].aliases,
        ["postgres"]
    );
    assert_eq!(
        catalog
            .draft
            .unwrap()
            .networks
            .into_iter()
            .find_map(|network| match network {
                solodock::domain::NetworkInput::External { aliases, .. } => Some(aliases),
                solodock::domain::NetworkInput::OwnedDefault => None,
            })
            .unwrap(),
        ["database"]
    );

    let (status, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let networks = preview["retained"]["networks"].as_array().unwrap();
    assert_eq!(networks.len(), 2);
    assert!(networks.iter().all(|network| network["kind"] == "external"));
    assert!(!networks.iter().any(|network| {
        network["name"]
            .as_str()
            .is_some_and(|name| name.ends_with("-default"))
    }));
    assert!(networks.iter().any(|network| {
        network["aliases"] == json!(["postgres"]) && network["configured_in"] == "active"
    }));
    assert!(networks.iter().any(|network| {
        network["aliases"] == json!(["database"]) && network["configured_in"] == "draft"
    }));
}

#[tokio::test]
async fn remove_rechecks_all_candidates_after_effect_marker_before_compose() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let (_, created) = body(
        harness
            .create(Some("m3-create-remove-race"), &draft("secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let valid = owned_container('a', app_id, release_id, true);
    docker.set(vec![vec![valid.clone()]]);
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":true}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let unmanaged = owned_container('b', app_id, release_id, false);
    docker.set(vec![
        vec![valid.clone()],
        vec![valid.clone()],
        vec![valid, unmanaged],
    ]);
    harness.compose_actions.lock().unwrap().clear();
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":true
    });
    let (status, _) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("m3-remove-candidate-race"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !harness
            .compose_actions
            .lock()
            .unwrap()
            .contains(&ComposeAction::Remove)
    );
    assert!(harness.store.app_directory(app_id).exists());
}

#[tokio::test]
async fn remove_stops_with_the_active_release_grace_before_removing() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let mut input = draft("secret");
    input["stop_grace_period_seconds"] = json!(60);
    let (_, created) = body(harness.create(Some("m3-create-remove-grace"), &input).await).await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let valid = owned_container('a', app_id, release_id, true);
    docker.set(vec![vec![valid.clone()]]);
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":true}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    docker.set(vec![
        vec![valid.clone()],
        vec![valid.clone()],
        vec![valid],
        Vec::new(),
    ]);
    harness.compose_contexts.lock().unwrap().clear();
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":true
    });
    let (status, response) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("m3-remove-with-grace"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        harness.compose_contexts.lock().unwrap().as_slice(),
        [(ComposeAction::Stop, 60), (ComposeAction::Remove, 60)]
    );
}

#[tokio::test]
async fn failed_final_deletion_fact_recheck_unblocks_future_streams() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let (_, created) = body(
        harness
            .create(Some("m3-create-stream-rollback"), &draft("secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let valid = owned_container('a', app_id, release_id, true);
    docker.set(vec![vec![valid.clone()]]);
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    docker.set(vec![
        vec![valid],
        vec![owned_container('b', app_id, release_id, false)],
    ]);
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":false
    });
    let (status, _) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("m3-stream-barrier-rollback"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let permit = harness.state.stream_gate.acquire_for(
        "new-session".into(),
        solodock::api::streams::StreamKind::Events,
        app_id,
        &harness.state.shutdown,
    );
    assert!(permit.is_some());
    assert!(harness.store.app_directory(app_id).exists());
}

#[tokio::test]
async fn deletion_rechecks_container_after_slow_resource_inventory() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-resource-candidate-race"),
                &draft_with_owned_volume("secret", "data"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let valid = owned_container('a', app_id, release_id, true);
    docker.set(vec![vec![valid.clone()]]);
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":false
    });
    // The delete performs one initial inventory (volume + default network),
    // then a second inventory after its stream barrier. Pause in that second
    // inventory, after its first candidate enumeration.
    docker.block_resource_inspect_after(3);
    let delete_route = format!("/api/v1/apps/{app_id}");
    let delete = harness.mutate(
        "DELETE",
        &delete_route,
        Some("m3-resource-candidate-race"),
        &request,
    );
    let replace = async {
        docker.wait_for_blocked_resource_inspect().await;
        docker.set(vec![vec![owned_container('b', app_id, release_id, false)]]);
        docker.release_resource_inspect();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(delete, replace)
    })
    .await
    .unwrap();
    let (status, _) = body(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(harness.store.app_directory(app_id).exists());
    assert!(
        harness
            .state
            .stream_gate
            .acquire_for(
                "replacement-session".into(),
                solodock::api::streams::StreamKind::Events,
                app_id,
                &harness.state.shutdown,
            )
            .is_some()
    );
}

#[tokio::test]
async fn deletion_retains_visible_tombstone_until_catalog_publication_recovers() {
    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-deletion-publication"),
                &draft_with_owned_volume("secret", "data"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let valid = owned_container('a', app_id, release_id, true);
    docker.set(vec![vec![valid]]);
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":false
    });
    docker.block_resource_inspect_after(3);
    let unsafe_entry = harness.apps.join("unsafe-link");
    let delete_route = format!("/api/v1/apps/{app_id}");
    let delete = harness.mutate(
        "DELETE",
        &delete_route,
        Some("m3-delete-publication-recovery"),
        &request,
    );
    let break_scan = async {
        docker.wait_for_blocked_resource_inspect().await;
        std::os::unix::fs::symlink("/outside-solodock", &unsafe_entry).unwrap();
        docker.release_resource_inspect();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(delete, break_scan)
    })
    .await
    .unwrap();
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["projection_warning"], "FILESYSTEM_RESCAN_FAILED");
    assert!(harness.catalog.get(app_id).is_some());
    assert!(!harness.store.app_directory(app_id).exists());
    assert_eq!(
        fs::read_dir(harness.apps.join(".trash")).unwrap().count(),
        1
    );
    assert!(
        harness
            .state
            .stream_gate
            .acquire_for(
                "post-delete-session".into(),
                solodock::api::streams::StreamKind::Events,
                app_id,
                &harness.state.shutdown,
            )
            .is_none()
    );

    fs::remove_file(&unsafe_entry).unwrap();
    let (status, replayed) = body(
        harness
            .mutate(
                "DELETE",
                &format!("/api/v1/apps/{app_id}"),
                Some("m3-delete-publication-recovery"),
                &request,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert!(harness.catalog.get(app_id).is_none());
    assert_eq!(
        fs::read_dir(harness.apps.join(".trash")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn tombstone_failure_rolls_back_stream_barrier() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(Some("m3-create-tombstone-rollback"), &draft("secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision = created["app"]["config_revision"].as_str().unwrap();
    let (_, preview) = body(
        harness
            .mutate(
                "POST",
                &format!("/api/v1/apps/{app_id}/deletion-preview"),
                None,
                &json!({"remove_container":false}),
            )
            .await,
    )
    .await;
    let preview: Value = serde_json::from_str(&preview).unwrap();
    let request = json!({
        "confirmation_token":preview["confirmation_token"],
        "slug":"example",
        "expected_revision":revision,
        "remove_container":false
    });
    let route = format!("/api/v1/apps/{app_id}");
    let key = "m3-tombstone-barrier-rollback";
    let operation_id = match harness
        .idempotency
        .claim(
            &route,
            key,
            &delete_fingerprint(&harness, &route, &request),
            Uuid::new_v4(),
        )
        .await
        .unwrap()
    {
        solodock::mutation::ClaimResult::New(id) => id,
        _ => panic!("new claim expected"),
    };
    let trash = harness.apps.join(".trash");
    fs::create_dir(&trash).unwrap();
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o700)).unwrap();
    let conflicting_target = trash.join(format!("{app_id}-{operation_id}"));
    fs::create_dir(&conflicting_target).unwrap();
    fs::set_permissions(&conflicting_target, fs::Permissions::from_mode(0o700)).unwrap();
    harness
        .idempotency
        .mark_interrupted(&route, key, Uuid::new_v4())
        .await
        .unwrap();
    let (status, _) = body(harness.mutate("DELETE", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        harness
            .state
            .stream_gate
            .acquire_for(
                "new-session".into(),
                solodock::api::streams::StreamKind::Events,
                app_id,
                &harness.state.shutdown,
            )
            .is_some()
    );
    assert!(harness.store.app_directory(app_id).exists());
}

#[tokio::test]
async fn delete_resumes_after_token_consumption_and_tombstone_failpoints() {
    for tombstoned in [false, true] {
        let harness = Harness::new().await;
        let (_, created) = body(
            harness
                .create(Some("m3-create-delete-failpoint"), &draft("delete-secret"))
                .await,
        )
        .await;
        let created: Value = serde_json::from_str(&created).unwrap();
        let app_id = created["app"]["id"].as_str().unwrap();
        let revision = created["app"]["config_revision"].as_str().unwrap();
        let route = format!("/api/v1/apps/{app_id}");
        let (_, preview) = body(
            harness
                .mutate(
                    "POST",
                    &format!("{route}/deletion-preview"),
                    None,
                    &json!({"remove_container":false}),
                )
                .await,
        )
        .await;
        let preview: Value = serde_json::from_str(&preview).unwrap();
        let request = json!({
            "confirmation_token":preview["confirmation_token"],
            "slug":"example",
            "expected_revision":revision,
            "remove_container":false,
        });
        let key = if tombstoned {
            "m3-delete-after-tombstone"
        } else {
            "m3-delete-after-consume"
        };
        let operation_id = match harness
            .idempotency
            .claim(
                &route,
                key,
                &delete_fingerprint(&harness, &route, &request),
                Uuid::new_v4(),
            )
            .await
            .unwrap()
        {
            solodock::mutation::ClaimResult::New(id) => id,
            _ => panic!("new delete claim expected"),
        };
        let token_hash = Sha256::digest(request["confirmation_token"].as_str().unwrap().as_bytes());
        sqlx::query("UPDATE deletion_previews SET consumed_at=? WHERE token_hash=?")
            .bind(solodock::db::format_time(time::OffsetDateTime::now_utc()).unwrap())
            .bind(token_hash.as_slice())
            .execute(harness.database.pool())
            .await
            .unwrap();
        if tombstoned {
            harness
                .store
                .tombstone(app_id.parse().unwrap(), operation_id)
                .unwrap();
        }
        harness
            .idempotency
            .mark_interrupted(&route, key, Uuid::new_v4())
            .await
            .unwrap();
        let (status, response) =
            body(harness.mutate("DELETE", &route, Some(key), &request).await).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "tombstoned={tombstoned}: {response}"
        );
        assert!(!harness.apps.join(app_id).exists());
        assert!(harness.catalog.get(app_id.parse().unwrap()).is_none());
        if tombstoned {
            assert!(
                !harness
                    .store
                    .tombstone_path(app_id.parse().unwrap(), operation_id)
                    .exists()
            );
        }
    }
}

#[tokio::test]
async fn projection_reconciler_recovers_without_another_mutation() {
    let harness = Harness::new().await;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let worker = solodock::api::mutations::start_projection_reconciler(
        harness.state.clone(),
        cancellation.clone(),
    );
    let services = harness.state.m3.as_ref().unwrap();
    let mut connection = harness.database.pool().acquire().await.unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *connection)
        .await
        .unwrap();
    services
        .projection_degraded
        .store(true, std::sync::atomic::Ordering::Release);
    services.reconcile_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        services
            .projection_degraded
            .load(std::sync::atomic::Ordering::Acquire)
    );
    sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while services
            .projection_degraded
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn degraded_recovery_never_removes_a_live_stream_secret_pattern() {
    let harness = Harness::new().await;
    let canary = "DEGRADED_STREAM_SECRET_CANARY";
    let (status, created) = body(
        harness
            .create(Some("m4-redactor-degraded-0001"), &draft(canary))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    assert_eq!(
        harness.state.redactor.redact(canary.as_bytes()),
        b"[REDACTED]"
    );

    fs::write(harness.apps.join(app_id).join("app.toml"), b"not = [valid").unwrap();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let worker = solodock::api::mutations::start_projection_reconciler(
        harness.state.clone(),
        cancellation.clone(),
    );
    let services = harness.state.m3.as_ref().unwrap();
    services
        .projection_degraded
        .store(true, std::sync::atomic::Ordering::Release);
    services.reconcile_notify.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while harness.catalog.get(app_id.parse().unwrap()).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        services
            .projection_degraded
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert_eq!(
        harness.state.redactor.redact(canary.as_bytes()),
        b"[REDACTED]",
        "an existing log framer shares this dynamic redactor"
    );
    cancellation.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn route_specific_body_limits_reject_lifecycle_and_large_delete_payloads() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-body-limit-0001"),
                &draft("body-limit-secret"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let lifecycle = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{app_id}/actions/start"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "m3-body-limit-start")
                .body(Body::from("not-empty"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(lifecycle.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let preview = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{app_id}/deletion-preview"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"padding\":\"{}\"}}",
                    "x".repeat(20 * 1024)
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preview.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn validate_returns_the_exact_compose_artifact_given_to_the_runner() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(
                Some("m3-create-validate-artifact"),
                &draft("validate-secret"),
            )
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let mut candidate = mutable_draft("validate-secret");
    candidate["stop_grace_period_seconds"] = json!(60);
    candidate["files"] = json!([{
        "logical_name":"settings", "target_path":"/app/settings.toml",
        "sensitive":false, "readonly":true, "content":"mode='preview'"
    }]);
    let response = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/validate"),
            None,
            &json!({"draft":candidate}),
        )
        .await;
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let response: Value = serde_json::from_str(&response).unwrap();
    let returned = response["compose_yaml"].as_str().unwrap().as_bytes();
    let captured = harness.validated_yaml.lock().unwrap().clone().unwrap();
    let captured_text = String::from_utf8_lossy(&captured);
    assert!(captured_text.contains("stop_grace_period:"));
    assert!(captured_text.contains("60s"));
    assert_eq!(returned, captured);
    assert!(
        response["compose_yaml"]
            .as_str()
            .unwrap()
            .contains("settings")
    );
}

#[tokio::test]
async fn draft_validation_returns_safe_field_issues_for_preview_and_save() {
    let harness = Harness::new().await;
    let (_, created) = body(
        harness
            .create(Some("field-issues-create"), &draft("stored-secret"))
            .await,
    )
    .await;
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id = created["app"]["id"].as_str().unwrap();
    let revision = created["app"]["config_revision"].clone();
    let mut overlapping_files = mutable_draft("replacement-secret");
    overlapping_files["files"] = json!([
        {
            "logical_name":"config", "target_path":"/etc/app",
            "sensitive":false, "readonly":true, "content":"root"
        },
        {
            "logical_name":"settings", "target_path":"/etc/app/config.json",
            "sensitive":false, "readonly":true, "content":"nested"
        }
    ]);
    let overlap = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/validate"),
            None,
            &json!({"draft":overlapping_files}),
        )
        .await;
    let (status, overlap) = body(overlap).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{overlap}");
    let overlap: Value = serde_json::from_str(&overlap).unwrap();
    assert_eq!(overlap["issues"][0]["path"], "files[1].target_path");

    let mut candidate = mutable_draft("secret-canary-must-not-leak");
    candidate["health"] = json!({
        "policy":"healthy",
        "http":{
            "client":"curl", "scheme":"http", "host":"127.0.0.1",
            "port":3000, "path":"/readyz", "interval_seconds":10,
            "timeout_seconds":5, "retries":11, "start_period_seconds":30
        }
    });

    let preview = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/validate"),
            None,
            &json!({"draft":candidate.clone()}),
        )
        .await;
    let (status, preview) = body(preview).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{preview}");
    assert!(!preview.contains("secret-canary-must-not-leak"));
    let preview: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(preview["code"], "CONFIG_INVALID");
    assert_eq!(preview["issues"][0]["path"], "health.http.retries");
    assert_eq!(preview["issues"][0]["code"], "OUT_OF_RANGE");

    let saved = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some("field-issues-save"),
            &json!({"expected_revision":revision,"draft":candidate.clone()}),
        )
        .await;
    let (status, saved) = body(saved).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{saved}");
    assert!(!saved.contains("secret-canary-must-not-leak"));
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["issues"][0]["path"], "health.http.retries");

    let replay = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some("field-issues-save"),
            &json!({"expected_revision":revision,"draft":candidate}),
        )
        .await;
    let (status, replay) = body(replay).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{replay}");
    assert!(!replay.contains("secret-canary-must-not-leak"));
    let replay: Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(replay["issues"], saved["issues"]);
    assert_eq!(replay["idempotency_replayed"], true);
}

#[tokio::test]
async fn draft_save_rejects_a_cross_application_read_write_bind_ancestor() {
    let bind_root = tempfile::tempdir().unwrap();
    let parent = bind_root.path().join("parent");
    let child = parent.join("child");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

    let harness = Harness::new().await;
    harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);

    let mut writer = draft("writer-secret");
    writer["slug"] = json!("writer");
    writer["binds"] = json!([{
        "source": parent,
        "target_path": "/data",
        "readonly": false,
        "acknowledge_non_rollbackable": true
    }]);
    let (status, created) = body(harness.create(Some("bind-ancestor-writer"), &writer).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let writer_id: Uuid = serde_json::from_str::<Value>(&created).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    harness.publish_active(
        writer_id,
        Uuid::new_v4(),
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );

    let (status, unconfigured) = body(
        harness
            .mutate(
                "POST",
                "/api/v1/apps",
                Some("bind-ancestor-reader-create"),
                &json!({"slug": "reader"}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let reader_id = serde_json::from_str::<Value>(&unconfigured).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut reader = mutable_draft("reader-secret");
    reader["binds"] = json!([{
        "source": child,
        "target_path": "/data",
        "readonly": true,
        "acknowledge_non_rollbackable": false
    }]);
    let (status, rejected) = body(
        harness
            .mutate(
                "PUT",
                &format!("/api/v1/apps/{reader_id}/draft"),
                Some("bind-ancestor-reader"),
                &json!({"expected_revision": null, "draft": reader}),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let rejected: Value = serde_json::from_str(&rejected).unwrap();
    assert_eq!(rejected["code"], "BIND_SOURCE_ANCESTOR_CONFLICT");
    assert_eq!(rejected["issues"][0]["path"], "binds");
}

#[tokio::test]
async fn live_bind_ancestor_blocks_start_but_preserves_stop_and_corrective_edit() {
    let bind_root = tempfile::tempdir().unwrap();
    let parent = bind_root.path().join("parent");
    let child = parent.join("child");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);

    let mut writer = draft("writer-secret");
    writer["slug"] = json!("writer");
    writer["binds"] = json!([{
        "source": parent,
        "target_path": "/data",
        "readonly": false,
        "acknowledge_non_rollbackable": true
    }]);
    let (status, writer) = body(harness.create(Some("live-bind-writer"), &writer).await).await;
    assert_eq!(status, StatusCode::CREATED, "{writer}");
    let writer: Value = serde_json::from_str(&writer).unwrap();
    let writer_id: Uuid = writer["app"]["id"].as_str().unwrap().parse().unwrap();
    let writer_revision = writer["app"]["config_revision"].as_str().unwrap();

    let mut reader = draft("reader-secret");
    reader["binds"] = json!([{
        "source": child,
        "target_path": "/data",
        "readonly": true,
        "acknowledge_non_rollbackable": false
    }]);
    let (status, reader) = body(harness.create(Some("live-bind-reader"), &reader).await).await;
    assert_eq!(status, StatusCode::CREATED, "{reader}");
    let reader: Value = serde_json::from_str(&reader).unwrap();
    let reader_id: Uuid = reader["app"]["id"].as_str().unwrap().parse().unwrap();
    let writer_release = Uuid::new_v4();
    let reader_release = Uuid::new_v4();
    harness.publish_active(
        writer_id,
        writer_release,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    harness.publish_active(
        reader_id,
        reader_release,
        &format!("registry.example/app@sha256:{}", "b".repeat(64)),
    );
    harness.compose_actions.lock().unwrap().clear();

    docker.set(vec![vec![owned_container(
        'b',
        reader_id,
        reader_release,
        true,
    )]]);
    let start = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{writer_id}/actions/start"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "live-bind-writer-start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, start) = body(start).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{start}");
    assert_eq!(
        serde_json::from_str::<Value>(&start).unwrap()["code"],
        "BIND_SOURCE_ANCESTOR_CONFLICT"
    );
    assert!(harness.compose_actions.lock().unwrap().is_empty());

    docker.set(vec![Vec::new()]);
    let stop = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{writer_id}/actions/stop"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "live-bind-writer-stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stop.status(), StatusCode::OK);

    let corrective = mutable_draft("writer-secret");
    let saved = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{writer_id}/draft"),
            Some("live-bind-writer-corrective-edit"),
            &json!({"expected_revision": writer_revision, "draft": corrective}),
        )
        .await;
    assert_eq!(saved.status(), StatusCode::OK);
}

#[tokio::test]
async fn legacy_intra_app_bind_ancestor_conflict_does_not_block_stop() {
    let bind_root = tempfile::tempdir().unwrap();
    let parent = bind_root.path().join("parent");
    let child = parent.join("child");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

    let docker = Arc::new(ScriptedDocker::default());
    let harness = Harness::new_with_docker(docker.clone()).await;
    harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);

    let mut input = draft("legacy-bind-secret");
    input["binds"] = json!([{
        "source": parent,
        "target_path": "/data",
        "readonly": false,
        "acknowledge_non_rollbackable": true
    }]);
    let (status, created) = body(harness.create(Some("legacy-bind-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision_id: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let legacy_binds = serde_json::from_value(json!([
        {
            "source": parent,
            "target_path": "/data",
            "readonly": false,
            "acknowledge_non_rollbackable": true
        },
        {
            "source": child,
            "target_path": "/child",
            "readonly": true,
            "acknowledge_non_rollbackable": false
        }
    ]))
    .unwrap();
    harness.install_legacy_signed_binds(app_id, revision_id, legacy_binds);
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "c".repeat(64)),
    );
    harness.compose_actions.lock().unwrap().clear();
    for (action, key) in [
        ("start", "legacy-bind-start"),
        ("restart", "legacy-bind-restart"),
    ] {
        let response = harness
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/apps/{app_id}/actions/{action}"))
                    .header(header::HOST, "solodock.example.com")
                    .header(header::ORIGIN, "https://solodock.example.com")
                    .header(header::COOKIE, &harness.cookie)
                    .header("x-csrf-token", &harness.csrf)
                    .header("idempotency-key", key)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, response) = body(response).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["code"],
            "BIND_SOURCE_ANCESTOR_CONFLICT"
        );
    }
    assert!(harness.compose_actions.lock().unwrap().is_empty());
    let running = owned_container('c', app_id, release_id, true);
    let mut exited = running.clone();
    exited.status = ContainerStatus::Exited;
    docker.set(vec![
        vec![running.clone()],
        vec![running.clone()],
        vec![running],
        vec![exited],
    ]);

    let stopped = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{app_id}/actions/stop"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "legacy-bind-stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, stopped) = body(stopped).await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(
        harness.compose_actions.lock().unwrap().as_slice(),
        &[ComposeAction::Stop]
    );
}

#[tokio::test]
async fn deployment_engine_post_stop_guard_blocks_candidate_apply_after_symlink_swap() {
    let bind_root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = bind_root.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = Arc::new(StopScenario {
        source: source.clone(),
        replacement: Some(outside.path().to_path_buf()),
        mutate_on_stop: 1,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        phase: std::sync::atomic::AtomicUsize::new(0),
    });
    let docker = Arc::new(ScenarioDocker {
        scenario: Some(scenario.clone()),
        ..ScenarioDocker::default()
    });
    let harness =
        Harness::new_with_components(docker.clone(), Some(scenario), Some(Arc::new(NoopPuller)))
            .await;
    harness
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);

    let mut input = draft("candidate-guard-secret");
    input["binds"] = json!([{
        "source": source,
        "target_path": "/data",
        "readonly": true,
        "acknowledge_non_rollbackable": false
    }]);
    let (status, created) =
        body(harness.create(Some("candidate-guard-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision_id: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let active_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        active_id,
        &format!("registry.example/app@sha256:{}", "a".repeat(64)),
    );
    let active = harness.store.load_v2_release(app_id, active_id).unwrap();
    let candidate_id = Uuid::new_v4();
    let candidate = harness.publish_pending(
        app_id,
        candidate_id,
        &format!("registry.example/app@sha256:{}", "b".repeat(64)),
        Some(active_id),
    );
    let active_container = release_container('a', app_id, &active);
    let active_container_id = active_container.id.clone();
    docker.install(active_container, release_container('b', app_id, &candidate));
    harness.compose_actions.lock().unwrap().clear();

    let response = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/deployments"),
            Some("candidate-post-stop-guard"),
            &json!({
                "expected_draft_revision": revision_id,
                "expected_active_release_id": active_id,
                "expected_pending_release_id": candidate_id,
                "expected_actual_release_id": active_id,
                "expected_actual_container_id": active_container_id,
                "acknowledge_non_rollbackable_data": true
            }),
        )
        .await;
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{response}");
    let deployment_id: Uuid = serde_json::from_str::<Value>(&response).unwrap()["deployment_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let terminal = wait_for_deployment(&harness, deployment_id).await;
    assert!(matches!(
        terminal.status,
        solodock::deploy::DeploymentStatus::Failed
            | solodock::deploy::DeploymentStatus::Interrupted
            | solodock::deploy::DeploymentStatus::NeedsAttention
    ));
    assert_eq!(
        harness.compose_actions.lock().unwrap().as_slice(),
        &[ComposeAction::Stop]
    );
}

#[tokio::test]
async fn deployment_engine_compensation_guard_blocks_predecessor_restore_after_inode_swap() {
    let bind_root = tempfile::tempdir().unwrap();
    let source = bind_root.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = Arc::new(StopScenario {
        source: source.clone(),
        replacement: None,
        mutate_on_stop: 2,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        phase: std::sync::atomic::AtomicUsize::new(0),
    });
    let docker = Arc::new(ScenarioDocker {
        scenario: Some(scenario.clone()),
        ..ScenarioDocker::default()
    });
    let harness =
        Harness::new_with_components(docker.clone(), Some(scenario), Some(Arc::new(NoopPuller)))
            .await;
    harness
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);

    let mut input = draft("compensation-guard-secret");
    input["binds"] = json!([{
        "source": source,
        "target_path": "/data",
        "readonly": true,
        "acknowledge_non_rollbackable": false
    }]);
    input["health"] = json!({"policy":"healthy"});
    let (status, created) = body(
        harness
            .create(Some("compensation-guard-create"), &input)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app_id: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision_id: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let active_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        active_id,
        &format!("registry.example/app@sha256:{}", "c".repeat(64)),
    );
    let active = harness.store.load_v2_release(app_id, active_id).unwrap();
    let candidate_id = Uuid::new_v4();
    let candidate = harness.publish_pending(
        app_id,
        candidate_id,
        &format!("registry.example/app@sha256:{}", "d".repeat(64)),
        Some(active_id),
    );
    let active_container = release_container('c', app_id, &active);
    let active_container_id = active_container.id.clone();
    let mut candidate_container = release_container('d', app_id, &candidate);
    candidate_container.health = HealthStatus::Unhealthy;
    docker.install(active_container, candidate_container);
    harness.compose_actions.lock().unwrap().clear();

    let response = harness
        .mutate(
            "POST",
            &format!("/api/v1/apps/{app_id}/deployments"),
            Some("compensation-post-stop-guard"),
            &json!({
                "expected_draft_revision": revision_id,
                "expected_active_release_id": active_id,
                "expected_pending_release_id": candidate_id,
                "expected_actual_release_id": active_id,
                "expected_actual_container_id": active_container_id,
                "acknowledge_non_rollbackable_data": true
            }),
        )
        .await;
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{response}");
    let deployment_id: Uuid = serde_json::from_str::<Value>(&response).unwrap()["deployment_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let terminal = wait_for_deployment(&harness, deployment_id).await;
    assert_eq!(
        terminal.status,
        solodock::deploy::DeploymentStatus::NeedsAttention
    );
    assert_eq!(
        harness.compose_actions.lock().unwrap().as_slice(),
        &[
            ComposeAction::Stop,
            ComposeAction::DeployCandidate,
            ComposeAction::Stop
        ]
    );
}

#[tokio::test]
async fn lifecycle_restart_guard_blocks_start_after_symlink_swap() {
    let bind_root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = bind_root.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = Arc::new(StopScenario {
        source: source.clone(),
        replacement: Some(outside.path().to_path_buf()),
        mutate_on_stop: 1,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        phase: std::sync::atomic::AtomicUsize::new(0),
    });
    let docker = Arc::new(ScenarioDocker {
        scenario: Some(scenario.clone()),
        ..ScenarioDocker::default()
    });
    let harness = Harness::new_with_components(docker.clone(), Some(scenario), None).await;
    harness
        .store
        .replace_allowed_bind_roots(vec![bind_root.path().to_path_buf()]);
    let mut input = draft("restart-guard-secret");
    input["binds"] = json!([{
        "source": source,
        "target_path": "/data",
        "readonly": true,
        "acknowledge_non_rollbackable": false
    }]);
    let (status, created) = body(harness.create(Some("restart-guard-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app_id: Uuid = serde_json::from_str::<Value>(&created).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let release_id = Uuid::new_v4();
    harness.publish_active(
        app_id,
        release_id,
        &format!("registry.example/app@sha256:{}", "e".repeat(64)),
    );
    let release = harness.store.load_v2_release(app_id, release_id).unwrap();
    let container = release_container('e', app_id, &release);
    docker.install(container.clone(), container);
    harness.compose_actions.lock().unwrap().clear();

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/apps/{app_id}/actions/restart"))
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("x-csrf-token", &harness.csrf)
                .header("idempotency-key", "restart-post-stop-guard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, response) = body(response).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["code"],
        "BIND_CHANGED"
    );
    assert_eq!(
        harness.compose_actions.lock().unwrap().as_slice(),
        &[ComposeAction::Stop]
    );
}

#[tokio::test]
async fn signed_webhook_is_host_isolated_write_only_and_durably_coalesced() {
    fn signed_request(
        app_id: Uuid,
        host: &'static str,
        raw: &'static [u8],
        timestamp: i64,
        nonce_byte: u8,
        secret: &[u8],
    ) -> Request<Body> {
        let nonce = URL_SAFE_NO_PAD.encode([nonce_byte; 16]);
        let input = solodock::webhook::protocol::signing_input(app_id, raw, timestamp, &nonce);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(input.as_bytes());
        Request::builder()
            .method("POST")
            .uri(format!("/hooks/v1/apps/{app_id}/registry"))
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-solodock-timestamp", timestamp.to_string())
            .header("x-solodock-nonce", nonce)
            .header(
                "x-solodock-signature",
                format!("v1={:x}", mac.finalize().into_bytes()),
            )
            .body(Body::from(raw))
            .unwrap()
    }

    let harness = Harness::new().await;
    let mut candidate = draft("application-secret");
    candidate["auto_deploy_enabled"] = json!(true);
    candidate["auto_deploy_acknowledged"] = json!(true);
    let (status, created) =
        body(harness.create(Some("m6-create-hook-app"), &candidate).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app_id: Uuid = serde_json::from_str::<Value>(&created).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let secret_bytes = *b"M6_SECRET_CANARY_32_BYTES_VALUE!";
    let secret = URL_SAFE_NO_PAD.encode(secret_bytes);
    let configured = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/webhook"),
            Some("m6-webhook-configure"),
            &json!({"expected_metadata_revision":null,"secret":secret}),
        )
        .await;
    let (status, configured_body) = body(configured).await;
    assert_eq!(status, StatusCode::OK, "{configured_body}");
    assert!(!configured_body.contains(&secret));
    let configured: Value = serde_json::from_str(&configured_body).unwrap();

    let raw = br#"{"event":"registry.push"}"#;
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let hidden = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "solodock.example.com",
            raw,
            timestamp,
            9,
            &secret_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert_eq!(hidden.headers()[header::CACHE_CONTROL], "no-store");
    let accepted = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            raw,
            timestamp,
            9,
            &secret_bytes,
        ))
        .await
        .unwrap();
    let (status, accepted_body) = body(accepted).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted_body}");
    assert!(!accepted_body.contains(&secret));
    let replay = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            raw,
            timestamp,
            9,
            &secret_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    let wrong_schema = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            br#"{"event":"untrusted.push"}"#,
            timestamp,
            10,
            &secret_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(wrong_schema.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let hook_cannot_serve_ui = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/healthz")
                .header(header::HOST, "hooks.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hook_cannot_serve_ui.status(), StatusCode::NOT_FOUND);

    let rotated_bytes = *b"M6_ROTATED_CANARY_32_BYTES_VALUE";
    let rotated = URL_SAFE_NO_PAD.encode(rotated_bytes);
    let response = harness
        .mutate(
            "PUT",
            &format!("/api/v1/apps/{app_id}/webhook"),
            Some("m6-webhook-rotate"),
            &json!({
                "expected_metadata_revision": configured["metadata_revision"],
                "secret": rotated,
            }),
        )
        .await;
    let (status, rotated_body) = body(response).await;
    assert_eq!(status, StatusCode::OK, "{rotated_body}");
    assert!(!rotated_body.contains(&rotated));
    let rotated_status: Value = serde_json::from_str(&rotated_body).unwrap();
    let old_secret = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            raw,
            timestamp,
            11,
            &secret_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(old_secret.status(), StatusCode::UNAUTHORIZED);
    let new_secret = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            raw,
            timestamp,
            12,
            &rotated_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(new_secret.status(), StatusCode::ACCEPTED);
    let revoked = harness
        .mutate(
            "DELETE",
            &format!("/api/v1/apps/{app_id}/webhook"),
            Some("m6-webhook-revoke"),
            &json!({"expected_metadata_revision":rotated_status["metadata_revision"]}),
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::OK);
    let after_revoke = harness
        .app
        .clone()
        .oneshot(signed_request(
            app_id,
            "hooks.example.com",
            raw,
            timestamp,
            13,
            &rotated_bytes,
        ))
        .await
        .unwrap();
    assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);
    let poll = harness
        .state
        .m4
        .as_ref()
        .unwrap()
        .poller
        .store
        .get(app_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(poll.webhook_sequence, 2);
    assert_eq!(poll.webhook_processed_sequence, 0);
}

#[tokio::test]
async fn webhook_final_secret_check_serializes_with_rotate_revoke_and_delete() {
    fn signed_request(app_id: Uuid, nonce_byte: u8, secret: &[u8]) -> Request<Body> {
        let raw = br#"{"event":"registry.push"}"#;
        let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
        let nonce = URL_SAFE_NO_PAD.encode([nonce_byte; 16]);
        let input = solodock::webhook::protocol::signing_input(app_id, raw, timestamp, &nonce);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(input.as_bytes());
        Request::builder()
            .method("POST")
            .uri(format!("/hooks/v1/apps/{app_id}/registry"))
            .header(header::HOST, "hooks.example.com")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-solodock-timestamp", timestamp.to_string())
            .header("x-solodock-nonce", nonce)
            .header(
                "x-solodock-signature",
                format!("v1={:x}", mac.finalize().into_bytes()),
            )
            .body(Body::from(raw.as_slice()))
            .unwrap()
    }

    let harness = Harness::new().await;
    let mut candidate = draft("application-secret");
    candidate["auto_deploy_enabled"] = json!(true);
    candidate["auto_deploy_acknowledged"] = json!(true);
    let (_, created) = body(
        harness
            .create(Some("m6-create-hook-order-app"), &candidate)
            .await,
    )
    .await;
    let app_id: Uuid = serde_json::from_str::<Value>(&created).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let webhooks = harness.state.webhooks.as_ref().unwrap();
    let first_raw = *b"M6_ORDER_FIRST_SECRET_32_BYTES!!";
    let first = SecretValue::new(URL_SAFE_NO_PAD.encode(first_raw));
    let first_metadata = webhooks
        .store
        .configure(app_id, None, Uuid::new_v4(), &first)
        .unwrap();

    let catalog_guard = harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .catalog_lock()
        .await;
    let old_request = tokio::spawn(
        harness
            .app
            .clone()
            .oneshot(signed_request(app_id, 31, &first_raw)),
    );
    tokio::task::yield_now().await;
    let second_raw = *b"M6_ORDER_SECOND_SECRET_32_BYTE!!";
    let second = SecretValue::new(URL_SAFE_NO_PAD.encode(second_raw));
    let second_metadata = webhooks
        .store
        .configure(
            app_id,
            Some(first_metadata.metadata_revision),
            Uuid::new_v4(),
            &second,
        )
        .unwrap();
    drop(catalog_guard);
    assert_eq!(
        old_request.await.unwrap().unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let catalog_guard = harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .catalog_lock()
        .await;
    let revoked_request = tokio::spawn(harness.app.clone().oneshot(signed_request(
        app_id,
        32,
        &second_raw,
    )));
    tokio::task::yield_now().await;
    webhooks
        .store
        .revoke(app_id, second_metadata.metadata_revision, Uuid::new_v4())
        .unwrap();
    drop(catalog_guard);
    assert_eq!(
        revoked_request.await.unwrap().unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let third_raw = *b"M6_ORDER_THIRD_SECRET_32_BYTES!!";
    let third = SecretValue::new(URL_SAFE_NO_PAD.encode(third_raw));
    webhooks
        .store
        .configure(app_id, None, Uuid::new_v4(), &third)
        .unwrap();
    let catalog_guard = harness
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .catalog_lock()
        .await;
    let deleted_request = tokio::spawn(
        harness
            .app
            .clone()
            .oneshot(signed_request(app_id, 33, &third_raw)),
    );
    tokio::task::yield_now().await;
    harness.store.tombstone(app_id, Uuid::new_v4()).unwrap();
    drop(catalog_guard);
    assert_eq!(
        deleted_request.await.unwrap().unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(webhooks.poll_states.get(app_id).await.unwrap().is_none());
}

#[tokio::test]
async fn global_timezone_is_revisioned_idempotent_and_csrf_protected() {
    let harness = Harness::new().await;
    let initial = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .header(header::HOST, "solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, initial) = body(initial).await;
    assert_eq!(status, StatusCode::OK);
    let initial: Value = serde_json::from_str(&initial).unwrap();
    assert_eq!(initial["display_timezone"], "UTC");
    assert_eq!(initial["supported_timezones"][0], "UTC");
    assert_eq!(
        initial["configuration_limits"]["health"]["http_retries"]["max"],
        10
    );
    assert_eq!(
        initial["configuration_limits"]["health"]["http_timeout_seconds"]["max"],
        60
    );
    let request = json!({
        "expected_revision": initial["revision"],
        "display_timezone": "Asia/Shanghai",
        "allowed_bind_roots": [],
    });

    let missing_csrf = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/settings")
                .header(header::HOST, "solodock.example.com")
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &harness.cookie)
                .header("idempotency-key", "settings-missing-csrf")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

    let updated = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-update-shanghai"),
            &request,
        )
        .await;
    let (status, updated) = body(updated).await;
    assert_eq!(status, StatusCode::OK);
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(updated["display_timezone"], "Asia/Shanghai");
    assert_ne!(updated["revision"], initial["revision"]);
    let replayed = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-update-shanghai"),
            &request,
        )
        .await;
    let (status, replayed) = body(replayed).await;
    assert_eq!(status, StatusCode::OK);
    let replayed: Value = serde_json::from_str(&replayed).unwrap();
    assert_eq!(replayed["idempotency_replayed"], true);
    let audit_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = '/api/v1/settings'")
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    assert_eq!(audit_count, 2);

    let stale = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-stale-revision"),
            &request,
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let invalid = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-invalid-timezone"),
            &json!({
                "expected_revision": updated["revision"],
                "display_timezone": "Mars/Olympus",
                "allowed_bind_roots": [],
            }),
        )
        .await;
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn settings_replay_precedes_mutable_bind_root_validation() {
    let bind_root = tempfile::tempdir().unwrap();
    let bind_path = bind_root.path().to_path_buf();
    let harness = Harness::new_with_docker(Arc::new(SettingsDocker {
        docker_root_directory: Some("/var/lib/docker".into()),
    }))
    .await;
    let initial = solodock::settings::SettingsStore::new(harness.database.clone())
        .load()
        .await
        .unwrap();
    let request = json!({
        "expected_revision": initial.revision,
        "display_timezone": "UTC",
        "allowed_bind_roots": [bind_path],
    });
    let first = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-bind-replay"),
            &request,
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    fs::remove_dir_all(&bind_path).unwrap();

    let replay = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-bind-replay"),
            &request,
        )
        .await;
    let (status, replay) = body(replay).await;
    assert_eq!(status, StatusCode::OK);
    let replay: Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(replay["idempotency_replayed"], true);
    assert_eq!(replay["allowed_bind_roots"], json!([bind_path]));
}

#[tokio::test]
async fn settings_reject_bind_roots_overlapping_the_live_docker_data_root() {
    let external = tempfile::tempdir().unwrap();
    let docker_root = external.path().join("docker-data");
    fs::create_dir(&docker_root).unwrap();
    let harness = Harness::new_with_docker(Arc::new(SettingsDocker {
        docker_root_directory: Some(docker_root.display().to_string()),
    }))
    .await;
    let initial = solodock::settings::SettingsStore::new(harness.database.clone())
        .load()
        .await
        .unwrap();
    let rejected = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-docker-root-overlap"),
            &json!({
                "expected_revision": initial.revision,
                "display_timezone": "UTC",
                "allowed_bind_roots": [external.path()],
            }),
        )
        .await;
    let (status, rejected) = body(rejected).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        serde_json::from_str::<Value>(&rejected).unwrap()["code"],
        "BIND_ROOT_INVALID"
    );
    assert!(
        solodock::settings::SettingsStore::new(harness.database.clone())
            .load()
            .await
            .unwrap()
            .allowed_bind_roots
            .is_empty()
    );
}

#[tokio::test]
async fn settings_fail_closed_when_docker_root_cannot_be_observed() {
    let bind_root = tempfile::tempdir().unwrap();
    let harness =
        Harness::new_with_docker(Arc::new(solodock::docker::models::UnavailableDocker)).await;
    let initial = solodock::settings::SettingsStore::new(harness.database.clone())
        .load()
        .await
        .unwrap();
    let rejected = harness
        .mutate(
            "PUT",
            "/api/v1/settings",
            Some("settings-docker-unavailable"),
            &json!({
                "expected_revision": initial.revision,
                "display_timezone": "UTC",
                "allowed_bind_roots": [bind_root.path()],
            }),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        solodock::settings::SettingsStore::new(harness.database.clone())
            .load()
            .await
            .unwrap()
            .allowed_bind_roots
            .is_empty()
    );
}
