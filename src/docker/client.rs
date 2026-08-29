use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use bollard::{
    API_DEFAULT_VERSION, Docker,
    container::LogOutput,
    errors::Error as BollardError,
    models::{ContainerInspectResponse, ContainerStatsResponse},
    query_parameters::{
        EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder, StatsOptionsBuilder,
    },
};
use bytes::Bytes;
use futures_util::StreamExt;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::{
    models::{
        ContainerRecord, ContainerStatus, DockerError, DockerErrorKind, DockerReadApi,
        DockerStream, HealthStatus, LogChunk, LogRequest, LogStreamKind, MountKind,
        MountProjection, NetworkProjection, PortProjection, ProbeSnapshot, ProbeStatus,
        RawDockerEvent, RawStats,
    },
    ownership::{MANAGED_LABEL, valid_container_id},
};

const SOCKET_PATH: &str = "/var/run/docker.sock";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct BollardReadClient {
    endpoint: Endpoint,
    client: Arc<Mutex<Option<Docker>>>,
}

#[derive(Clone)]
enum Endpoint {
    Unix,
    #[cfg(feature = "docker-e2e")]
    TestHttp(String),
}

impl BollardReadClient {
    pub fn production() -> Self {
        Self {
            endpoint: Endpoint::Unix,
            client: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "docker-e2e")]
    pub fn for_test_http(endpoint: String) -> Self {
        Self {
            endpoint: Endpoint::TestHttp(endpoint),
            client: Arc::new(Mutex::new(None)),
        }
    }

    async fn client(&self) -> Result<Docker, DockerError> {
        let mut guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }
        let client = match &self.endpoint {
            Endpoint::Unix => Docker::connect_with_unix(SOCKET_PATH, 5, API_DEFAULT_VERSION),
            #[cfg(feature = "docker-e2e")]
            Endpoint::TestHttp(endpoint) => {
                Docker::connect_with_http(endpoint, 5, API_DEFAULT_VERSION)
            }
        }
        .map_err(|error| classify(&error, false))?;
        let client = tokio::time::timeout(REQUEST_TIMEOUT, client.negotiate_version())
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
            .map_err(|error| classify(&error, false))?;
        *guard = Some(client.clone());
        Ok(client)
    }

    async fn reset_client(&self) {
        *self.client.lock().await = None;
    }
}

