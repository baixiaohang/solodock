#![cfg(feature = "docker-e2e")]

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bollard::{
    API_DEFAULT_VERSION, Docker,
    models::{
        ContainerCreateBody, EndpointSettings, HostConfig, Mount, MountType, NetworkConnectRequest,
        NetworkCreateRequest, NetworkingConfig, VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use solodock::docker::{
    client::BollardReadClient,
    models::{DockerReadApi, LogRequest, LogStreamKind},
    ownership::*,
};
use solodock::{
    AppState,
    api::{deployments::M4Services, mutations::M3Services},
    app_store::AppStore,
    auth::AuthService,
    compose::{
        ComposeAction, ComposeCapability, ComposeError, ComposeOutput, ComposeRunner,
        FixedComposeRunner, RunContext,
    },
    db::Database,
    deploy::{
        DeploymentEngine, DeploymentLedger, DeploymentScheduler, FixedImagePuller, HealthVerifier,
        TestEffectAction, TestEffectGate, TestPullAction, TestPullGate,
    },
    docker::{
        AppCatalog, DockerObserver,
        events::DockerEventHub,
        logs::{EmptySecretProvider, SecretRedactor},
        probe::DockerSupervisor,
        stats::StatsHub,
    },
    mutation::{AppMutationCoordinator, IdempotencyService},
    registry::{CredentialStore, PollCoordinator, PollStateStore, RegistryResolver},
    router,
    webhook::{WebhookRateLimiter, WebhookServices, WebhookStore},
};
use tempfile::TempDir;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower::ServiceExt;
use uuid::Uuid;

struct Outcome {
    listed: bool,
    inspected_run_token: Option<String>,
    log_stream: LogStreamKind,
    log_contains_canary: bool,
    memory_observed: bool,
    event_container_id: String,
    event_run_token: Option<String>,
}

#[derive(Clone, Copy)]
struct ProcessMetrics {
    rss_kib: u64,
    fd_count: u64,
    task_count: u64,
}

struct ProcessPeakSampler {
    peak_rss_kib: Arc<AtomicU64>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl ProcessPeakSampler {
    fn start(pid: u32) -> Self {
        let peak_rss_kib = Arc::new(AtomicU64::new(0));
        let stop = CancellationToken::new();
        let peak = peak_rss_kib.clone();
        let cancellation = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                if let Ok(metrics) = process_metrics(pid) {
                    peak.fetch_max(metrics.rss_kib, Ordering::AcqRel);
                }
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        });
        Self {
            peak_rss_kib,
            stop,
            task,
        }
    }

    async fn finish(self) -> u64 {
        self.stop.cancel();
        let _ = self.task.await;
        self.peak_rss_kib.load(Ordering::Acquire)
    }
}

fn process_metrics(pid: u32) -> std::io::Result<ProcessMetrics> {
    let process = format!("/proc/{pid}");
    let status = std::fs::read_to_string(format!("{process}/status"))?;
    let rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| std::io::Error::other("VmRSS unavailable"))?;
    let count = |path: &str| -> std::io::Result<u64> {
        Ok(u64::try_from(std::fs::read_dir(path)?.count()).unwrap_or(u64::MAX))
    };
    Ok(ProcessMetrics {
        rss_kib,
        fd_count: count(&format!("{process}/fd"))?,
        task_count: count(&format!("{process}/task"))?,
    })
}

fn tree_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    for entry in std::fs::read_dir(path)? {
        total = total.saturating_add(tree_bytes(&entry?.path())?);
    }
    Ok(total)
}

fn resource_stream_hold_seconds() -> u64 {
    match std::env::var("SOLODOCK_E2E_STREAM_HOLD_SECONDS") {
        Ok(raw) => {
            let seconds = raw
                .parse::<u64>()
                .expect("SOLODOCK_E2E_STREAM_HOLD_SECONDS must be an integer");
            assert!(
                (1..=60).contains(&seconds),
                "SOLODOCK_E2E_STREAM_HOLD_SECONDS must be between 1 and 60"
            );
            seconds
        }
        Err(std::env::VarError::NotPresent) => 60,
        Err(error) => panic!("invalid SOLODOCK_E2E_STREAM_HOLD_SECONDS: {error}"),
    }
}

struct MutationHarness {
    _root: TempDir,
    app: axum::Router,
    state: AppState,
    store: AppStore,
    catalog: AppCatalog,
    idempotency: IdempotencyService,
    cookie: String,
    csrf: String,
}

#[derive(Clone)]
struct RecordingComposeRunner {
    inner: FixedComposeRunner,
    calls: Arc<AtomicU64>,
}

impl RecordingComposeRunner {
    fn new(inner: FixedComposeRunner) -> Self {
        Self {
            inner,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst);
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ComposeRunner for RecordingComposeRunner {
    async fn run(
        &self,
        action: ComposeAction,
        context: RunContext,
    ) -> Result<ComposeOutput, ComposeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.run(action, context).await
    }
}

impl MutationHarness {
    async fn new(endpoint: String, bind_root: std::path::PathBuf) -> Self {
        Self::new_with_components(endpoint, bind_root, None, None, None).await
    }

    async fn new_with_test_gates(
        endpoint: String,
        bind_root: std::path::PathBuf,
        pull_gate: Option<TestPullGate>,
        effect_gate: Option<TestEffectGate>,
    ) -> Self {
        Self::new_with_components(endpoint, bind_root, pull_gate, effect_gate, None).await
    }

    async fn new_with_compose(
        endpoint: String,
        bind_root: std::path::PathBuf,
        compose: Arc<dyn ComposeRunner>,
    ) -> Self {
        Self::new_with_components(endpoint, bind_root, None, None, Some(compose)).await
    }

    async fn new_with_components(
        endpoint: String,
        bind_root: std::path::PathBuf,
        pull_gate: Option<TestPullGate>,
        effect_gate: Option<TestEffectGate>,
        compose: Option<Arc<dyn ComposeRunner>>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let state_directory = root.path().join("state");
        let runtime_directory = root.path().join("runtime");
        let apps = state_directory.join("apps");
        for path in [&state_directory, &runtime_directory, &apps] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let database = Database::open(&state_directory.join("state.sqlite3"))
            .await
            .unwrap();
        let auth = AuthService::new(database.clone(), runtime_directory.join("bootstrap.token"));
        assert!(auth.prepare_bootstrap().await.unwrap());
        let token = std::fs::read_to_string(runtime_directory.join("bootstrap.token")).unwrap();
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
        let store = AppStore::initialize_managed(
            apps,
            idempotency.integrity_key(),
            vec![bind_root.clone()],
        )
        .unwrap();
        let docker_api = Arc::new(BollardReadClient::for_test_http(endpoint.clone()));
        let probe = docker_api.probe().await.unwrap();
        let catalog = AppCatalog::default();
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let redactor = SecretRedactor::new(&EmptySecretProvider);
        let compose = compose.unwrap_or_else(|| {
            Arc::new(FixedComposeRunner::for_test_http(
                shutdown.clone(),
                tasks.clone(),
                redactor.clone(),
                endpoint.clone(),
            ))
        });
        let capability = ComposeCapability::default();
        capability.probe(compose.as_ref()).await;
        let observer = DockerObserver::new(
            docker_api.clone(),
            catalog.clone(),
            DockerSupervisor::from_snapshot(probe),
        );
        let mut state = AppState {
            auth,
            public_origin: "https://solodock.example.com".into(),
            observer,
            events: DockerEventHub::new(),
            stats: StatsHub::new(docker_api.clone(), shutdown.clone(), tasks.clone()),
            stream_gate: solodock::api::streams::StreamGate::default(),
            redactor,
            state_directory: state_directory.clone(),
            shutdown,
            stream_tasks: tasks,
            m3: None,
            m4: None,
            webhooks: None,
        };
        let m3 = Arc::new(M3Services {
            store: store.clone(),
            database: database.clone(),
            allowed_bind_roots: vec![bind_root],
            runtime_directory: runtime_directory.clone(),
            idempotency: idempotency.clone(),
            coordinator: AppMutationCoordinator::new(runtime_directory).unwrap(),
            compose,
            compose_capability: capability,
            projection_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_notify: Arc::new(tokio::sync::Notify::new()),
            publication_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        state.m3 = Some(m3.clone());
        let credentials = CredentialStore::initialize(
            state_directory.join("registry-credentials"),
            idempotency.integrity_key(),
        )
        .unwrap();
        let ledger = DeploymentLedger::new(database.clone());
        let mut puller = FixedImagePuller::new(
            state_directory.clone(),
            state_directory.join("pull-runtime"),
            docker_api.clone(),
            state.shutdown.clone(),
            state.stream_tasks.clone(),
        )
        .unwrap()
        .with_test_host(endpoint)
        .with_test_pressure_root(state_directory.clone());
        if let Some(gate) = pull_gate {
            puller = puller.with_test_gate(gate);
        }
        let engine = DeploymentEngine {
            store: store.clone(),
            credentials: credentials.clone(),
            resolver: RegistryResolver::for_test_http().unwrap(),
            ledger: ledger.clone(),
            puller: Arc::new(puller),
            compose: m3.compose.clone(),
            docker: docker_api.clone(),
            health: HealthVerifier::new(docker_api, state.shutdown.clone()),
            shutdown: state.shutdown.clone(),
            tasks: state.stream_tasks.clone(),
            test_effect_gate: effect_gate,
        };
        let scheduler = DeploymentScheduler::new(engine.clone());
        let poller = PollCoordinator::new(
            PollStateStore::new(database),
            state.shutdown.clone(),
            state.stream_tasks.clone(),
        );
        state.m4 = Some(Arc::new(M4Services {
            credentials,
            ledger,
            engine,
            scheduler,
            poller,
        }));
        let poller = state.m4.as_ref().unwrap().poller.clone();
        state.webhooks = Some(Arc::new(WebhookServices {
            origin: "https://hooks.example.com".into(),
            authority: "hooks.example.com".into(),
            store: WebhookStore::new(store.clone(), idempotency.integrity_key()),
            poll_states: poller.store.clone(),
            database: state.m3.as_ref().unwrap().database.clone(),
            notify: poller.notify.clone(),
            limiter: WebhookRateLimiter::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(16)),
        }));
        let app = router(state.clone());
        Self {
            _root: root,
            app,
            state,
            store,
            catalog,
            idempotency,
            cookie,
            csrf,
        }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        key: Option<&str>,
        body: Option<&Value>,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::ORIGIN, "https://solodock.example.com")
            .header(header::COOKIE, &self.cookie)
            .header("x-csrf-token", &self.csrf);
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = if let Some(value) = body {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        } else {
            Body::empty()
        };
        self.app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn json(response: axum::response::Response) -> Value {
        Self::try_json(response).await.unwrap()
    }

    async fn try_json(response: axum::response::Response) -> Result<Value, String> {
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("read response body: {error}"))?
            .to_bytes();
        serde_json::from_slice(&bytes).map_err(|error| format!("decode response JSON: {error}"))
    }

