use std::{collections::HashMap, pin::Pin};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

pub type DockerStream<T> = Pin<Box<dyn Stream<Item = Result<T, DockerError>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Starting,
    Ready,
    Unavailable,
    PermissionDenied,
    Incompatible,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeSnapshot {
    pub status: ProbeStatus,
    pub error_code: Option<&'static str>,
    pub server_version: Option<String>,
    pub api_version: Option<String>,
    pub os: Option<String>,
    pub architecture: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    #[serde(skip)]
    pub docker_root_directory: Option<String>,
}

impl ProbeSnapshot {
    pub fn starting() -> Self {
        Self {
            status: ProbeStatus::Starting,
            error_code: Some("DOCKER_UNAVAILABLE"),
            server_version: None,
            api_version: None,
            os: None,
            architecture: None,
            observed_at: OffsetDateTime::now_utc(),
            docker_root_directory: None,
        }
    }

    pub fn failed(error: &DockerError) -> Self {
        Self {
            status: error.probe_status(),
            error_code: Some(error.public_code()),
            server_version: None,
            api_version: None,
            os: None,
            architecture: None,
            observed_at: OffsetDateTime::now_utc(),
            docker_root_directory: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockerErrorKind {
    Unavailable,
    PermissionDenied,
    Incompatible,
    ContainerChanged,
    ObservationFailed,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("Docker observation failed ({kind:?})")]
pub struct DockerError {
    pub kind: DockerErrorKind,
}

impl DockerError {
    pub const fn new(kind: DockerErrorKind) -> Self {
        Self { kind }
    }

    pub const fn public_code(&self) -> &'static str {
        match self.kind {
            DockerErrorKind::Unavailable => "DOCKER_UNAVAILABLE",
            DockerErrorKind::PermissionDenied => "DOCKER_PERMISSION_DENIED",
            DockerErrorKind::Incompatible => "DOCKER_API_INCOMPATIBLE",
            DockerErrorKind::ContainerChanged => "CONTAINER_CHANGED",
            DockerErrorKind::ObservationFailed => "DOCKER_OBSERVATION_FAILED",
        }
    }

    pub const fn probe_status(&self) -> ProbeStatus {
        match self.kind {
            DockerErrorKind::PermissionDenied => ProbeStatus::PermissionDenied,
            DockerErrorKind::Incompatible => ProbeStatus::Incompatible,
            DockerErrorKind::Unavailable
            | DockerErrorKind::ContainerChanged
            | DockerErrorKind::ObservationFailed => ProbeStatus::Unavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerStatus {
    Missing,
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    Exited,
    Dead,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    None,
    Starting,
    Healthy,
    Unhealthy,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    Volume,
    Bind,
    Tmpfs,
}

#[derive(Clone, Debug, Serialize)]
pub struct PortProjection {
    pub container_port: u16,
    pub protocol: String,
    pub host_ip: String,
    pub host_port: u16,
}

#[derive(Clone, Debug, Serialize)]
pub struct MountProjection {
    pub kind: MountKind,
    pub source: Option<String>,
    pub destination: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetworkProjection {
    pub name: String,
    pub container_ip: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ContainerRecord {
    pub id: String,
    pub name: String,
    pub labels: HashMap<String, String>,
    pub status: ContainerStatus,
    pub health: HealthStatus,
    pub exit_code: Option<i64>,
    pub restart_count: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub configured_image_ref: Option<String>,
    pub image_id: Option<String>,
    pub ports: Vec<PortProjection>,
    pub mounts: Vec<MountProjection>,
    pub networks: Vec<NetworkProjection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContainerProjection {
    pub id: String,
    pub name: String,
    pub status: ContainerStatus,
    pub health: HealthStatus,
    pub exit_code: Option<i64>,
    pub restart_count: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub configured_image_ref: Option<String>,
    pub image_id: Option<String>,
    pub ports: Vec<PortProjection>,
    pub mounts: Vec<MountProjection>,
    pub networks: Vec<NetworkProjection>,
}

impl From<&ContainerRecord> for ContainerProjection {
    fn from(value: &ContainerRecord) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            status: value.status,
            health: value.health,
            exit_code: value.exit_code,
            restart_count: value.restart_count,
            started_at: value.started_at.clone(),
            finished_at: value.finished_at.clone(),
            configured_image_ref: value.configured_image_ref.clone(),
            image_id: value.image_id.clone(),
            ports: value.ports.clone(),
            mounts: value.mounts.clone(),
            networks: value.networks.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawDockerEvent {
    pub container_id: String,
    pub action: String,
    pub labels: HashMap<String, String>,
    pub occurred_at: OffsetDateTime,
    pub exit_code: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppEvent {
    pub id: String,
    pub kind: EventKind,
    pub app_id: Uuid,
    pub container_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    pub exit_code: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Created,
    Started,
    Stopped,
    Died,
    Destroyed,
    Paused,
    Unpaused,
    Restarted,
    HealthChanged,
    Renamed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStreamKind {
    Stdout,
    Stderr,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct LogChunk {
    pub stream: LogStreamKind,
    pub bytes: Bytes,
}

#[derive(Clone, Debug)]
pub struct LogRequest {
    pub tail: usize,
    pub since_seconds: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEvent {
    pub timestamp: String,
    pub stream: LogStreamKind,
    pub message: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct RawStats {
    pub observed_at: OffsetDateTime,
    pub cpu_total: Option<u64>,
    pub previous_cpu_total: Option<u64>,
    pub system_cpu_total: Option<u64>,
    pub previous_system_cpu_total: Option<u64>,
    pub online_cpus: Option<u64>,
    pub memory_usage: Option<u64>,
    pub memory_limit: Option<u64>,
    pub networks: Vec<(Option<u64>, Option<u64>)>,
}

impl Default for RawStats {
    fn default() -> Self {
        Self {
            observed_at: OffsetDateTime::UNIX_EPOCH,
            cpu_total: None,
            previous_cpu_total: None,
            system_cpu_total: None,
            previous_system_cpu_total: None,
            online_cpus: None,
            memory_usage: None,
            memory_limit: None,
            networks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsSample {
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub memory_percent: Option<f64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}

impl Default for StatsSample {
    fn default() -> Self {
        Self {
            observed_at: OffsetDateTime::UNIX_EPOCH,
            cpu_percent: None,
            memory_usage_bytes: None,
            memory_limit_bytes: None,
            memory_percent: None,
            network_rx_bytes: None,
            network_tx_bytes: None,
        }
    }
}

#[async_trait]
pub trait DockerReadApi: Send + Sync {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError>;
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError>;
    async fn inspect_container(&self, id: &str) -> Result<ContainerRecord, DockerError>;
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError>;
    async fn logs(
        &self,
        id: &str,
        request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError>;
    async fn stats(&self, id: &str) -> Result<DockerStream<RawStats>, DockerError>;
}

pub struct UnavailableDocker;

#[async_trait]
impl DockerReadApi for UnavailableDocker {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn logs(
        &self,
        _id: &str,
        _request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
}
