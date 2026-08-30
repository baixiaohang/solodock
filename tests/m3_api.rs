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
    app_store::{AppStore, config_revision},
    auth::AuthService,
    compose::{
        ComposeAction, ComposeCapability, ComposeInput, ComposeOutput, ComposeRunner, RunContext,
        generate,
    },
    db::Database,
    deploy::{
        DeploymentEngine, DeploymentLedger, DeploymentScheduler, FixedImagePuller, HealthVerifier,
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
}

struct MissingDocker;

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
    ) -> Result<Option<solodock::docker::models::DockerResource>, DockerError> {
        Ok(None)
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
    ) -> Result<Option<solodock::docker::models::DockerResource>, DockerError> {
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
            (
                PROJECT_LABEL.into(),
                format!("solodock-{}", app_id.simple()),
            ),
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
}

impl Harness {
    async fn new() -> Self {
        Self::new_with_docker(Arc::new(MissingDocker)).await
    }

    async fn new_with_docker(docker: Arc<dyn DockerReadApi>) -> Self {
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
        let auth = AuthService::new(database.clone(), runtime_directory.join("bootstrap.token"));
        assert!(auth.prepare_bootstrap().await.unwrap());
        let token = fs::read_to_string(runtime_directory.join("bootstrap.token")).unwrap();
        auth.bootstrap(token, "correct horse battery".into(), Uuid::new_v4())
            .await
            .unwrap();
        let login = auth
            .login("admin", "correct horse battery".into(), Uuid::new_v4())
            .await
            .unwrap();
        let cookie = format!(
            "__Host-solodock_session={}; __Host-solodock_csrf={}",
            login.session_token.expose(),
            login.csrf_token.expose()
        );
        let csrf = login.csrf_token.expose().to_owned();
        let idempotency =
            IdempotencyService::initialize(database.clone(), &state_directory).unwrap();
        let store =
            AppStore::initialize_verified(apps.clone(), idempotency.integrity_key()).unwrap();
        let fake_compose = Arc::new(FakeCompose::default());
        let validated_yaml = fake_compose.validated_yaml.clone();
        let compose_actions = fake_compose.actions.clone();
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
        let puller = FixedImagePuller::new(
            state_directory.clone(),
            runtime_directory.join("pull"),
            docker.clone(),
            state.shutdown.clone(),
            state.stream_tasks.clone(),
        )
        .unwrap();
        let engine = DeploymentEngine {
            store: store.clone(),
            credentials: credentials.clone(),
            resolver: RegistryResolver::production().unwrap(),
            ledger: ledger.clone(),
            puller: Arc::new(puller),
            compose: state.m3.as_ref().unwrap().compose.clone(),
            docker,
            health: HealthVerifier::new(state.observer.api(), state.shutdown.clone()),
            shutdown: state.shutdown.clone(),
            tasks: state.stream_tasks.clone(),
            #[cfg(feature = "docker-e2e")]
            test_effect_gate: None,
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
            authority: "hooks.example.com".into(),
            store: WebhookStore::new(store.clone(), idempotency.integrity_key()),
            poll_states: poller.store.clone(),
            database: database.clone(),
            notify: poller.notify.clone(),
            limiter: WebhookRateLimiter::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(16)),
        }));
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
        }
    }

    async fn create(&self, key: Option<&str>, body: &Value) -> axum::response::Response {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/apps")
            .header(header::ORIGIN, "https://solodock.example.com")
            .header(header::COOKIE, &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
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
        let loaded = config_revision::load_verified(
            &self.store.app_directory(app_id),
            metadata.draft_revision,
            &self.idempotency.integrity_key(),
        )
        .unwrap();
        let input = loaded.input(
            metadata.slug.clone(),
            metadata.display_name.clone(),
            metadata.discovery_image_ref.clone(),
            metadata.credential_ref,
            metadata.auto_deploy_enabled,
            metadata.poll_interval_seconds,
        );
        let normalized = solodock::domain::normalize_draft(
            input,
            &loaded.secrets,
            &self.idempotency.integrity_key(),
            &[],
        )
        .unwrap();
        let revision_directory = self
            .store
            .app_directory(app_id)
            .join("config-revisions")
            .join(metadata.draft_revision.to_string());
        let (yaml, _) = generate(
            ComposeInput {
                app_id,
                release_id,
                image_ref: image,
                revision_directory: &revision_directory,
                draft: &normalized,
            },
            true,
        )
        .unwrap();
        let release = format!(
            "schema_version=1\nid='{release_id}'\napp_id='{app_id}'\nrunnable_image_ref='{image}'\nconfig_revision='{}'\nconfig_sha256='{}'\ncreated_at='2026-08-29T00:00:00Z'\n",
            metadata.draft_revision, metadata.draft_config_sha256
        );
        solodock::app_store::atomic::AtomicWriter::publish_release(
            &self.store.app_directory(app_id).join("releases"),
            release_id,
            release.as_bytes(),
            yaml.as_bytes(),
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
        "health": {"policy":"running","stable_window_seconds":15}
    })
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

#[tokio::test]
async fn create_requires_idempotency_and_replays_without_secret_disclosure() {
    let harness = Harness::new().await;
    let canary = "M3_SECRET_CANARY";
    serde_json::from_value::<solodock::domain::DraftInput>(draft(canary)).unwrap();
    let missing = harness.create(None, &draft(canary)).await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");

    let key = "m3-create-example-0001";
    let (status, first) = body(harness.create(Some(key), &draft(canary)).await).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!first.contains(canary));
    let first_json: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first_json["app"]["deployment_status"], "DEPLOY_REQUIRED");
    assert_eq!(first_json["idempotency_replayed"], false);

    let (status, replay) = body(harness.create(Some(key), &draft(canary)).await).await;
    assert_eq!(status, StatusCode::CREATED);
    let replay_json: Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(replay_json["app"]["id"], first_json["app"]["id"]);
    assert_eq!(replay_json["idempotency_replayed"], true);
    assert!(!replay.contains(canary));
    assert_eq!(harness.database.audit_count().await.unwrap(), 4); // bootstrap, login, attempt, success

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
    let request = draft("reconcile-secret");
    let (status, first) = body(harness.create(Some(key), &request).await).await;
    assert_eq!(status, StatusCode::CREATED);
    let first: Value = serde_json::from_str(&first).unwrap();
    sqlx::query("UPDATE idempotency_records SET status='interrupted',response_status=NULL,response_body=NULL WHERE route='/api/v1/apps'")
        .execute(harness.database.pool())
        .await
        .unwrap();

    let (status, resumed) = body(harness.create(Some(key), &request).await).await;
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
async fn interrupted_update_resumes_after_startup_only_old_revision_cleanup() {
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
    let mut updated = draft("new-secret");
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
        !old_path.exists(),
        "startup recovery collects old revisions"
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
        "draft":draft_with_owned_volume("secret-b", "draft-data")
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
        item["name"].as_str().unwrap().ends_with("-active-data")
            && item["configured_in"] == "active"
            && item["exists"] == false
    }));
    assert!(volumes.iter().any(|item| {
        item["name"].as_str().unwrap().ends_with("-draft-data")
            && item["configured_in"] == "draft"
            && item["exists"] == false
    }));

    let changed = json!({
        "expected_revision":draft_revision,
        "draft":draft_with_owned_volume("secret-c", "changed-data")
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
        "draft":external_only_draft("secret-b", "database")
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
    let mut candidate = draft("validate-secret");
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
    assert_eq!(returned, captured);
    assert!(
        response["compose_yaml"]
            .as_str()
            .unwrap()
            .contains("settings")
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
