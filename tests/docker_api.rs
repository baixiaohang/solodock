mod support;

use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use solodock::{
    AppState,
    app_store::recovery::{RecoveredApp, RecoveryReport},
    auth::AuthService,
    db::Database,
    docker::{
        AppCatalog, DockerObserver,
        events::DockerEventHub,
        logs::{SecretProvider, SecretRedactor},
        models::{
            ContainerRecord, ContainerStatus, DockerError, DockerErrorKind, DockerReadApi,
            DockerStream, HealthStatus, LogChunk, LogRequest, MountKind, MountProjection,
            ProbeSnapshot, ProbeStatus, RawDockerEvent, RawStats,
        },
        ownership::*,
        probe::DockerSupervisor,
        stats::StatsHub,
    },
    router,
};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct FakeDocker {
    containers: Arc<Vec<ContainerRecord>>,
    io_calls: Arc<AtomicUsize>,
    stats_error: Option<DockerErrorKind>,
    logs_pending: bool,
}

struct TestSecrets;

impl SecretProvider for TestSecrets {
    fn known_secrets(&self) -> Vec<Vec<u8>> {
        vec![b"managed-secret".to_vec()]
    }
}

#[async_trait]
impl DockerReadApi for FakeDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        Ok(ready_probe())
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        self.io_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.containers.as_ref().clone())
    }
    async fn inspect_container(&self, id: &str) -> Result<ContainerRecord, DockerError> {
        self.io_calls.fetch_add(1, Ordering::SeqCst);
        self.containers
            .iter()
            .find(|container| container.id == id)
            .cloned()
            .ok_or_else(|| DockerError::new(DockerErrorKind::ContainerChanged))
    }
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        self.io_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::pending()))
    }
    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        self.io_calls.fetch_add(1, Ordering::SeqCst);
        if self.logs_pending {
            return Ok(Box::pin(stream::pending()));
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(LogChunk {
                stream: solodock::docker::models::LogStreamKind::Stdout,
                bytes: Bytes::from_static(b"2026-08-28T00:00:00Z managed-"),
            }),
            Ok(LogChunk {
                stream: solodock::docker::models::LogStreamKind::Stdout,
                bytes: Bytes::from_static(b"secret\n"),
            }),
        ])))
    }
    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        self.io_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(kind) = self.stats_error {
            return Err(DockerError::new(kind));
        }
        Ok(Box::pin(stream::pending()))
    }
}

struct Harness {
    _root: TempDir,
    app: axum::Router,
    auth: AuthService,
    stream_gate: solodock::api::streams::StreamGate,
    shutdown: CancellationToken,
    stream_tasks: TaskTracker,
    io_calls: Arc<AtomicUsize>,
    cookie: String,
    app_id: Uuid,
}

impl Harness {
    async fn new(ready: bool, image_matches: bool) -> Self {
        Self::with_stream_behavior(ready, image_matches, None, false).await
    }

