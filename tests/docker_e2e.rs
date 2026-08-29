#![cfg(feature = "docker-e2e")]

use std::{collections::HashMap, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};

use bollard::{
    API_DEFAULT_VERSION, Docker,
    models::{
        ContainerCreateBody, HostConfig, Mount, MountType, NetworkConnectRequest,
        NetworkCreateRequest, VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use futures_util::StreamExt;
use solodock::docker::{
    client::BollardReadClient,
    models::{DockerReadApi, LogRequest, LogStreamKind},
    ownership::*,
};
use solodock::{
    AppState,
    api::mutations::M3Services,
    app_store::AppStore,
    auth::AuthService,
    compose::{ComposeAction, ComposeCapability, ComposeRunner, FixedComposeRunner, RunContext},
    db::Database,
    docker::{
        AppCatalog, DockerObserver,
        events::DockerEventHub,
        logs::{EmptySecretProvider, SecretRedactor},
        probe::DockerSupervisor,
        stats::StatsHub,
    },
    mutation::{AppMutationCoordinator, IdempotencyService},
    router,
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

impl MutationHarness {
    async fn new(endpoint: String, bind_root: std::path::PathBuf) -> Self {
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
        let compose: Arc<dyn ComposeRunner> = Arc::new(FixedComposeRunner::for_test_http(
            shutdown.clone(),
            tasks.clone(),
            redactor.clone(),
            endpoint,
        ));
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
            stats: StatsHub::new(docker_api, shutdown.clone(), tasks.clone()),
            stream_gate: solodock::api::streams::StreamGate::default(),
            redactor,
            state_directory: state_directory.clone(),
            shutdown,
            stream_tasks: tasks,
            m3: None,
        };
        state.m3 = Some(Arc::new(M3Services {
            store: store.clone(),
            database,
            allowed_bind_roots: vec![bind_root],
            runtime_directory: runtime_directory.clone(),
            idempotency: idempotency.clone(),
            coordinator: AppMutationCoordinator::new(runtime_directory).unwrap(),
            compose,
            compose_capability: capability,
            projection_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_notify: Arc::new(tokio::sync::Notify::new()),
            publication_lock: Arc::new(tokio::sync::Mutex::new(())),
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
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    fn publish_active(&self, app_id: Uuid, release_id: Uuid, image: &str) {
        let metadata = self.store.read_metadata(app_id).unwrap();
        let loaded = solodock::app_store::config_revision::load_verified(
            &self.store.app_directory(app_id),
            metadata.draft_revision,
            &self.idempotency.integrity_key(),
        )
        .unwrap();
        let draft = solodock::domain::normalize_draft(
            loaded.input(
                metadata.slug,
                metadata.display_name,
                metadata.discovery_image_ref,
                metadata.poll_interval_seconds,
            ),
            &loaded.secrets,
            &self.idempotency.fingerprint(b"config"),
            &self.state.m3.as_ref().unwrap().allowed_bind_roots,
        )
        .unwrap();
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
        .unwrap();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
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
        .unwrap();
        solodock::app_store::atomic::AtomicWriter::switch_release_link(
            &self.store.app_directory(app_id),
            "active",
            release_id,
        )
        .unwrap();
        self.catalog.replace(&self.store.scan().unwrap());
    }
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon"]
async fn observes_owned_container_on_isolated_daemon() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    assert!(endpoint.starts_with("tcp://127.0.0.1:") || endpoint.starts_with("tcp://localhost:"));
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
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
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
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
            networks: vec![
                solodock::domain::NetworkInput::OwnedDefault,
                solodock::domain::NetworkInput::External {
                    name: network_name.clone(),
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
        endpoint,
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

    let harness = MutationHarness::new(endpoint, bind_root.clone()).await;
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