#[async_trait]
impl DockerReadApi for BollardReadClient {
    async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
        let result = async {
            let docker = self.client().await?;
            docker
                .ping()
                .await
                .map_err(|error| classify(&error, false))?;
            let version = docker
                .version()
                .await
                .map_err(|error| classify(&error, false))?;
            let api_version = version
                .api_version
                .clone()
                .ok_or_else(|| DockerError::new(DockerErrorKind::Incompatible))?;
            if !api_at_least_1_41(&api_version) || version.os.as_deref() != Some("linux") {
                return Err(DockerError::new(DockerErrorKind::Incompatible));
            }
            let info = docker
                .info()
                .await
                .map_err(|error| classify(&error, false))?;
            let filters =
                HashMap::from([("label".to_string(), vec![format!("{MANAGED_LABEL}=true")])]);
            let options = ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build();
            docker
                .list_containers(Some(options))
                .await
                .map_err(|error| classify(&error, false))?;
            Ok(ProbeSnapshot {
                status: ProbeStatus::Ready,
                error_code: None,
                server_version: version.version,
                api_version: Some(api_version),
                os: version.os,
                architecture: version.arch,
                observed_at: OffsetDateTime::now_utc(),
                docker_root_directory: info.docker_root_dir,
            })
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, result).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                self.reset_client().await;
                Err(error)
            }
            Err(_) => {
                self.reset_client().await;
                Err(DockerError::new(DockerErrorKind::Unavailable))
            }
        }
    }

    async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        let operation = async {
            let docker = self.client().await?;
            let filters =
                HashMap::from([("label".to_string(), vec![format!("{MANAGED_LABEL}=true")])]);
            let options = ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build();
            let summaries = docker
                .list_containers(Some(options))
                .await
                .map_err(|error| classify(&error, false))?;
            let mut result = Vec::with_capacity(summaries.len());
            for summary in summaries {
                let Some(id) = summary.id else { continue };
                match self.inspect_container(&id).await {
                    Ok(container) => result.push(container),
                    Err(error) if error.kind == DockerErrorKind::ContainerChanged => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(result)
        };
        match with_list_deadline(operation).await {
            Ok(result) => Ok(result),
            Err(error) => {
                if error.kind != DockerErrorKind::Unavailable {
                    return Err(error);
                }
                self.reset_client().await;
                Err(error)
            }
        }
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerRecord, DockerError> {
        let docker = self.client().await?;
        let inspect = tokio::time::timeout(REQUEST_TIMEOUT, docker.inspect_container(id, None))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
            .map_err(|error| classify(&error, true))?;
        let projected = project_inspect(inspect);
        if projected.id != id || !valid_container_id(&projected.id) {
            return Err(DockerError::new(DockerErrorKind::ContainerChanged));
        }
        Ok(projected)
    }

    async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
        let docker = self.client().await?;
        let filters = HashMap::from([
            ("type".to_string(), vec!["container".to_string()]),
            ("label".to_string(), vec![format!("{MANAGED_LABEL}=true")]),
        ]);
        let options = EventsOptionsBuilder::default().filters(&filters).build();
        let stream = docker.events(Some(options)).map(|item| {
            let event = item.map_err(|error| classify(&error, false))?;
            let actor = event.actor.unwrap_or_default();
            let labels = actor.attributes.unwrap_or_default();
            let exit_code = labels.get("exitCode").and_then(|value| value.parse().ok());
            let occurred_at = event
                .time
                .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
                .unwrap_or_else(OffsetDateTime::now_utc);
            Ok(RawDockerEvent {
                container_id: actor.id.unwrap_or_default(),
                action: event.action.unwrap_or_default(),
                labels,
                occurred_at,
                exit_code,
            })
        });
        Ok(Box::pin(stream))
    }

    async fn logs(
        &self,
        id: &str,
        request: LogRequest,
    ) -> Result<DockerStream<LogChunk>, DockerError> {
        let docker = self.client().await?;
        let mut builder = LogsOptionsBuilder::default();
        let tail = request.tail.to_string();
        builder = builder
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .follow(true)
            .tail(&tail);
        if let Some(since) = request.since_seconds {
            let since = i32::try_from(since)
                .map_err(|_| DockerError::new(DockerErrorKind::ObservationFailed))?;
            builder = builder.since(since);
        }
        let stream = docker.logs(id, Some(builder.build())).map(|item| {
            let output = item.map_err(|error| classify(&error, true))?;
            let (stream, bytes) = match output {
                LogOutput::StdOut { message } => (LogStreamKind::Stdout, message),
                LogOutput::StdErr { message } => (LogStreamKind::Stderr, message),
                LogOutput::StdIn { .. } | LogOutput::Console { .. } => {
                    (LogStreamKind::Unknown, Bytes::new())
                }
            };
            Ok(LogChunk { stream, bytes })
        });
        Ok(Box::pin(stream))
    }

    async fn stats(&self, id: &str) -> Result<DockerStream<RawStats>, DockerError> {
        let docker = self.client().await?;
        let options = StatsOptionsBuilder::default()
            .stream(true)
            .one_shot(false)
            .build();
        let stream = docker.stats(id, Some(options)).map(|item| {
            item.map(project_stats)
                .map_err(|error| classify(&error, true))
        });
        Ok(Box::pin(stream))
    }
}

async fn with_list_deadline<T>(
    operation: impl std::future::Future<Output = Result<T, DockerError>>,
) -> Result<T, DockerError> {
    tokio::time::timeout(REQUEST_TIMEOUT, operation)
        .await
        .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
}

fn project_inspect(inspect: ContainerInspectResponse) -> ContainerRecord {
    let state = inspect.state.unwrap_or_default();
    let config = inspect.config.unwrap_or_default();
    let network_settings = inspect.network_settings.unwrap_or_default();
    let ports = network_settings
        .ports
        .unwrap_or_default()
        .into_iter()
        .flat_map(|(key, bindings)| {
            let (port, protocol) = parse_port_key(&key)?;
            Some(
                bindings
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(move |binding| {
                        let host_ip = binding.host_ip?;
                        let parsed: std::net::IpAddr = host_ip.parse().ok()?;
                        if !parsed.is_loopback() {
                            return None;
                        }
                        Some(PortProjection {
                            container_port: port,
                            protocol: protocol.clone(),
                            host_ip,
                            host_port: binding.host_port?.parse().ok()?,
                        })
                    }),
            )
        })
        .flatten()
        .collect();
    let mounts = inspect
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mount| {
            let kind = match mount.typ.as_deref()? {
                "volume" => MountKind::Volume,
                "bind" => MountKind::Bind,
                "tmpfs" => MountKind::Tmpfs,
                _ => return None,
            };
            let source = match kind {
                MountKind::Volume => mount.name,
                MountKind::Bind => mount.source,
                MountKind::Tmpfs => None,
            };
            Some(MountProjection {
                kind,
                source,
                destination: mount.destination?,
                read_only: !mount.rw.unwrap_or(false),
            })
        })
        .collect();
    let networks = network_settings
        .networks
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| NetworkProjection {
            name,
            container_ip: value.ip_address.filter(|value| !value.is_empty()),
        })
        .collect();
    ContainerRecord {
        id: inspect.id.unwrap_or_default(),
        name: inspect
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned(),
        labels: config.labels.unwrap_or_default(),
        status: map_status(state.status.map(|value| value.to_string()).as_deref()),
        health: map_health(
            state
                .health
                .and_then(|value| value.status.map(|status| status.to_string()))
                .as_deref(),
        ),
        exit_code: state.exit_code,
        restart_count: inspect.restart_count,
        started_at: state.started_at.filter(|value| nonzero_time(value)),
        finished_at: state.finished_at.filter(|value| nonzero_time(value)),
        configured_image_ref: config.image,
        image_id: inspect.image,
        ports,
        mounts,
        networks,
    }
}