    async fn with_stream_behavior(
        ready: bool,
        image_matches: bool,
        stats_error: Option<DockerErrorKind>,
        logs_pending: bool,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let test_auth =
            support::seed_authenticated_session(&database, root.path().join("bootstrap.token"))
                .await;
        let auth = test_auth.service;
        let cookie = test_auth.cookie;

        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let expected_image = format!("registry.example/app@sha256:{}", "a".repeat(64));
        let report = RecoveryReport {
            valid_apps: vec![RecoveredApp {
                app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                project_name: "solodock-example".into(),
                active_release_id: Some(release_id),
                active_image_ref: Some(expected_image.clone()),
                active_config_revision: None,
                active_config_sha256: None,
                active_network_plan: None,
                pending_release_id: None,
                pending_image_ref: None,
                pending_config_revision: None,
                pending_network_plan: None,
                discovery_image_ref: None,
                draft_revision: None,
                draft_config_sha256: None,
                desired_state: solodock::domain::DesiredState::Stopped,
                auto_deploy_enabled: false,
                poll_interval_seconds: 300,
                last_operation_id: None,
                draft: None,
                source_updated_at: OffsetDateTime::UNIX_EPOCH,
            }],
            issues: Vec::new(),
        };
        let labels = HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (SCHEMA_LABEL.into(), "1".into()),
            (APP_ID_LABEL.into(), app_id.to_string()),
            (RELEASE_ID_LABEL.into(), release_id.to_string()),
            (PROJECT_LABEL.into(), "solodock-example".into()),
            (SERVICE_LABEL.into(), "app".into()),
            (ONEOFF_LABEL.into(), "False".into()),
            ("secret.canary".into(), "DO_NOT_EXPOSE".into()),
        ]);
        let container = ContainerRecord {
            id: "a".repeat(64),
            name: "solodock-example-app-1".into(),
            labels,
            status: ContainerStatus::Running,
            health: HealthStatus::Healthy,
            exit_code: Some(0),
            restart_count: Some(1),
            started_at: Some("2026-08-28T00:00:00Z".into()),
            finished_at: None,
            configured_image_ref: Some(if image_matches {
                expected_image
            } else {
                "registry.example/app@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
            }),
            image_id: Some(format!("sha256:{}", "c".repeat(64))),
            manifest_descriptor: None,
            ports: Vec::new(),
            mounts: vec![MountProjection {
                kind: MountKind::Volume,
                source: Some("example-data".into()),
                destination: "/data".into(),
                read_only: false,
            }],
            networks: Vec::new(),
        };
        let mut foreign = container.clone();
        foreign.id = "f".repeat(64);
        foreign.name = "FOREIGN_CONTAINER_MUST_NOT_APPEAR".into();
        foreign.labels.remove(MANAGED_LABEL);
        let io_calls = Arc::new(AtomicUsize::new(0));
        let api: Arc<dyn DockerReadApi> = Arc::new(FakeDocker {
            containers: Arc::new(vec![container, foreign]),
            io_calls: io_calls.clone(),
            stats_error,
            logs_pending,
        });
        let catalog = AppCatalog::from_recovery(&report);
        let supervisor = if ready {
            DockerSupervisor::from_snapshot(ready_probe())
        } else {
            DockerSupervisor::from_snapshot(ProbeSnapshot::failed(&DockerError::new(
                DockerErrorKind::Unavailable,
            )))
        };
        let stream_gate = solodock::api::streams::StreamGate::default();
        let shutdown = CancellationToken::new();
        let stream_tasks = TaskTracker::new();
        let state = AppState {
            auth: auth.clone(),
            public_origin: "https://solodock.example.com".into(),
            observer: DockerObserver::new(api.clone(), catalog, supervisor),
            events: DockerEventHub::new(),
            stats: StatsHub::new(api, shutdown.clone(), stream_tasks.clone()),
            stream_gate: stream_gate.clone(),
            redactor: SecretRedactor::new(&TestSecrets),
            state_directory: root.path().to_owned(),
            shutdown: shutdown.clone(),
            stream_tasks: stream_tasks.clone(),
            m3: None,
            m4: None,
            webhooks: None,
        };
        Self {
            _root: root,
            app: router(state),
            auth,
            stream_gate,
            shutdown,
            stream_tasks,
            io_calls,
            cookie,
            app_id,
        }
    }

    async fn get(&self, uri: &str, authenticated: bool) -> axum::response::Response {
        let mut request = Request::builder()
            .uri(uri)
            .extension(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
        if authenticated {
            request = request.header(header::COOKIE, &self.cookie);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
}

fn ready_probe() -> ProbeSnapshot {
    ProbeSnapshot {
        status: ProbeStatus::Ready,
        error_code: None,
        server_version: Some("28.0".into()),
        api_version: Some("1.47".into()),
        os: Some("linux".into()),
        architecture: Some("x86_64".into()),
        observed_at: OffsetDateTime::UNIX_EPOCH,
        docker_root_directory: None,
    }
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn read_api_requires_session_and_exposes_only_allowlisted_projection() {
    let harness = Harness::new(true, false).await;
    let unauthenticated = harness.get("/api/v1/apps", false).await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(unauthenticated.headers()[header::CACHE_CONTROL], "no-store");
    let response = harness.get("/api/v1/apps", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = json_body(response).await;
    let encoded = body.to_string();
    assert!(!encoded.contains("DO_NOT_EXPOSE"));
    assert!(!encoded.contains("secret.canary"));
    assert!(!encoded.contains("FOREIGN_CONTAINER_MUST_NOT_APPEAR"));
    assert!(encoded.contains("IMAGE_REF_MISMATCH"));

    let detail = harness
        .get(&format!("/api/v1/apps/{}", harness.app_id), true)
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail = json_body(detail).await;
    assert_eq!(detail["actual"]["mounts"][0]["source"], "example-data");

    let drift = json_body(harness.get("/api/v1/system/drift", true).await).await;
    assert_eq!(drift["complete"], true);
    assert_eq!(drift["issues"][0]["code"], "IMAGE_REF_MISMATCH");
}

#[tokio::test]
async fn docker_unavailable_keeps_catalog_and_health_available_but_rejects_streams() {
    let harness = Harness::new(false, true).await;
    let apps = json_body(harness.get("/api/v1/apps", true).await).await;
    assert_eq!(apps["docker_status"], "unavailable");
    assert!(apps["apps"][0]["actual"].is_null());
    assert_eq!(
        apps["apps"][0]["drift_codes"],
        json!(["DOCKER_UNAVAILABLE"])
    );
    let health = json_body(harness.get("/api/v1/system/health", true).await).await;
    assert_eq!(health["status"], "degraded");
    assert!(health["memory"]["total_bytes"].as_u64().unwrap() > 0);
    assert!(health["memory"]["available_bytes"].as_u64().unwrap() > 0);
    assert!(health["memory"]["used_percent"].as_f64().unwrap() >= 0.0);
    let logs = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    assert_eq!(logs.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(logs.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(json_body(logs).await["code"], "DOCKER_UNAVAILABLE");
}

#[tokio::test]
async fn revoked_session_closes_sse_on_the_next_heartbeat_and_releases_permit() {
    let harness = Harness::new(true, true).await;
    let response = harness
        .get(&format!("/api/v1/apps/{}/events", harness.app_id), true)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(harness.stream_gate.active(), 1);
    tokio::time::pause();
    let body_task =
        tokio::spawn(async move { response.into_body().collect().await.unwrap().to_bytes() });
    tokio::task::yield_now().await;
    harness.auth.revoke_all(Uuid::new_v4()).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(16)).await;
    tokio::time::resume();
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), body_task)
        .await
        .expect("revoked SSE session must close after the heartbeat")
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("SESSION_EXPIRED"));
    assert_eq!(harness.stream_gate.active(), 0);
}

#[tokio::test]
async fn logs_sse_redacts_known_secret_across_docker_chunks() {
    let harness = Harness::new(true, true).await;
    let response = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains("managed-secret"));
}

#[tokio::test]
async fn full_stream_gate_rejects_before_any_docker_io() {
    let harness = Harness::with_stream_behavior(true, true, None, true).await;
    let first = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    let second = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(harness.stream_gate.active(), 2);
    let calls_before_rejection = harness.io_calls.load(Ordering::SeqCst);

    let response = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(response).await["code"], "STREAM_LIMIT_REACHED");
    assert_eq!(
        harness.io_calls.load(Ordering::SeqCst),
        calls_before_rejection
    );
    drop(first);
    drop(second);
    tokio::task::yield_now().await;
    assert_eq!(harness.stream_gate.active(), 0);
}