    fn publish_active(&self, app_id: Uuid, release_id: Uuid, image: &str) {
        self.try_publish_active(app_id, release_id, image).unwrap();
    }

    fn try_publish_active(
        &self,
        app_id: Uuid,
        release_id: Uuid,
        image: &str,
    ) -> Result<(), String> {
        let metadata = self
            .store
            .read_metadata(app_id)
            .map_err(|error| format!("read app metadata: {error:?}"))?;
        let loaded = solodock::app_store::config_revision::load_verified(
            &self.store.app_directory(app_id),
            metadata.draft_revision,
            &self.idempotency.integrity_key(),
        )
        .map_err(|error| format!("load config revision: {error:?}"))?;
        let allowed_bind_roots = &self
            .state
            .m3
            .as_ref()
            .ok_or_else(|| "M3 services missing".to_string())?
            .allowed_bind_roots;
        let draft = solodock::domain::normalize_draft(
            loaded.input(
                metadata.slug,
                metadata.display_name,
                metadata.discovery_image_ref,
                metadata.credential_ref,
                metadata.auto_deploy_enabled,
                metadata.poll_interval_seconds,
            ),
            &loaded.secrets,
            &self.idempotency.fingerprint(b"config"),
            allowed_bind_roots,
        )
        .map_err(|error| format!("normalize active draft: {error:?}"))?;
        let revision_directory = self
            .store
            .app_directory(app_id)
            .join("config-revisions")
            .join(metadata.draft_revision.to_string());
        let (yaml, _) = solodock::compose::generate(
            solodock::compose::ComposeInput {
                app_id,
                release_id,
                image_ref: image,
                revision_directory: &revision_directory,
                draft: &draft,
            },
            true,
        )
        .map_err(|error| format!("generate active compose: {error:?}"))?;
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| format!("format active timestamp: {error:?}"))?;
        let release = format!(
            "schema_version = 1\nid = '{release_id}'\napp_id = '{app_id}'\nrunnable_image_ref = '{image}'\nconfig_revision = '{}'\nconfig_sha256 = '{}'\ncreated_at = '{now}'\n",
            metadata.draft_revision, metadata.draft_config_sha256
        );
        solodock::app_store::atomic::AtomicWriter::publish_release(
            &self.store.app_directory(app_id).join("releases"),
            release_id,
            release.as_bytes(),
            yaml.as_bytes(),
        )
        .map_err(|error| format!("publish active release: {error:?}"))?;
        solodock::app_store::atomic::AtomicWriter::switch_release_link(
            &self.store.app_directory(app_id),
            "active",
            release_id,
        )
        .map_err(|error| format!("switch active release: {error:?}"))?;
        self.catalog.replace(
            &self
                .store
                .scan()
                .map_err(|error| format!("scan active release: {error:?}"))?,
        );
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon"]
async fn observes_owned_container_on_isolated_daemon() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    assert!(endpoint.starts_with("tcp://127.0.0.1:") || endpoint.starts_with("tcp://localhost:"));
    // Fixture setup is not part of the production five-second unary contract;
    // allow a busy shared DinD daemon to create this container while another
    // isolated test is exercising image/Registry I/O.
    let docker = Docker::connect_with_http(&endpoint, 30, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let run_token = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let release_id = Uuid::new_v4();
    let name = format!("solodock-test-{run_token}");
    let labels = HashMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (SCHEMA_LABEL.into(), "1".into()),
        (APP_ID_LABEL.into(), app_id.to_string()),
        (RELEASE_ID_LABEL.into(), release_id.to_string()),
        (
            PROJECT_LABEL.into(),
            format!("solodock-test-{}", app_id.simple()),
        ),
        (SERVICE_LABEL.into(), "app".into()),
        (ONEOFF_LABEL.into(), "False".into()),
        ("com.solodock.test-run".into(), run_token.to_string()),
    ]);
    let created = docker
        .create_container(
            Some(CreateContainerOptionsBuilder::default().name(&name).build()),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo solodock-e2e-log; sleep 20".into(),
                ]),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let container_id = created.id;

    let result: Result<Outcome, String> = async {
        let adapter = BollardReadClient::for_test_http(endpoint);
        let mut events = adapter.events().await.map_err(|error| error.to_string())?;
        let event_task = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
                .await
                .map_err(|_| "event timeout".to_string())?
                .ok_or_else(|| "event stream ended".to_string())?
                .map_err(|error| error.to_string())
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        docker
            .start_container(&container_id, None)
            .await
            .map_err(|_| "failed to start test container".to_string())?;

        let listed = adapter
            .list_managed_containers()
            .await
            .map_err(|error| error.to_string())?;
        let inspected = adapter
            .inspect_container(&container_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut logs = adapter
            .logs(
                &container_id,
                LogRequest {
                    tail: 20,
                    since_seconds: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let log = tokio::time::timeout(std::time::Duration::from_secs(10), logs.next())
            .await
            .map_err(|_| "log timeout".to_string())?
            .ok_or_else(|| "log stream ended".to_string())?
            .map_err(|error| error.to_string())?;
        let mut stats = adapter
            .stats(&container_id)
            .await
            .map_err(|error| error.to_string())?;
        let sample = tokio::time::timeout(std::time::Duration::from_secs(10), stats.next())
            .await
            .map_err(|_| "stats timeout".to_string())?
            .ok_or_else(|| "stats stream ended".to_string())?
            .map_err(|error| error.to_string())?;
        let event = event_task
            .await
            .map_err(|_| "event task failed".to_string())??;
        Ok(Outcome {
            listed: listed.iter().any(|container| container.id == container_id),
            inspected_run_token: inspected.labels.get("com.solodock.test-run").cloned(),
            log_stream: log.stream,
            log_contains_canary: log
                .bytes
                .windows(b"solodock-e2e-log".len())
                .any(|value| value == b"solodock-e2e-log"),
            memory_observed: sample.memory_usage.is_some(),
            event_container_id: event.container_id,
            event_run_token: event.labels.get("com.solodock.test-run").cloned(),
        })
    }
    .await;

    let cleanup_target = docker.inspect_container(&container_id, None).await.unwrap();
    assert_eq!(
        cleanup_target
            .config
            .and_then(|config| config.labels)
            .and_then(|labels| labels.get("com.solodock.test-run").cloned()),
        Some(run_token.to_string())
    );
    docker
        .remove_container(
            &container_id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .unwrap();

    let outcome = result.unwrap();
    assert!(outcome.listed);
    assert_eq!(outcome.inspected_run_token, Some(run_token.to_string()));
    assert_eq!(outcome.log_stream, LogStreamKind::Stdout);
    assert!(outcome.log_contains_canary);
    assert!(outcome.memory_observed);
    assert_eq!(outcome.event_container_id, container_id);
    assert_eq!(outcome.event_run_token, Some(run_token.to_string()));
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon"]
async fn exact_container_removal_preserves_volume_data_and_network() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    assert!(endpoint.starts_with("tcp://127.0.0.1:") || endpoint.starts_with("tcp://localhost:"));
    // Fixture creation is outside the production five-second unary contract;
    // allow concurrent DinD tests enough time to allocate the canary container.
    let docker = Docker::connect_with_http(&endpoint, 30, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let run_token = Uuid::new_v4();
    let volume_name = format!("solodock-e2e-volume-{run_token}");
    let network_name = format!("solodock-e2e-network-{run_token}");
    let labels = HashMap::from([("com.solodock.test-run".into(), run_token.to_string())]);
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(volume_name.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_network(NetworkCreateRequest {
            name: network_name.clone(),
            labels: Some(labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let writer_name = format!("solodock-e2e-writer-{run_token}");

    let writer = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&writer_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf persistent-canary >/data/value".into(),
                ]),
                labels: Some(labels.clone()),
                host_config: Some(HostConfig {
                    mounts: Some(vec![Mount {
                        target: Some("/data".into()),
                        source: Some(volume_name.clone()),
                        typ: Some(MountType::VOLUME),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    docker
        .connect_network(
            &network_name,
            NetworkConnectRequest {
                container: writer.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker.start_container(&writer, None).await.unwrap();
    docker
        .wait_container(
            &writer,
            Some(
                WaitContainerOptionsBuilder::default()
                    .condition("not-running")
                    .build(),
            ),
        )
        .next()
        .await
        .unwrap()
        .unwrap();
    docker
        .remove_container(
            &writer,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .unwrap();

    let result: Result<(), String> = async {
        let volume = docker
            .inspect_volume(&volume_name)
            .await
            .map_err(|error| error.to_string())?;
        if volume.labels.get("com.solodock.test-run") != Some(&run_token.to_string()) {
            return Err("volume ownership changed".into());
        }
        let network = docker
            .inspect_network(&network_name, None)
            .await
            .map_err(|error| error.to_string())?;
        if network
            .labels
            .as_ref()
            .and_then(|value| value.get("com.solodock.test-run"))
            != Some(&run_token.to_string())
        {
            return Err("network ownership changed".into());
        }
        let reader_name = format!("solodock-e2e-reader-{run_token}");
        let reader = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&reader_name)
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some("alpine:3.20".into()),
                    cmd: Some(vec!["cat".into(), "/data/value".into()]),
                    labels: Some(labels.clone()),
                    host_config: Some(HostConfig {
                        mounts: Some(vec![Mount {
                            target: Some("/data".into()),
                            source: Some(volume_name.clone()),
                            typ: Some(MountType::VOLUME),
                            read_only: Some(true),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .id;
        docker
            .start_container(&reader, None)
            .await
            .map_err(|error| error.to_string())?;
        docker
            .wait_container(
                &reader,
                Some(
                    WaitContainerOptionsBuilder::default()
                        .condition("not-running")
                        .build(),
                ),
            )
            .next()
            .await
            .ok_or_else(|| "reader wait ended".to_string())?
            .map_err(|error| error.to_string())?;
        let logs = docker
            .logs(
                &reader,
                Some(
                    bollard::query_parameters::LogsOptionsBuilder::default()
                        .stdout(true)
                        .build(),
                ),
            )
            .collect::<Vec<_>>()
            .await;
        docker
            .remove_container(
                &reader,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await
            .map_err(|error| error.to_string())?;
        let bytes: Vec<u8> = logs
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .flat_map(|output| output.into_bytes())
            .collect();
        if !bytes
            .windows(b"persistent-canary".len())
            .any(|value| value == b"persistent-canary")
        {
            return Err("volume data was not retained".into());
        }
        Ok(())
    }
    .await;

    let network = docker.inspect_network(&network_name, None).await.unwrap();
    assert_eq!(
        network
            .labels
            .as_ref()
            .and_then(|value| value.get("com.solodock.test-run")),
        Some(&run_token.to_string())
    );
    docker.remove_network(&network_name).await.unwrap();
    let volume = docker.inspect_volume(&volume_name).await.unwrap();
    assert_eq!(
        volume.labels.get("com.solodock.test-run"),
        Some(&run_token.to_string())
    );
    docker
        .remove_volume(
            &volume_name,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon and docker compose CLI"]
async fn external_only_apps_keep_stable_aliases_on_one_unmanaged_network() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    assert!(endpoint.starts_with("tcp://127.0.0.1:") || endpoint.starts_with("tcp://localhost:"));
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let run_token = Uuid::new_v4();
    let network_name = format!("solodock-e2e-alias-{run_token}");
    let pinned = docker
        .inspect_image("nginx:1.27-alpine")
        .await
        .unwrap()
        .repo_digests
        .into_iter()
        .flatten()
        .next()
        .expect("isolated daemon must expose a digest-bearing nginx image");
    let cancellation = CancellationToken::new();
    let tasks = TaskTracker::new();
    let recording_runner = RecordingComposeRunner::new(FixedComposeRunner::for_test_http(
        cancellation.clone(),
        tasks.clone(),
        SecretRedactor::new(&EmptySecretProvider),
        endpoint.clone(),
    ));
    let harness = MutationHarness::new_with_compose(
        endpoint.clone(),
        std::env::temp_dir(),
        Arc::new(recording_runner.clone()),
    )
    .await;
    let labels = HashMap::from([("com.solodock.test-run".into(), run_token.to_string())]);
    let mut network_id = None;
    let mut app_ids = Vec::new();
    let mut exact_app_containers = Vec::new();
    let mut exact_test_containers = Vec::new();

    let mut result: Result<(), String> = async {
        let created_network = docker
            .create_network(NetworkCreateRequest {
                name: network_name.clone(),
                labels: Some(labels.clone()),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("create external network: {error}"))?;
        network_id = Some(created_network.id);

        for alias in ["postgres", "sologrove"] {
            let draft = json!({
                "slug": format!("e2e-{alias}-{run_token}"),
                "display_name": alias,
                "discovery_image_ref": "nginx:1.27-alpine",
                "credential_ref": null,
                "auto_deploy_enabled": false,
                "poll_interval_seconds": 300,
                "environment": {"public": [], "secrets": []},
                "files": [], "ports": [], "volumes": [], "binds": [],
                "owned_default_network": false,
                "networks": [{"kind":"external", "name":network_name, "aliases":[alias]}],
                "health": {"policy":"running", "stable_window_seconds":5}
            });
            let create = harness
                .request(
                    "POST",
                    "/api/v1/apps",
                    Some(&format!("e2e-network-create-{alias}")),
                    Some(&draft),
                )
                .await;
            let create_status = create.status();
            let create_body = MutationHarness::try_json(create).await?;
            if create_status != StatusCode::CREATED {
                return Err(format!("create {alias} app failed: {create_body}"));
            }
            let app_id = create_body["app"]["id"]
                .as_str()
                .ok_or_else(|| format!("create {alias} response omitted app ID"))?
                .parse::<Uuid>()
                .map_err(|error| format!("parse {alias} app ID: {error}"))?;
            app_ids.push(app_id);
            harness.try_publish_active(app_id, Uuid::new_v4(), &pinned)?;
            let start = harness
                .request(
                    "POST",
                    &format!("/api/v1/apps/{app_id}/actions/start"),
                    Some(&format!("e2e-network-start-{alias}")),
                    None,
                )
                .await;
            let start_status = start.status();
            let start_body = MutationHarness::try_json(start).await?;
            if start_status != StatusCode::OK {
                return Err(format!("start {alias} app failed: {start_body}"));
            }
            let containers = harness
                .state
                .observer
                .api()
                .list_compose_app_containers(&format!("solodock-{}", app_id.simple()))
                .await
                .map_err(|error| format!("observe {alias} app: {error}"))?;
            if containers.len() != 1 {
                return Err(format!("expected one exact container for {alias}"));
            }
            let container_id = containers[0].id.clone();
            exact_app_containers.push((container_id.clone(), app_id));
            let inspected = docker
                .inspect_container(&container_id, None)
                .await
                .map_err(|error| format!("inspect {alias} app: {error}"))?;
            let attachment = inspected
                .network_settings
                .and_then(|settings| settings.networks)
                .and_then(|networks| networks.get(&network_name).cloned())
                .ok_or_else(|| format!("{alias} missing external attachment"))?;
            let effective = attachment
                .dns_names
                .unwrap_or_default()
                .into_iter()
                .chain(attachment.aliases.unwrap_or_default())
                .collect::<Vec<_>>();
            if !effective.iter().any(|value| value == alias) {
                return Err(format!("{alias} missing from effective DNS names"));
            }
            let owned_name = format!("solodock-{}-default", app_id.simple());
            if docker.inspect_network(&owned_name, None).await.is_ok() {
                return Err(format!("external-only app created {owned_name}"));
            }
        }

        let adapter = BollardReadClient::for_test_http(endpoint.clone());
        let snapshot = adapter
            .inspect_network_snapshot(&network_name)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "external network disappeared".to_string())?;
        if snapshot.members.len() != 2
            || !["postgres", "sologrove"].iter().all(|alias| {
                snapshot
                    .members
                    .iter()
                    .any(|member| member.dns_names.iter().any(|name| name == alias))
            })
        {
            return Err("fresh network snapshot did not preserve both aliases".into());
        }

        recording_runner.reset();
        let first_app_id = app_ids
            .first()
            .copied()
            .ok_or_else(|| "no external-only app was created".to_string())?;
        let allowed = harness
            .request(
                "POST",
                &format!("/api/v1/apps/{first_app_id}/validate"),
                None,
                Some(&json!({})),
            )
            .await;
        let allowed_status = allowed.status();
        let allowed_body = MutationHarness::try_json(allowed).await?;
        if allowed_status != StatusCode::OK || recording_runner.calls() != 1 {
            return Err(format!(
                "exact current full ID was not allowed before runner: {allowed_body}"
            ));
        }

        let collision_name = format!("solodock-e2e-alias-conflict-{run_token}");
        let collision = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&collision_name)
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some("alpine:3.20".into()),
                    cmd: Some(vec!["sleep".into(), "300".into()]),
                    labels: Some(labels.clone()),
                    networking_config: Some(NetworkingConfig {
                        endpoints_config: Some(HashMap::from([(
                            network_name.clone(),
                            EndpointSettings {
                                aliases: Some(vec!["postgres".into()]),
                                ..Default::default()
                            },
                        )])),
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| format!("create alias collision: {error}"))?;
        exact_test_containers.push(collision.id.clone());
        docker
            .start_container(&collision.id, None)
            .await
            .map_err(|error| format!("start alias collision: {error}"))?;
        recording_runner.reset();
        let conflict = harness
            .request(
                "POST",
                &format!("/api/v1/apps/{first_app_id}/validate"),
                None,
                Some(&json!({})),
            )
            .await;
        let conflict_status = conflict.status();
        let conflict_body = MutationHarness::try_json(conflict).await?;
        if conflict_status != StatusCode::CONFLICT
            || conflict_body["code"] != "NETWORK_ALIAS_CONFLICT"
            || recording_runner.calls() != 0
        {
            return Err(format!(
                "unrelated alias owner did not block before runner: {conflict_body}"
            ));
        }

        let missing_draft = json!({
            "slug": format!("e2e-missing-{run_token}"),
            "display_name": "missing network",
            "discovery_image_ref": "alpine:3.20",
            "credential_ref": null,
            "auto_deploy_enabled": false,
            "poll_interval_seconds": 300,
            "environment": {"public": [], "secrets": []},
            "files": [], "ports": [], "volumes": [], "binds": [],
            "owned_default_network": false,
            "networks": [{
                "kind":"external",
                "name":format!("solodock-e2e-missing-{run_token}"),
                "aliases":[]
            }],
            "health": {"policy":"running", "stable_window_seconds":5}
        });
        let missing_create = harness
            .request(
                "POST",
                "/api/v1/apps",
                Some("e2e-network-create-missing"),
                Some(&missing_draft),
            )
            .await;
        let missing_create_status = missing_create.status();
        let missing_create_body = MutationHarness::try_json(missing_create).await?;
        if missing_create_status != StatusCode::CREATED {
            return Err(format!(
                "create missing-network fixture failed: {missing_create_body}"
            ));
        }
        let missing_app_id = missing_create_body["app"]["id"]
            .as_str()
            .ok_or_else(|| "missing-network fixture omitted app ID".to_string())?
            .parse::<Uuid>()
            .map_err(|error| format!("parse missing-network app ID: {error}"))?;
        app_ids.push(missing_app_id);
        recording_runner.reset();
        let missing = harness
            .request(
                "POST",
                &format!("/api/v1/apps/{missing_app_id}/validate"),
                None,
                Some(&json!({})),
            )
            .await;
        let missing_status = missing.status();
        let missing_body = MutationHarness::try_json(missing).await?;
        if missing_status != StatusCode::CONFLICT
            || missing_body["code"] != "EXTERNAL_NETWORK_NOT_FOUND"
            || recording_runner.calls() != 0
        {
            return Err(format!(
                "missing external network did not fail before runner: {missing_body}"
            ));
        }
        Ok(())
    }
    .await;

    let mut cleanup_errors = Vec::new();
    let mut app_cleanup_targets = exact_app_containers.clone();
    for app_id in &app_ids {
        match docker
            .list_containers(Some(
                bollard::query_parameters::ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&HashMap::from([(
                        "label".to_string(),
                        vec![format!("{APP_ID_LABEL}={app_id}")],
                    )]))
                    .build(),
            ))
            .await
        {
            Ok(leftovers) => {
                for leftover in leftovers {
                    if let Some(container_id) = leftover.id
                        && !app_cleanup_targets
                            .iter()
                            .any(|(existing, _)| existing == &container_id)
                    {
                        app_cleanup_targets.push((container_id, *app_id));
                    }
                }
            }
            Err(error) => cleanup_errors.push(format!("list app cleanup targets: {error}")),
        }
    }
    for (container_id, app_id) in &app_cleanup_targets {
        match docker.inspect_container(container_id, None).await {
            Ok(inspected)
                if inspected
                    .config
                    .as_ref()
                    .and_then(|config| config.labels.as_ref())
                    .and_then(|labels| labels.get(APP_ID_LABEL))
                    == Some(&app_id.to_string()) =>
            {
                if let Err(error) = docker
                    .remove_container(
                        container_id,
                        Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                    )
                    .await
                {
                    cleanup_errors.push(format!("remove exact app container: {error}"));
                }
            }
            Ok(_) => cleanup_errors.push("app cleanup identity changed".into()),
            Err(error) => cleanup_errors.push(format!("inspect exact app container: {error}")),
        }
    }
    for container_id in &exact_test_containers {
        match docker.inspect_container(container_id, None).await {
            Ok(inspected)
                if inspected
                    .config
                    .as_ref()
                    .and_then(|config| config.labels.as_ref())
                    .and_then(|labels| labels.get("com.solodock.test-run"))
                    == Some(&run_token.to_string()) =>
            {
                if let Err(error) = docker
                    .remove_container(
                        container_id,
                        Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                    )
                    .await
                {
                    cleanup_errors.push(format!("remove exact conflict container: {error}"));
                }
            }
            Ok(_) => cleanup_errors.push("conflict cleanup identity changed".into()),
            Err(error) => cleanup_errors.push(format!("inspect conflict container: {error}")),
        }
    }
    if let Some(expected_network_id) = network_id.as_deref() {
        match docker.inspect_network(&network_name, None).await {
            Ok(network)
                if network.id.as_deref() == Some(expected_network_id)
                    && network
                        .labels
                        .as_ref()
                        .and_then(|value| value.get("com.solodock.test-run"))
                        == Some(&run_token.to_string()) =>
            {
                if let Err(error) = docker.remove_network(&network_name).await {
                    cleanup_errors.push(format!("remove exact test network: {error}"));
                }
            }
            Ok(_) => cleanup_errors.push("test network cleanup identity changed".into()),
            Err(error) => cleanup_errors.push(format!("inspect exact test network: {error}")),
        }
    }
    if result.is_ok() && !cleanup_errors.is_empty() {
        result = Err(cleanup_errors.join("; "));
    }
    harness.state.shutdown.cancel();
    harness.state.stream_tasks.close();
    harness.state.stream_tasks.wait().await;
    cancellation.cancel();
    tasks.close();
    tasks.wait().await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon and docker compose CLI"]
async fn production_compose_actions_preserve_volume_bind_and_network_data() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let run_token = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let release_id = Uuid::new_v4();
    let volume_name = format!("solodock-e2e-compose-volume-{run_token}");
    let network_name = format!("solodock-e2e-compose-network-{run_token}");
    let owned_volume_name = format!("solodock-{}-owned", app_id.simple());
    let owned_network_name = format!("solodock-{}-default", app_id.simple());
    let bind_root = format!("/tmp/solodock-e2e-bind-root-{run_token}");
    let bind_source = format!("{bind_root}/data");
    std::fs::create_dir_all(&bind_source).unwrap();
    let test_labels = HashMap::from([("com.solodock.test-run".into(), run_token.to_string())]);
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(volume_name.clone()),
            labels: Some(test_labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_network(NetworkCreateRequest {
            name: network_name.clone(),
            labels: Some(test_labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

    let seed_name = format!("solodock-e2e-seed-{run_token}");
    let seed = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&seed_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf volume-canary >/volume/value; printf bind-canary >/bind/value".into(),
                ]),
                labels: Some(test_labels.clone()),
                host_config: Some(HostConfig {
                    mounts: Some(vec![Mount {
                        target: Some("/volume".into()),
                        source: Some(volume_name.clone()),
                        typ: Some(MountType::VOLUME),
                        ..Default::default()
                    }]),
                    // Test setup uses Docker's legacy bind syntax solely to
                    // create the daemon-side directory. SoloDock's production
                    // path only consumes an already existing authorized bind.
                    binds: Some(vec![format!("{bind_source}:/bind")]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    docker.start_container(&seed, None).await.unwrap();
    docker
        .wait_container(
            &seed,
            Some(
                WaitContainerOptionsBuilder::default()
                    .condition("not-running")
                    .build(),
            ),
        )
        .next()
        .await
        .unwrap()
        .unwrap();
    remove_test_container(&docker, &seed, run_token).await;

    let image = docker.inspect_image("alpine:3.20").await.unwrap();
    let pinned =
        image.repo_digests.into_iter().flatten().next().expect(
            "isolated daemon must pull a digest-bearing image; mutable fallback is forbidden",
        );
    let project = tempfile::tempdir().unwrap();
    std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(project.path().join("releases")).unwrap();
    std::fs::set_permissions(
        project.path().join("releases"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let compose_file = project.path().join("compose.yaml");
    let draft = solodock::domain::normalize_draft(
        solodock::domain::DraftInput {
            slug: "e2e".into(),
            display_name: "E2E".into(),
            discovery_image_ref: "alpine:3.20".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: solodock::domain::EnvironmentInput::default(),
            files: vec![],
            ports: vec![],
            volumes: vec![
                solodock::domain::VolumeInput::Owned {
                    logical_name: "owned".into(),
                    target_path: "/owned".into(),
                },
                solodock::domain::VolumeInput::External {
                    name: volume_name.clone(),
                    target_path: "/volume".into(),
                },
            ],
            binds: vec![solodock::domain::BindMountInput {
                source: bind_source.clone(),
                target_path: "/bind".into(),
                readonly: true,
                acknowledge_non_rollbackable: false,
            }],
            owned_default_network: true,
            networks: vec![
                solodock::domain::NetworkInput::OwnedDefault,
                solodock::domain::NetworkInput::External {
                    name: network_name.clone(),
                    aliases: vec![],
                },
            ],
            health: solodock::domain::HealthPolicy::default(),
        },
        &solodock::domain::ExistingSecrets::default(),
        b"e2e-integrity",
        &[std::path::PathBuf::from(&bind_root)],
    )
    .unwrap();
    solodock::app_store::config_revision::publish(project.path(), release_id, &draft).unwrap();
    let revision_directory = project
        .path()
        .join("config-revisions")
        .join(release_id.to_string());
    let (yaml, _) = solodock::compose::generate(
        solodock::compose::ComposeInput {
            app_id,
            release_id,
            image_ref: &pinned,
            revision_directory: &revision_directory,
            draft: &draft,
        },
        true,
    )
    .unwrap();
    solodock::app_store::atomic::AtomicWriter::write(&compose_file, yaml.as_bytes(), 0o600)
        .unwrap();
    let cancellation = CancellationToken::new();
    let tasks = TaskTracker::new();
    let runner: Arc<dyn ComposeRunner> = Arc::new(FixedComposeRunner::for_test_http(
        cancellation.clone(),
        tasks.clone(),
        SecretRedactor::new(&EmptySecretProvider),
        endpoint.clone(),
    ));
    let context = || RunContext {
        project_name: format!("solodock-{}", app_id.simple()),
        project_directory: project.path().to_owned(),
        compose_file: compose_file.clone(),
        timeout: Duration::from_secs(60),
        redaction_patterns: vec![],
    };
    let result: Result<(), String> = async {
        runner
            .run(ComposeAction::Recreate, context())
            .await
            .map_err(|error| error.to_string())?;
        for action in [
            ComposeAction::Stop,
            ComposeAction::Start,
            ComposeAction::Restart,
        ] {
            runner
                .run(action, context())
                .await
                .map_err(|error| error.to_string())?;
        }
        let target = docker
            .list_containers(Some(
                bollard::query_parameters::ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&HashMap::from([(
                        "label".to_string(),
                        vec![
                            format!("{PROJECT_LABEL}=solodock-{}", app_id.simple()),
                            format!("{SERVICE_LABEL}=app"),
                        ],
                    )]))
                    .build(),
            ))
            .await
            .map_err(|error| error.to_string())?;
        if target.len() != 1 {
            return Err("expected one exact compose-owned target".into());
        }
        runner
            .run(ComposeAction::Remove, context())
            .await
            .map_err(|error| error.to_string())?;
        let target_id = target[0]
            .id
            .as_deref()
            .ok_or_else(|| "Compose target had no full ID".to_string())?;
        if docker.inspect_container(target_id, None).await.is_ok() {
            return Err("Compose remove left the exact container behind".into());
        }
        docker
            .inspect_volume(&volume_name)
            .await
            .map_err(|error| error.to_string())?;
        let owned_volume = docker
            .inspect_volume(&owned_volume_name)
            .await
            .map_err(|error| error.to_string())?;
        if owned_volume.labels.get(APP_ID_LABEL) != Some(&app_id.to_string()) {
            return Err("owned volume identity was not generated by SoloDock".into());
        }
        docker
            .inspect_network(&network_name, None)
            .await
            .map_err(|error| error.to_string())?;
        let owned_network = docker
            .inspect_network(&owned_network_name, None)
            .await
            .map_err(|error| error.to_string())?;
        if owned_network
            .labels
            .as_ref()
            .and_then(|labels| labels.get(APP_ID_LABEL))
            != Some(&app_id.to_string())
        {
            return Err("owned network identity was not generated by SoloDock".into());
        }
        let reader_name = format!("solodock-e2e-compose-reader-{run_token}");
        let reader = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&reader_name)
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some("alpine:3.20".into()),
                    cmd: Some(vec![
                        "sh".into(),
                        "-c".into(),
                        "cat /volume/value; cat /bind/value".into(),
                    ]),
                    labels: Some(test_labels.clone()),
                    host_config: Some(HostConfig {
                        mounts: Some(vec![
                            Mount {
                                target: Some("/volume".into()),
                                source: Some(volume_name.clone()),
                                typ: Some(MountType::VOLUME),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            Mount {
                                target: Some("/bind".into()),
                                source: Some(bind_source.clone()),
                                typ: Some(MountType::BIND),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .id;
        docker
            .start_container(&reader, None)
            .await
            .map_err(|error| error.to_string())?;
        docker
            .wait_container(
                &reader,
                Some(
                    WaitContainerOptionsBuilder::default()
                        .condition("not-running")
                        .build(),
                ),
            )
            .next()
            .await
            .ok_or_else(|| "reader wait ended".to_string())?
            .map_err(|error| error.to_string())?;
        let output = docker
            .logs(
                &reader,
                Some(
                    bollard::query_parameters::LogsOptionsBuilder::default()
                        .stdout(true)
                        .build(),
                ),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .flat_map(|chunk| chunk.into_bytes())
            .collect::<Vec<_>>();
        if !output
            .windows(b"volume-canary".len())
            .any(|part| part == b"volume-canary")
            || !output
                .windows(b"bind-canary".len())
                .any(|part| part == b"bind-canary")
        {
            return Err("retained volume or bind canary was unreadable".into());
        }
        remove_test_container(&docker, &reader, run_token).await;
        Ok(())
    }
    .await;

    cancellation.cancel();
    tasks.close();
    tasks.wait().await;
    if let Ok(leftovers) = docker
        .list_containers(Some(
            bollard::query_parameters::ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&HashMap::from([(
                    "label".to_string(),
                    vec![format!("com.solodock.test-run={run_token}")],
                )]))
                .build(),
        ))
        .await
    {
        for container in leftovers {
            if let Some(id) = container.id {
                remove_test_container(&docker, &id, run_token).await;
            }
        }
    }
    if let Ok(network) = docker.inspect_network(&network_name, None).await
        && network
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.solodock.test-run"))
            == Some(&run_token.to_string())
    {
        let _ = docker.remove_network(&network_name).await;
    }
    if let Ok(volume) = docker.inspect_volume(&volume_name).await
        && volume.labels.get("com.solodock.test-run") == Some(&run_token.to_string())
    {
        let _ = docker
            .remove_volume(
                &volume_name,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await;
    }
    if let Ok(network) = docker.inspect_network(&owned_network_name, None).await
        && network
            .labels
            .as_ref()
            .and_then(|labels| labels.get(APP_ID_LABEL))
            == Some(&app_id.to_string())
    {
        let _ = docker.remove_network(&owned_network_name).await;
    }
    if let Ok(volume) = docker.inspect_volume(&owned_volume_name).await
        && volume.labels.get(APP_ID_LABEL) == Some(&app_id.to_string())
    {
        let _ = docker
            .remove_volume(
                &owned_volume_name,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await;
    }
    let _ = std::fs::remove_dir_all(&bind_root);
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon and docker compose CLI"]
async fn production_http_mutations_use_canonical_active_release_and_safe_deletion() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST").unwrap();
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let image = docker.inspect_image("nginx:1.27-alpine").await.unwrap();
    let pinned = image
        .repo_digests
        .into_iter()
        .flatten()
        .next()
        .expect("isolated daemon must expose the pulled nginx digest");
    let token = Uuid::new_v4();
    let external_volume = format!("solodock-api-external-volume-{token}");
    let external_network = format!("solodock-api-external-network-{token}");
    let test_labels = HashMap::from([("com.solodock.test-run".into(), token.to_string())]);
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(external_volume.clone()),
            labels: Some(test_labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_network(NetworkCreateRequest {
            name: external_network.clone(),
            labels: Some(test_labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let bind_root = std::path::PathBuf::from(format!("/tmp/solodock-api-bind-root-{token}"));
    let bind_source = bind_root.join("data");
    std::fs::create_dir_all(&bind_source).unwrap();
    let daemon_seed = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&format!("solodock-api-seed-{token}"))
                    .build(),
            ),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf bind-canary >/bind/value".into(),
                ]),
                labels: Some(test_labels.clone()),
                host_config: Some(HostConfig {
                    binds: Some(vec![format!("{}:/bind", bind_source.display())]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    docker.start_container(&daemon_seed, None).await.unwrap();
    docker
        .wait_container(
            &daemon_seed,
            Some(
                WaitContainerOptionsBuilder::default()
                    .condition("not-running")
                    .build(),
            ),
        )
        .next()
        .await
        .unwrap()
        .unwrap();
    remove_test_container(&docker, &daemon_seed, token).await;

    let harness = MutationHarness::new(endpoint.clone(), bind_root.clone()).await;
    let create_body = |slug: &str| {
        json!({
            "slug": slug, "display_name": slug, "discovery_image_ref": "nginx:1.27-alpine",
            "credential_ref": null, "auto_deploy_enabled": false, "poll_interval_seconds": 300,
            "environment": {"public": [], "secrets": []}, "files": [], "ports": [],
            "volumes": [
                {"kind":"owned","logical_name":"owned","target_path":"/owned"},
                {"kind":"external","name":external_volume,"target_path":"/external"}
            ],
            "binds": [{"source":bind_source,"target_path":"/bind","readonly":true,"acknowledge_non_rollbackable":false}],
            "networks": [{"kind":"owned_default"},{"kind":"external","name":external_network}],
            "health": {"policy":"running","stable_window_seconds":5}
        })
    };

    let create = harness
        .request(
            "POST",
            "/api/v1/apps",
            Some("e2e-unregister-create"),
            Some(&create_body("e2e-unregister")),
        )
        .await;
    assert_eq!(create.status(), StatusCode::CREATED);
    let app_id = MutationHarness::json(create).await["app"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    harness.publish_active(app_id, Uuid::new_v4(), &pinned);
    let start = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{app_id}/actions/start"),
            Some("e2e-unregister-start"),
            None,
        )
        .await;
    assert_eq!(
        start.status(),
        StatusCode::OK,
        "{}",
        MutationHarness::json(start).await
    );
    let containers = harness
        .state
        .observer
        .api()
        .list_compose_app_containers(&format!("solodock-{}", app_id.simple()))
        .await
        .unwrap();
    assert_eq!(containers.len(), 1);
    let retained_container = containers[0].id.clone();
    let preview = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{app_id}/deletion-preview"),
            None,
            Some(&json!({"remove_container":false})),
        )
        .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = MutationHarness::json(preview).await;
    let delete = harness
        .request(
            "DELETE",
            &format!("/api/v1/apps/{app_id}"),
            Some("e2e-unregister-delete"),
            Some(&json!({
                "confirmation_token":preview["confirmation_token"], "slug":"e2e-unregister",
                "expected_revision":preview["expected_revision"], "remove_container":false
            })),
        )
        .await;
    assert_eq!(
        delete.status(),
        StatusCode::OK,
        "{}",
        MutationHarness::json(delete).await
    );
    assert!(
        docker
            .inspect_container(&retained_container, None)
            .await
            .is_ok()
    );
    remove_owned_container(&docker, &retained_container, app_id).await;

    let create = harness
        .request(
            "POST",
            "/api/v1/apps",
            Some("e2e-remove-create"),
            Some(&create_body("e2e-remove")),
        )
        .await;
    assert_eq!(create.status(), StatusCode::CREATED);
    let remove_app_id = MutationHarness::json(create).await["app"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    harness.publish_active(remove_app_id, Uuid::new_v4(), &pinned);
    let start = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{remove_app_id}/actions/start"),
            Some("e2e-remove-start"),
            None,
        )
        .await;
    assert_eq!(
        start.status(),
        StatusCode::OK,
        "{}",
        MutationHarness::json(start).await
    );
    let preview = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{remove_app_id}/deletion-preview"),
            None,
            Some(&json!({"remove_container":true})),
        )
        .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = MutationHarness::json(preview).await;
    let removed_id = preview["container_ids"][0].as_str().unwrap().to_owned();
    let delete = harness
        .request(
            "DELETE",
            &format!("/api/v1/apps/{remove_app_id}"),
            Some("e2e-remove-delete"),
            Some(&json!({
                "confirmation_token":preview["confirmation_token"], "slug":"e2e-remove",
                "expected_revision":preview["expected_revision"], "remove_container":true
            })),
        )
        .await;
    assert_eq!(
        delete.status(),
        StatusCode::OK,
        "{}",
        MutationHarness::json(delete).await
    );
    assert!(docker.inspect_container(&removed_id, None).await.is_err());
    assert!(docker.inspect_volume(&external_volume).await.is_ok());
    assert!(
        docker
            .inspect_network(&external_network, None)
            .await
            .is_ok()
    );
    for id in [app_id, remove_app_id] {
        let volume = format!("solodock-{}-owned", id.simple());
        let network = format!("solodock-{}-default", id.simple());
        let inspected = docker.inspect_volume(&volume).await.unwrap();
        assert_eq!(inspected.labels.get(APP_ID_LABEL), Some(&id.to_string()));
        docker
            .remove_volume(
                &volume,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await
            .unwrap();
        let inspected = docker.inspect_network(&network, None).await.unwrap();
        assert_eq!(
            inspected
                .labels
                .as_ref()
                .and_then(|labels| labels.get(APP_ID_LABEL)),
            Some(&id.to_string())
        );
        docker.remove_network(&network).await.unwrap();
    }
    assert_eq!(
        docker
            .inspect_volume(&external_volume)
            .await
            .unwrap()
            .labels
            .get("com.solodock.test-run"),
        Some(&token.to_string())
    );
    docker.remove_network(&external_network).await.unwrap();
    docker
        .remove_volume(
            &external_volume,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();
    harness.state.shutdown.cancel();
    harness.state.stream_tasks.close();
    harness.state.stream_tasks.wait().await;
    let _ = std::fs::remove_dir_all(bind_root);
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon, Registry access, and docker compose CLI"]
async fn production_m5_polling_digest_deploy_auto_rollback_and_manual_rollback() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST").unwrap();
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let token = Uuid::new_v4();
    let registry_user = "solodock-fixture";
    let registry_secret = format!("m4-private-canary-{token}");
    let auth_directory = tempfile::tempdir().unwrap();
    let registry_network = format!("solodock-m4-registry-network-{token}");
    let registry_data_volume = format!("solodock-m4-registry-data-{token}");
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(registry_data_volume.clone()),
            labels: Some(HashMap::from([(
                "com.solodock.test-run".into(),
                token.to_string(),
            )])),
            ..Default::default()
        })
        .await
        .unwrap();
    docker_cli(
        &endpoint,
        &[
            "network",
            "create",
            "--label",
            &format!("com.solodock.test-run={token}"),
            &registry_network,
        ],
    )
    .await;
    let registry_name = format!("solodock-m4-registry-backend-{token}");
    let registry_id = docker_cli(
        &endpoint,
        &[
            "create",
            "--name",
            &registry_name,
            "--label",
            &format!("com.solodock.test-run={token}"),
            "--network",
            &registry_network,
            "--network-alias",
            "registry-backend",
            "-v",
            &format!("{registry_data_volume}:/var/lib/registry"),
            "registry:2",
        ],
    )
    .await;
    docker_cli(&endpoint, &["start", registry_id.trim()]).await;
    let basic = STANDARD.encode(format!("{registry_user}:{registry_secret}"));
    let proxy_config = auth_directory.path().join("nginx.conf");
    std::fs::write(
        &proxy_config,
        format!(
            r#"events {{}}
http {{
  server {{
    listen 5000;
    client_max_body_size 0;
    location = /token {{
      if ($http_authorization != "Basic {basic}") {{ return 403; }}
      default_type application/json;
      return 200 '{{"token":"fixture-bearer-token"}}';
    }}
    location /v2/ {{
      error_page 418 = @challenge;
      if ($http_authorization != "Bearer fixture-bearer-token") {{ return 418; }}
      proxy_set_header Authorization "";
      proxy_set_header Host $http_host;
      proxy_pass http://registry-backend:5000;
    }}
    location @challenge {{
      add_header WWW-Authenticate 'Bearer realm="http://127.0.0.1:5000/token",service="solodock-fixture"' always;
      return 401;
    }}
  }}
}}
"#
        ),
    )
    .unwrap();
    let proxy_name = format!("solodock-m4-registry-proxy-{token}");
    let proxy_id = docker_cli(
        &endpoint,
        &[
            "create",
            "--name",
            &proxy_name,
            "--label",
            &format!("com.solodock.test-run={token}"),
            "--network",
            &registry_network,
            "-p",
            "5000:5000",
            "nginx:1.27-alpine",
        ],
    )
    .await;
    docker_cli(
        &endpoint,
        &[
            "cp",
            proxy_config.to_str().unwrap(),
            &format!("{}:/etc/nginx/nginx.conf", proxy_id.trim()),
        ],
    )
    .await;
    docker_cli(&endpoint, &["start", proxy_id.trim()]).await;
    let readiness_client = reqwest::Client::new();
    let mut registry_ready = false;
    for _ in 0..100 {
        if readiness_client
            .get("http://127.0.0.1:5000/v2/")
            .bearer_auth("fixture-bearer-token")
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            registry_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        registry_ready,
        "Bearer proxy and Registry backend never became ready"
    );
    let docker_config = tempfile::tempdir().unwrap();
    docker_cli_with_input(
        &endpoint,
        docker_config.path(),
        &[
            "login",
            "127.0.0.1:5000",
            "--username",
            registry_user,
            "--password-stdin",
        ],
        registry_secret.as_bytes(),
    )
    .await;
    for (source, target) in [
        ("nginx:1.27-alpine", "127.0.0.1:5000/solodock/nginx:stable"),
        ("alpine:3.20", "127.0.0.1:5000/solodock/alpine:stable"),
    ] {
        docker_cli(&endpoint, &["tag", source, target]).await;
        docker_cli_with_input(&endpoint, docker_config.path(), &["push", target], &[]).await;
    }
    let unhealthy_image = format!("solodock-m5-unhealthy-{token}:latest");
    docker_cli_with_input(
        &endpoint,
        docker_config.path(),
        &["build", "-t", &unhealthy_image, "-"],
        b"FROM nginx:1.27-alpine\nHEALTHCHECK --interval=1s --timeout=1s --retries=1 CMD false\n",
    )
    .await;
    let external_volume = format!("solodock-m4-external-volume-{token}");
    let external_network = format!("solodock-m4-external-network-{token}");
    let labels = HashMap::from([("com.solodock.test-run".into(), token.to_string())]);
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(external_volume.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_network(NetworkCreateRequest {
            name: external_network.clone(),
            labels: Some(labels),
            ..Default::default()
        })
        .await
        .unwrap();
    let bind_root = std::path::PathBuf::from(format!("/tmp/solodock-m4-bind-root-{token}"));
    std::fs::create_dir_all(&bind_root).unwrap();
    let bind_source = bind_root.join("persistent-data");
    std::fs::create_dir_all(&bind_source).unwrap();
    let bind_canary = format!("m4-bind-canary-{token}");
    std::fs::write(bind_source.join("canary"), &bind_canary).unwrap();
    let pull_gate = TestPullGate::new([
        TestPullAction::Pause,
        TestPullAction::Continue,
        TestPullAction::Continue,
        TestPullAction::Interrupt,
    ]);
    let effect_gate = TestEffectGate::new([
        TestEffectAction::Continue,
        TestEffectAction::Continue,
        TestEffectAction::Continue,
        TestEffectAction::Continue,
        TestEffectAction::Continue,
        TestEffectAction::Pause,
    ]);
    let harness = MutationHarness::new_with_test_gates(
        endpoint.clone(),
        bind_root.clone(),
        Some(pull_gate.clone()),
        Some(effect_gate.clone()),
    )
    .await;
    let resource_sampler = ProcessPeakSampler::start(std::process::id());
    let idle_metrics = process_metrics(std::process::id()).unwrap();
    let stream_hold_seconds = resource_stream_hold_seconds();
    let credential = harness
        .request(
            "POST",
            "/api/v1/registry-credentials",
            Some("m4-e2e-credential-0001"),
            Some(&json!({
                "registry":"127.0.0.1:5000",
                "username":registry_user,
                "secret":registry_secret,
            })),
        )
        .await;
    assert_eq!(credential.status(), StatusCode::CREATED);
    let credential_body = MutationHarness::json(credential).await;
    assert!(!credential_body.to_string().contains(&registry_secret));
    let credential_id = credential_body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        harness.state.redactor.redact(registry_secret.as_bytes()),
        b"[REDACTED]"
    );
    let draft = |slug: &str, image: &str, health: Value| {
        json!({
            "slug": slug, "display_name": "M4 E2E", "discovery_image_ref": image,
            "credential_ref": credential_id, "auto_deploy_enabled": true,
            "auto_deploy_acknowledged": true, "poll_interval_seconds": 300,
            "environment": {"public": [], "secrets": []}, "files": [], "ports": [],
            "volumes": [
                {"kind":"owned","logical_name":"owned","target_path":"/owned"},
                {"kind":"external","name":external_volume,"target_path":"/external"}
            ],
            "binds": [{
                "source": bind_source,
                "target_path": "/bind",
                "readonly": true,
                "acknowledge_non_rollbackable": false
            }],
            "networks": [{"kind":"owned_default"},{"kind":"external","name":external_network}],
            "health": health
        })
    };
    let create = harness
        .request(
            "POST",
            "/api/v1/apps",
            Some("m4-e2e-create-0001"),
            Some(&draft(
                "m4-e2e",
                "127.0.0.1:5000/solodock/nginx:stable",
                json!({"policy":"running","stable_window_seconds":5}),
            )),
        )
        .await;
    assert_eq!(create.status(), StatusCode::CREATED);
    let app_id = MutationHarness::json(create).await["app"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    assert_eq!(
        harness.state.redactor.redact(registry_secret.as_bytes()),
        b"[REDACTED]",
        "app publication must not drop registry credentials from the inventory"
    );

    let project = format!("solodock-{}", app_id.simple());
    let collision_name = format!("solodock-m4-collision-{token}");
    let collision_id = docker_cli(
        &endpoint,
        &[
            "run",
            "-d",
            "--name",
            &collision_name,
            "--label",
            &format!("com.solodock.test-run={token}"),
            "--label",
            &format!("com.docker.compose.project={project}"),
            "--label",
            "com.docker.compose.service=app",
            "nginx:1.27-alpine",
        ],
    )
    .await;
    let current = app_detail(&harness, app_id).await;
    let blocked = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{app_id}/deployments"),
            Some("m4-e2e-collision-0001"),
            Some(&json!({
                "expected_draft_revision":current["draft_revision"],
                "expected_active_release_id":current["active_release"]["id"].as_str(),
                "expected_pending_release_id":current["pending_release_id"],
                "expected_actual_release_id":current["actual_release_id"],
                "expected_actual_container_id":current["actual"]["id"].as_str(),
                "acknowledge_non_rollbackable_data":true
            })),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(
        MutationHarness::json(blocked).await["code"],
        "APP_CONTAINER_INVALID"
    );
    remove_test_container(&docker, collision_id.trim(), token).await;

    let m3 = harness.state.m3.as_ref().unwrap().clone();
    let m4 = harness.state.m4.as_ref().unwrap().clone();
    assert!(m3.store.read_metadata(app_id).unwrap().auto_deploy_enabled);
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    let deployments = m4.ledger.list(app_id, 1).await.unwrap();
    assert!(
        !deployments.is_empty(),
        "first poll did not schedule a deployment: {:?}",
        m4.poller.store.get(app_id).await.unwrap()
    );
    let first = deployments[0].id;
    // The worker has already resolved the tag and published the immutable
    // candidate when the test-only pull boundary pauses. Moving the tag here
    // proves the production pull/Compose path remains pinned to that digest.
    pull_gate.wait_until_reached().await;
    docker_cli(
        &endpoint,
        &["tag", "alpine:3.20", "127.0.0.1:5000/solodock/nginx:stable"],
    )
    .await;
    docker_cli_with_input(
        &endpoint,
        docker_config.path(),
        &["push", "127.0.0.1:5000/solodock/nginx:stable"],
        &[],
    )
    .await;
    pull_gate.resume();
    let first = wait_for_deployment(&harness, first).await;
    assert_eq!(first["status"], "succeeded", "{first}");
    assert!(!first.to_string().contains(&registry_secret));
    let first_release = first["candidate_release_id"].as_str().unwrap().to_owned();
    let owned_volume = format!("solodock-{}-owned", app_id.simple());
    write_m4_volume_canaries(&docker, &owned_volume, &external_volume, token).await;
    assert!(
        first["source_image_ref"]
            .as_str()
            .unwrap()
            .ends_with("/solodock/nginx:stable")
    );

    let after_tag_move = app_detail(&harness, app_id).await;
    assert_eq!(after_tag_move["active_release"]["id"], first_release);
    assert!(
        after_tag_move["actual"]["configured_image_ref"]
            .as_str()
            .unwrap()
            .contains("@sha256:")
    );
    assert!(
        first["manifest_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    // Hold the representative per-session maximum of real authenticated SSE
    // responses (4 events + 2 logs + 2 stats) and record the control-plane
    // delta. Dropping the bodies must release every stream permit again.
    let mut stream_responses = Vec::new();
    for suffix in [
        "events", "events", "events", "events", "logs", "logs", "stats", "stats",
    ] {
        let response = harness
            .request(
                "GET",
                &format!("/api/v1/apps/{app_id}/{suffix}"),
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "failed to open {suffix}");
        stream_responses.push(response);
    }
    assert_eq!(harness.state.stream_gate.active(), 8);
    tokio::time::sleep(Duration::from_secs(stream_hold_seconds)).await;
    let stream_metrics = process_metrics(std::process::id()).unwrap();
    drop(stream_responses);
    tokio::time::timeout(Duration::from_secs(10), async {
        while harness.state.stream_gate.active() != 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("SSE permits were not released");

    // Restore the original tag and verify repeated observations do not create
    // another deployment row for the active digest.
    docker_cli(
        &endpoint,
        &[
            "tag",
            "nginx:1.27-alpine",
            "127.0.0.1:5000/solodock/nginx:stable",
        ],
    )
    .await;
    docker_cli_with_input(
        &endpoint,
        docker_config.path(),
        &["push", "127.0.0.1:5000/solodock/nginx:stable"],
        &[],
    )
    .await;
    let before_unchanged = m4.ledger.list(app_id, 20).await.unwrap().len();
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    assert_eq!(
        m4.ledger.list(app_id, 20).await.unwrap().len(),
        before_unchanged,
        "an unchanged digest must not create a deployment"
    );

    // A new digest with a deterministically failing image healthcheck rolls
    // back to the exact previous release and is durably suppressed.
    let current = app_detail(&harness, app_id).await;
    update_draft(
        &harness,
        app_id,
        current["draft_revision"].as_str().unwrap(),
        draft(
            "m4-e2e",
            "127.0.0.1:5000/solodock/nginx:stable",
            json!({"policy":"healthy"}),
        ),
        "m5-e2e-health-update-0001",
    )
    .await;
    docker_cli(
        &endpoint,
        &[
            "tag",
            &unhealthy_image,
            "127.0.0.1:5000/solodock/nginx:stable",
        ],
    )
    .await;
    docker_cli_with_input(
        &endpoint,
        docker_config.path(),
        &["push", "127.0.0.1:5000/solodock/nginx:stable"],
        &[],
    )
    .await;
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    let failed_poll = m4.ledger.list(app_id, 1).await.unwrap()[0].id;
    let failed_poll = wait_for_deployment(&harness, failed_poll).await;
    assert_eq!(failed_poll["trigger"], "poll");
    assert_eq!(failed_poll["status"], "rolled_back", "{failed_poll}");
    assert_eq!(
        app_detail(&harness, app_id).await["active_release"]["id"],
        first_release
    );
    let before_suppressed = m4.ledger.list(app_id, 20).await.unwrap().len();
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    assert_eq!(
        m4.ledger.list(app_id, 20).await.unwrap().len(),
        before_suppressed,
        "a failed poll target must remain suppressed"
    );

    let current = app_detail(&harness, app_id).await;
    let unhealthy = draft(
        "m4-e2e",
        "127.0.0.1:5000/solodock/alpine:stable",
        json!({"policy":"healthy"}),
    );
    update_draft(
        &harness,
        app_id,
        current["draft_revision"].as_str().unwrap(),
        unhealthy,
        "m4-e2e-update-0001",
    )
    .await;
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    let interrupted = m4.ledger.list(app_id, 1).await.unwrap()[0].id;
    let interrupted = wait_for_deployment(&harness, interrupted).await;
    assert_eq!(interrupted["trigger"], "poll", "{interrupted}");
    assert_eq!(interrupted["status"], "interrupted", "{interrupted}");
    let interrupted_facts = app_detail(&harness, app_id).await;
    assert!(interrupted_facts["pending_release_id"].is_string());
    assert_eq!(
        interrupted_facts["actual_release_id"],
        interrupted_facts["active_release"]["id"]
    );
    let interrupted_id = interrupted["id"].as_str().unwrap();
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    let failed = m4.ledger.list(app_id, 1).await.unwrap()[0].id;
    assert_ne!(failed.to_string(), interrupted_id);
    let failed = wait_for_deployment(&harness, failed).await;
    assert_eq!(failed["trigger"], "poll", "{failed}");
    assert_eq!(failed["status"], "rolled_back", "{failed}");
    assert_eq!(
        app_detail(&harness, app_id).await["active_release"]["id"],
        first_release
    );

    let current = app_detail(&harness, app_id).await;
    update_draft(
        &harness,
        app_id,
        current["draft_revision"].as_str().unwrap(),
        draft(
            "m4-e2e",
            "127.0.0.1:5000/solodock/alpine:stable",
            json!({"policy":"completed"}),
        ),
        "m4-e2e-update-0002",
    )
    .await;
    let blocked_by_final_recheck =
        schedule_from_current(&harness, app_id, "m4-e2e-deploy-0003").await;
    effect_gate.wait_until_reached().await;
    let before_busy_poll = m4.ledger.list(app_id, 20).await.unwrap().len();
    assert!(
        m4.poller
            .run_once_for_test(&harness.state, &m3, &m4, app_id)
            .await
    );
    assert_eq!(
        m4.ledger.list(app_id, 20).await.unwrap().len(),
        before_busy_poll,
        "polling an app with a deployment lock must not queue work"
    );
    let final_collision_name = format!("solodock-m4-final-collision-{token}");
    let final_collision = docker_cli(
        &endpoint,
        &[
            "run",
            "-d",
            "--name",
            &final_collision_name,
            "--label",
            &format!("com.solodock.test-run={token}"),
            "--label",
            &format!("com.docker.compose.project={project}"),
            "--label",
            "com.docker.compose.service=app",
            "alpine:3.20",
            "sleep",
            "300",
        ],
    )
    .await;
    effect_gate.resume();
    let blocked_by_final_recheck = wait_for_deployment(&harness, blocked_by_final_recheck).await;
    assert_eq!(
        blocked_by_final_recheck["status"], "needs_attention",
        "{blocked_by_final_recheck}"
    );
    assert_eq!(
        blocked_by_final_recheck["error_code"],
        "APP_CONTAINER_AMBIGUOUS"
    );
    remove_test_container(&docker, final_collision.trim(), token).await;
    let second = schedule_from_current(&harness, app_id, "m4-e2e-deploy-0003-resume").await;
    let second = wait_for_deployment(&harness, second).await;
    assert_eq!(second["status"], "succeeded", "{second}");
    assert_ne!(second["candidate_release_id"], first_release);

    let current = app_detail(&harness, app_id).await;
    let rollback = harness
        .request(
            "POST",
            &format!(
                "/api/v1/deployments/{}/rollback",
                first["id"].as_str().unwrap()
            ),
            Some("m4-e2e-rollback-0001"),
            Some(&json!({
                "expected_active_release_id":current["active_release"]["id"],
                "expected_pending_release_id":current["pending_release_id"],
                "expected_actual_release_id":current["actual_release_id"],
                "expected_actual_container_id":current["actual"]["id"],
                "acknowledge_non_rollbackable_data":true
            })),
        )
        .await;
    assert_eq!(rollback.status(), StatusCode::ACCEPTED);
    let rollback_id = MutationHarness::json(rollback).await["deployment_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let rollback = wait_for_deployment(&harness, rollback_id).await;
    assert_eq!(rollback["status"], "succeeded", "{rollback}");
    assert_eq!(
        app_detail(&harness, app_id).await["active_release"]["id"],
        first_release
    );
    assert!(docker.inspect_volume(&external_volume).await.is_ok());
    assert!(
        docker
            .inspect_network(&external_network, None)
            .await
            .is_ok()
    );
    assert_eq!(
        std::fs::read_to_string(bind_source.join("canary")).unwrap(),
        bind_canary,
        "deploy and both rollback paths must preserve bind-mounted data"
    );
    assert_m4_volume_canaries(&docker, &owned_volume, &external_volume, token).await;
    assert_secret_absent(&harness.state.state_directory, registry_secret.as_bytes());

    let candidates = harness
        .state
        .observer
        .api()
        .list_compose_app_containers(&format!("solodock-{}", app_id.simple()))
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    remove_owned_container(&docker, &candidates[0].id, app_id).await;
    assert_eq!(
        docker
            .inspect_volume(&owned_volume)
            .await
            .unwrap()
            .labels
            .get(APP_ID_LABEL),
        Some(&app_id.to_string())
    );
    docker
        .remove_volume(
            &owned_volume,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();
    let owned_network = format!("solodock-{}-default", app_id.simple());
    assert_eq!(
        docker
            .inspect_network(&owned_network, None)
            .await
            .unwrap()
            .labels
            .as_ref()
            .and_then(|value| value.get(APP_ID_LABEL)),
        Some(&app_id.to_string())
    );
    docker.remove_network(&owned_network).await.unwrap();
    docker.remove_network(&external_network).await.unwrap();
    docker
        .remove_volume(
            &external_volume,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();

    // Exercise the production heap/dispatch loop and shutdown ownership. A
    // second auto-enabled app is made immediately due; the real coordinator
    // schedules it, then global shutdown interrupts the worker at the pull
    // boundary and joins both tasks with recoverable pending facts intact.
    let shutdown_create = harness
        .request(
            "POST",
            "/api/v1/apps",
            Some("m5-e2e-shutdown-create-0001"),
            Some(&json!({
                "slug":"m5-shutdown", "display_name":"M5 shutdown",
                "discovery_image_ref":"127.0.0.1:5000/solodock/nginx:stable",
                "credential_ref":credential_id, "auto_deploy_enabled":true,
                "auto_deploy_acknowledged":true, "poll_interval_seconds":300,
                "environment":{"public":[],"secrets":[]}, "files":[], "ports":[],
                "volumes":[], "binds":[], "networks":[],
                "health":{"policy":"running","stable_window_seconds":5}
            })),
        )
        .await;
    assert_eq!(shutdown_create.status(), StatusCode::CREATED);
    let shutdown_app = MutationHarness::json(shutdown_create).await["app"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let webhook_key = *b"M6_E2E_WEBHOOK_SECRET_32_BYTES!!";
    let webhook_secret = URL_SAFE_NO_PAD.encode(webhook_key);
    let configured = harness
        .request(
            "PUT",
            &format!("/api/v1/apps/{shutdown_app}/webhook"),
            Some("m6-e2e-webhook-configure"),
            Some(&json!({"expected_metadata_revision":null,"secret":webhook_secret})),
        )
        .await;
    assert_eq!(configured.status(), StatusCode::OK);
    assert_eq!(
        harness.state.redactor.redact(webhook_secret.as_bytes()),
        b"[REDACTED]"
    );
    let webhook_body = br#"{"event":"registry.push"}"#;
    let webhook_timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let webhook_nonce = URL_SAFE_NO_PAD.encode([4_u8; 16]);
    let signing_input = solodock::webhook::protocol::signing_input(
        shutdown_app,
        webhook_body,
        webhook_timestamp,
        &webhook_nonce,
    );
    let mut webhook_mac = Hmac::<Sha256>::new_from_slice(&webhook_key).unwrap();
    webhook_mac.update(signing_input.as_bytes());
    let webhook_signature = format!("v1={:x}", webhook_mac.finalize().into_bytes());
    let webhook = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/hooks/v1/apps/{shutdown_app}/registry"))
                .header(header::HOST, "hooks.example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-solodock-timestamp", webhook_timestamp.to_string())
                .header("x-solodock-nonce", webhook_nonce)
                .header("x-solodock-signature", webhook_signature)
                .body(Body::from(webhook_body.as_slice()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(webhook.status(), StatusCode::ACCEPTED);
    assert_eq!(
        m4.poller
            .store
            .get(shutdown_app)
            .await
            .unwrap()
            .unwrap()
            .webhook_sequence,
        1
    );
    pull_gate.push(TestPullAction::Pause);
    let coordinator_task = m4
        .poller
        .start(harness.state.clone(), m3.clone(), m4.clone());
    // No notify is needed: the production coordinator recovers the durable
    // sequence accepted before it started.
    tokio::time::timeout(Duration::from_secs(30), pull_gate.wait_until_reached())
        .await
        .expect("production coordinator did not dispatch the due app");
    let shutdown_deployment = m4.ledger.list(shutdown_app, 1).await.unwrap()[0].id;
    harness.state.shutdown.cancel();
    harness.state.stream_tasks.close();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = coordinator_task.await;
        harness.state.stream_tasks.wait().await;
    })
    .await
    .expect("poll coordinator and deployment worker did not join on shutdown");
    let shutdown_record = m4.ledger.get(shutdown_deployment).await.unwrap().unwrap();
    assert_eq!(
        shutdown_record.trigger,
        solodock::deploy::DeploymentTrigger::Poll
    );
    assert_eq!(
        shutdown_record.status,
        solodock::deploy::DeploymentStatus::Interrupted
    );
    assert!(
        m3.store
            .read_release_link(shutdown_app, "pending")
            .unwrap()
            .is_some()
    );
    assert!(
        m3.store
            .read_release_link(shutdown_app, "active")
            .unwrap()
            .is_none()
    );
    let control_plane_peak_rss_kib = resource_sampler.finish().await;
    if let Ok(report_path) = std::env::var("SOLODOCK_E2E_RESOURCE_REPORT") {
        let metadata_bytes = tree_bytes(&harness.state.state_directory).unwrap();
        let report = json!({
            "scenario": "authenticated streams plus private Registry resolve/pull/apply/health/rollback",
            "stream_connections": 8,
            "stream_hold_seconds": stream_hold_seconds,
            "stream_sample_timing": "at end of hold window",
            "idle": {
                "rss_kib": idle_metrics.rss_kib,
                "fd_count": idle_metrics.fd_count,
                "task_count": idle_metrics.task_count,
            },
            "streams": {
                "rss_kib": stream_metrics.rss_kib,
                "rss_delta_kib": stream_metrics.rss_kib.saturating_sub(idle_metrics.rss_kib),
                "fd_count": stream_metrics.fd_count,
                "task_count": stream_metrics.task_count,
            },
            "control_plane_peak_rss_kib": control_plane_peak_rss_kib,
            "control_plane_metadata_bytes": metadata_bytes,
            "daemon_measurement": "reported separately by measure-dind-daemon.sh",
        });
        std::fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    remove_test_container(&docker, proxy_id.trim(), token).await;
    remove_test_container(&docker, registry_id.trim(), token).await;
    let inspected_registry_network = docker
        .inspect_network(&registry_network, None)
        .await
        .unwrap();
    assert_eq!(
        inspected_registry_network
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.solodock.test-run")),
        Some(&token.to_string())
    );
    docker.remove_network(&registry_network).await.unwrap();
    let inspected_registry_volume = docker.inspect_volume(&registry_data_volume).await.unwrap();
    assert_eq!(
        inspected_registry_volume
            .labels
            .get("com.solodock.test-run"),
        Some(&token.to_string())
    );
    docker
        .remove_volume(
            &registry_data_volume,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(bind_root);
}

async fn docker_cli(endpoint: &str, args: &[&str]) -> String {
    let output = tokio::process::Command::new("docker")
        .arg("-H")
        .arg(endpoint)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "docker {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn docker_cli_with_input(
    endpoint: &str,
    config: &std::path::Path,
    args: &[&str],
    input: &[u8],
) -> String {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("docker")
        .arg("-H")
        .arg(endpoint)
        .env("DOCKER_CONFIG", config)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).await.unwrap();
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "docker {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_secret_absent(root: &std::path::Path, secret: &[u8]) {
    fn visit(path: &std::path::Path, secret: &[u8]) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == "registry-credentials" {
                continue;
            }
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                visit(&entry.path(), secret);
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                assert!(
                    !bytes.windows(secret.len()).any(|window| window == secret),
                    "secret canary leaked into {}",
                    entry.path().display()
                );
            }
        }
    }
    visit(root, secret);
}

async fn app_detail(harness: &MutationHarness, app_id: Uuid) -> Value {
    let response = harness
        .request("GET", &format!("/api/v1/apps/{app_id}"), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    MutationHarness::json(response).await
}

async fn schedule_from_current(harness: &MutationHarness, app_id: Uuid, key: &str) -> Uuid {
    let current = app_detail(harness, app_id).await;
    let response = harness
        .request(
            "POST",
            &format!("/api/v1/apps/{app_id}/deployments"),
            Some(key),
            Some(&json!({
                "expected_draft_revision":current["draft_revision"],
                "expected_active_release_id":current["active_release"]["id"].as_str(),
                "expected_pending_release_id":current["pending_release_id"],
                "expected_actual_release_id":current["actual_release_id"],
                "expected_actual_container_id":current["actual"]["id"].as_str(),
                "acknowledge_non_rollbackable_data":true
            })),
        )
        .await;
    let status = response.status();
    let body = MutationHarness::json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    body["deployment_id"].as_str().unwrap().parse().unwrap()
}

async fn wait_for_deployment(harness: &MutationHarness, id: Uuid) -> Value {
    for _ in 0..360 {
        let response = harness
            .request("GET", &format!("/api/v1/deployments/{id}"), None, None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = MutationHarness::json(response).await;
        if !matches!(value["status"].as_str(), Some("queued" | "running")) {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("deployment {id} did not become terminal")
}

async fn update_draft(
    harness: &MutationHarness,
    app_id: Uuid,
    expected: &str,
    draft: Value,
    key: &str,
) {
    let response = harness
        .request(
            "PUT",
            &format!("/api/v1/apps/{app_id}/draft"),
            Some(key),
            Some(&json!({"expected_revision":expected,"draft":draft})),
        )
        .await;
    let status = response.status();
    let body = MutationHarness::json(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn remove_owned_container(docker: &Docker, container_id: &str, app_id: Uuid) {
    let inspected = docker.inspect_container(container_id, None).await.unwrap();
    assert_eq!(
        inspected
            .config
            .and_then(|config| config.labels)
            .and_then(|labels| labels.get(APP_ID_LABEL).cloned()),
        Some(app_id.to_string())
    );
    docker
        .remove_container(
            container_id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .unwrap();
}

async fn remove_test_container(docker: &Docker, container_id: &str, run_token: Uuid) {
    let inspected = docker.inspect_container(container_id, None).await.unwrap();
    assert_eq!(
        inspected
            .config
            .and_then(|config| config.labels)
            .and_then(|labels| labels.get("com.solodock.test-run").cloned()),
        Some(run_token.to_string())
    );
    docker
        .remove_container(
            container_id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .unwrap();
}

async fn write_m4_volume_canaries(docker: &Docker, owned: &str, external: &str, run_token: Uuid) {
    let labels = HashMap::from([("com.solodock.test-run".into(), run_token.to_string())]);
    let writer_name = format!("solodock-m4-volume-writer-{run_token}");
    let writer = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&writer_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf owned-canary >/owned/value; printf external-canary >/external/value"
                        .into(),
                ]),
                labels: Some(labels),
                host_config: Some(HostConfig {
                    mounts: Some(vec![
                        Mount {
                            target: Some("/owned".into()),
                            source: Some(owned.into()),
                            typ: Some(MountType::VOLUME),
                            ..Default::default()
                        },
                        Mount {
                            target: Some("/external".into()),
                            source: Some(external.into()),
                            typ: Some(MountType::VOLUME),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    docker.start_container(&writer, None).await.unwrap();
    docker
        .wait_container(
            &writer,
            Some(
                WaitContainerOptionsBuilder::default()
                    .condition("not-running")
                    .build(),
            ),
        )
        .next()
        .await
        .unwrap()
        .unwrap();
    remove_test_container(docker, &writer, run_token).await;
}

async fn assert_m4_volume_canaries(docker: &Docker, owned: &str, external: &str, run_token: Uuid) {
    let labels = HashMap::from([("com.solodock.test-run".into(), run_token.to_string())]);
    let reader_name = format!("solodock-m4-volume-reader-{run_token}");
    let reader = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&reader_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "cat /owned/value /external/value".into(),
                ]),
                labels: Some(labels),
                host_config: Some(HostConfig {
                    mounts: Some(vec![
                        Mount {
                            target: Some("/owned".into()),
                            source: Some(owned.into()),
                            typ: Some(MountType::VOLUME),
                            read_only: Some(true),
                            ..Default::default()
                        },
                        Mount {
                            target: Some("/external".into()),
                            source: Some(external.into()),
                            typ: Some(MountType::VOLUME),
                            read_only: Some(true),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    docker.start_container(&reader, None).await.unwrap();
    docker
        .wait_container(
            &reader,
            Some(
                WaitContainerOptionsBuilder::default()
                    .condition("not-running")
                    .build(),
            ),
        )
        .next()
        .await
        .unwrap()
        .unwrap();
    let output = docker
        .logs(
            &reader,
            Some(
                bollard::query_parameters::LogsOptionsBuilder::default()
                    .stdout(true)
                    .build(),
            ),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .flat_map(|chunk| chunk.into_bytes())
        .collect::<Vec<_>>();
    assert!(
        output
            .windows(b"owned-canary".len())
            .any(|part| part == b"owned-canary")
    );
    assert!(
        output
            .windows(b"external-canary".len())
            .any(|part| part == b"external-canary")
    );
    remove_test_container(docker, &reader, run_token).await;
}