fn project_stats(stats: ContainerStatsResponse) -> RawStats {
    let cpu = stats.cpu_stats.unwrap_or_default();
    let previous = stats.precpu_stats.unwrap_or_default();
    let memory = stats.memory_stats.unwrap_or_default();
    RawStats {
        observed_at: OffsetDateTime::now_utc(),
        cpu_total: cpu.cpu_usage.and_then(|value| value.total_usage),
        previous_cpu_total: previous.cpu_usage.and_then(|value| value.total_usage),
        system_cpu_total: cpu.system_cpu_usage,
        previous_system_cpu_total: previous.system_cpu_usage,
        online_cpus: cpu.online_cpus.map(u64::from),
        memory_usage: memory.usage,
        memory_limit: memory.limit,
        networks: stats
            .networks
            .unwrap_or_default()
            .into_values()
            .map(|value| (value.rx_bytes, value.tx_bytes))
            .collect(),
    }
}

fn map_status(value: Option<&str>) -> ContainerStatus {
    match value {
        Some("created") => ContainerStatus::Created,
        Some("running") => ContainerStatus::Running,
        Some("paused") => ContainerStatus::Paused,
        Some("restarting") => ContainerStatus::Restarting,
        Some("removing") => ContainerStatus::Removing,
        Some("exited") => ContainerStatus::Exited,
        Some("dead") => ContainerStatus::Dead,
        _ => ContainerStatus::Unknown,
    }
}

fn map_health(value: Option<&str>) -> HealthStatus {
    match value {
        None | Some("none") => HealthStatus::None,
        Some("starting") => HealthStatus::Starting,
        Some("healthy") => HealthStatus::Healthy,
        Some("unhealthy") => HealthStatus::Unhealthy,
        _ => HealthStatus::Unknown,
    }
}

fn parse_port_key(value: &str) -> Option<(u16, String)> {
    let (port, protocol) = value.split_once('/')?;
    Some((port.parse().ok()?, protocol.to_owned()))
}

fn nonzero_time(value: &str) -> bool {
    !value.is_empty() && !value.starts_with("0001-")
}

fn api_at_least_1_41(value: &str) -> bool {
    value
        .split_once('.')
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .is_some_and(|version| version >= (1, 41))
}

fn classify(error: &BollardError, container_context: bool) -> DockerError {
    let kind = if error_chain_has_permission_denied(error) {
        DockerErrorKind::PermissionDenied
    } else {
        match error {
            BollardError::DockerResponseServerError {
                status_code: 403, ..
            } => DockerErrorKind::PermissionDenied,
            BollardError::DockerResponseServerError {
                status_code: 404, ..
            } if container_context => DockerErrorKind::ContainerChanged,
            BollardError::SocketNotFoundError(_) | BollardError::RequestTimeoutError => {
                DockerErrorKind::Unavailable
            }
            BollardError::IOError { err } if err.kind() == std::io::ErrorKind::PermissionDenied => {
                DockerErrorKind::PermissionDenied
            }
            BollardError::IOError { .. }
            | BollardError::HyperLegacyError { .. }
            | BollardError::HyperResponseError { .. } => DockerErrorKind::Unavailable,
            _ => DockerErrorKind::ObservationFailed,
        }
    };
    DockerError::new(kind)
}

fn error_chain_has_permission_denied(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(value) = current {
        if value
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            return true;
        }
        current = value.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_unknown_values_are_safely_classified() {
        assert!(api_at_least_1_41("1.41"));
        assert!(api_at_least_1_41("1.53"));
        assert!(!api_at_least_1_41("1.40"));
        assert!(!api_at_least_1_41("garbage"));
        assert_eq!(map_status(Some("future")), ContainerStatus::Unknown);
        assert_eq!(map_health(Some("future")), HealthStatus::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn list_deadline_bounds_multiple_slow_inspections_as_one_operation() {
        let operation = tokio::spawn(async {
            with_list_deadline(async {
                for _ in 0..3 {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
                Ok::<_, DockerError>(())
            })
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert!(!operation.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            operation.await.unwrap(),
            Err(DockerError {
                kind: DockerErrorKind::Unavailable
            })
        ));
    }
}