#[tokio::test]
async fn initial_stats_failure_closes_stream_and_releases_permit() {
    let harness =
        Harness::with_stream_behavior(true, true, Some(DockerErrorKind::PermissionDenied), false)
            .await;
    let response = harness
        .get(&format!("/api/v1/apps/{}/stats", harness.app_id), true)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        response.into_body().collect(),
    )
    .await
    .expect("terminal stats error closes SSE")
    .unwrap()
    .to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("DOCKER_PERMISSION_DENIED"));
    assert_eq!(harness.stream_gate.active(), 0);
}

#[tokio::test]
async fn global_shutdown_collects_pending_log_producer_and_connection() {
    let harness = Harness::with_stream_behavior(true, true, None, true).await;
    let response = harness
        .get(&format!("/api/v1/apps/{}/logs", harness.app_id), true)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(harness.stream_gate.active(), 1);
    let body = tokio::spawn(async move { response.into_body().collect().await });
    tokio::task::yield_now().await;

    harness.shutdown.cancel();
    harness.stream_tasks.close();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        harness.stream_tasks.wait(),
    )
    .await
    .expect("pending producer exits on global shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(1), body)
        .await
        .expect("SSE body exits on global shutdown")
        .unwrap()
        .unwrap();
    assert_eq!(harness.stream_gate.active(), 0);
}
