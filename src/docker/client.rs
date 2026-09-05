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

use crate::registry::ManifestDescriptor;

use super::{
    models::{
        ContainerRecord, ContainerStatus, DockerError, DockerErrorKind, DockerNetworkResource,
        DockerReadApi, DockerResource, DockerStream, HealthStatus, ImageRecord, LogChunk,
        LogRequest, LogStreamKind, MountKind, MountProjection, NetworkMember, NetworkProjection,
        NetworkSnapshot, PortProjection, ProbeSnapshot, ProbeStatus, RawDockerEvent, RawStats,
    },
    ownership::{MANAGED_LABEL, valid_container_id},
};

const SOCKET_PATH: &str = "/var/run/docker.sock";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NETWORK_MEMBERS: usize = 256;
const NETWORK_MEMBER_CONCURRENCY: usize = 8;

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
    pub fn image_cleanup(&self) -> BollardImageCleanup {
        BollardImageCleanup(self.clone())
    }
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

#[derive(Clone)]
pub struct BollardImageCleanup(BollardReadClient);

#[async_trait]
impl super::image_cleanup::ImageCleanup for BollardImageCleanup {
    async fn all_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        with_list_deadline(async {
            let docker = self.0.client().await?;
            // No label/status filter: unmanaged and stopped containers protect images too.
            let options = ListContainersOptionsBuilder::default().all(true).build();
            let summaries = docker
                .list_containers(Some(options))
                .await
                .map_err(|error| classify(&error, false))?;
            if summaries.len() > 4096 {
                return Err(DockerError::new(DockerErrorKind::ObservationFailed));
            }
            let mut result = Vec::new();
            for summary in summaries {
                let id = summary
                    .id
                    .filter(|id| valid_container_id(id))
                    .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?;
                // Unlike observation scans, disappearance/incomplete inspect is not skipped.
                result.push(self.0.inspect_container(&id).await?);
            }
            Ok(result)
        })
        .await
    }

    async fn inspect(
        &self,
        id: &super::image_cleanup::ExactImageId,
    ) -> Result<Option<super::image_cleanup::CleanupImage>, DockerError> {
        let docker = self.0.client().await?;
        let raw = match tokio::time::timeout(REQUEST_TIMEOUT, docker.inspect_image(id.as_str()))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
        {
            Ok(value) => value,
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => return Err(classify(&error, false)),
        };
        let size = raw
            .size
            .and_then(|size| u64::try_from(size).ok())
            .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?;
        let mut tags = raw.repo_tags.clone().unwrap_or_default();
        if tags.len() > 1024 {
            return Err(DockerError::new(DockerErrorKind::ObservationFailed));
        }
        tags.sort();
        tags.dedup();
        let mut image = project_image_inspect(raw)?;
        if super::image_cleanup::ExactImageId::parse(&image.id).is_err()
            || image.repo_digests.len() > 1024
        {
            return Err(DockerError::new(DockerErrorKind::ObservationFailed));
        }
        image.repo_digests.sort();
        image.repo_digests.dedup();
        Ok(Some(super::image_cleanup::CleanupImage {
            image,
            reported_size_bytes: size,
            repo_tags: tags,
        }))
    }

    async fn remove(
        &self,
        id: &super::image_cleanup::ExactImageId,
    ) -> Result<super::image_cleanup::RemoveImageResult, DockerError> {
        use super::image_cleanup::RemoveImageResult;
        let docker = self.0.client().await?;
        let options = bollard::query_parameters::RemoveImageOptionsBuilder::default()
            .force(false)
            .noprune(true)
            .build();
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            docker.remove_image(id.as_str(), Some(options), None),
        )
        .await
        .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
        {
            Ok(_)
            | Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(RemoveImageResult::Accepted),
            Err(BollardError::DockerResponseServerError {
                status_code: 409, ..
            }) => Ok(RemoveImageResult::Retained),
            Err(error) => Err(classify(&error, false)),
        }
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

    async fn list_compose_app_containers(
        &self,
        project_name: &str,
    ) -> Result<Vec<ContainerRecord>, DockerError> {
        let operation = async {
            let docker = self.client().await?;
            let filters = HashMap::from([(
                "label".to_string(),
                vec![
                    format!("{}={project_name}", crate::docker::ownership::PROJECT_LABEL),
                    format!("{}=app", crate::docker::ownership::SERVICE_LABEL),
                ],
            )]);
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
        with_list_deadline(operation).await
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

    async fn inspect_volume(&self, name: &str) -> Result<Option<DockerResource>, DockerError> {
        let docker = self.client().await?;
        let result = tokio::time::timeout(REQUEST_TIMEOUT, docker.inspect_volume(name))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?;
        match result {
            Ok(volume) => Ok(Some(DockerResource {
                name: volume.name,
                labels: volume.labels,
            })),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(classify(&error, false)),
        }
    }

    async fn inspect_network(
        &self,
        name: &str,
    ) -> Result<Option<DockerNetworkResource>, DockerError> {
        let docker = self.client().await?;
        let result = tokio::time::timeout(REQUEST_TIMEOUT, docker.inspect_network(name, None))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?;
        match result {
            Ok(network) => Ok(Some(DockerNetworkResource {
                name: network.name.unwrap_or_default(),
                labels: network.labels.unwrap_or_default(),
                driver: network.driver,
                internal: network.internal.unwrap_or(false),
                options: network.options.unwrap_or_default(),
            })),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(classify(&error, false)),
        }
    }

    async fn create_platform_network(&self) -> Result<(), DockerError> {
        use bollard::models::NetworkCreateRequest;
        let docker = self.client().await?;
        let request = NetworkCreateRequest {
            name: crate::domain::PLATFORM_NETWORK_NAME.to_owned(),
            driver: Some("bridge".to_owned()),
            internal: Some(true),
            labels: Some(crate::docker::platform_network::expected_labels()),
            options: Some(HashMap::from([(
                "com.docker.network.bridge.name".to_owned(),
                crate::domain::PLATFORM_BRIDGE_NAME.to_owned(),
            )])),
            ..Default::default()
        };
        tokio::time::timeout(REQUEST_TIMEOUT, docker.create_network(request))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
            .map(|_| ())
            .map_err(|error| classify(&error, false))
    }

    async fn inspect_network_snapshot(
        &self,
        name: &str,
    ) -> Result<Option<NetworkSnapshot>, DockerError> {
        let operation = async {
            let docker = self.client().await?;
            let inspected = match docker.inspect_network(name, None).await {
                Ok(value) => value,
                Err(error) if is_not_found(&error) => return Ok(None),
                Err(error) => return Err(classify(&error, false)),
            };
            let observed_name = inspected.name.unwrap_or_default();
            if observed_name != name {
                return Err(DockerError::new(DockerErrorKind::ObservationFailed));
            }
            let network_id = inspected
                .id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?;
            let labels = inspected.labels.unwrap_or_default();
            let expected_members =
                network_member_signature(inspected.containers.unwrap_or_default())?;
            let member_ids = expected_members
                .iter()
                .map(|(container_id, _)| container_id.clone())
                .collect::<Vec<_>>();
            let client = self.clone();
            let mut members = futures_util::stream::iter(member_ids.into_iter().map(|id| {
                let client = client.clone();
                let network_name = observed_name.clone();
                async move {
                    let container = client
                        .inspect_container(&id)
                        .await
                        .map_err(|_| DockerError::new(DockerErrorKind::ObservationFailed))?;
                    if container.id != id {
                        return Err(DockerError::new(DockerErrorKind::ObservationFailed));
                    }
                    let attachment = container
                        .networks
                        .into_iter()
                        .find(|network| network.name == network_name)
                        .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?;
                    Ok(NetworkMember {
                        container_id: id,
                        dns_names: attachment.aliases,
                    })
                }
            }))
            .buffer_unordered(NETWORK_MEMBER_CONCURRENCY)
            .collect::<Vec<Result<NetworkMember, DockerError>>>()
            .await;
            let mut complete = Vec::with_capacity(members.len());
            for member in members.drain(..) {
                complete.push(member?);
            }
            complete.sort_by(|left, right| left.container_id.cmp(&right.container_id));
            let confirmed = docker
                .inspect_network(name, None)
                .await
                .map_err(|_| DockerError::new(DockerErrorKind::ObservationFailed))?;
            let confirmed_members =
                network_member_signature(confirmed.containers.unwrap_or_default())?;
            if confirmed.name.as_deref() != Some(name)
                || confirmed.id.as_deref() != Some(network_id.as_str())
                || confirmed.labels.unwrap_or_default() != labels
                || confirmed_members != expected_members
            {
                return Err(DockerError::new(DockerErrorKind::ObservationFailed));
            }
            Ok(Some(NetworkSnapshot {
                name: observed_name,
                labels,
                members: complete,
            }))
        };
        tokio::time::timeout(REQUEST_TIMEOUT, operation)
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::ObservationFailed))?
    }

    async fn inspect_image(&self, reference: &str) -> Result<ImageRecord, DockerError> {
        let docker = self.client().await?;
        let image = tokio::time::timeout(REQUEST_TIMEOUT, docker.inspect_image(reference))
            .await
            .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
            .map_err(|error| classify(&error, true))?;
        project_image_inspect(image)
    }
}

async fn with_list_deadline<T>(
    operation: impl std::future::Future<Output = Result<T, DockerError>>,
) -> Result<T, DockerError> {
    tokio::time::timeout(REQUEST_TIMEOUT, operation)
        .await
        .map_err(|_| DockerError::new(DockerErrorKind::Unavailable))?
}

fn network_member_signature(
    containers: HashMap<String, bollard::models::EndpointResource>,
) -> Result<Vec<(String, String)>, DockerError> {
    if containers.len() > MAX_NETWORK_MEMBERS {
        return Err(DockerError::new(DockerErrorKind::ObservationFailed));
    }
    let mut signature = Vec::with_capacity(containers.len());
    for (container_id, endpoint) in containers {
        let endpoint_id = endpoint
            .endpoint_id
            .filter(|value| valid_container_id(value))
            .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?;
        if !valid_container_id(&container_id) {
            return Err(DockerError::new(DockerErrorKind::ObservationFailed));
        }
        signature.push((container_id, endpoint_id));
    }
    signature.sort();
    Ok(signature)
}

fn project_inspect(inspect: ContainerInspectResponse) -> ContainerRecord {
    let manifest_descriptor = project_manifest_descriptor(inspect.image_manifest_descriptor);
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
    let container_name = inspect
        .name
        .as_deref()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_owned();
    let mut networks = network_settings
        .networks
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| {
            let mut aliases = value.dns_names.unwrap_or_default();
            aliases.extend(value.aliases.unwrap_or_default());
            if !container_name.is_empty() {
                aliases.push(container_name.clone());
            }
            aliases.retain(|value| !value.is_empty());
            aliases.sort();
            aliases.dedup();
            NetworkProjection {
                name,
                container_ip: value.ip_address.filter(|value| !value.is_empty()),
                aliases,
            }
        })
        .collect::<Vec<_>>();
    networks.sort_by(|left, right| left.name.cmp(&right.name));
    ContainerRecord {
        id: inspect.id.unwrap_or_default(),
        name: container_name,
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
        manifest_descriptor,
        ports,
        mounts,
        networks,
    }
}

fn project_manifest_descriptor(
    descriptor: Option<bollard::models::OciDescriptor>,
) -> Option<ManifestDescriptor> {
    descriptor.map(|value| {
        let platform = value.platform.unwrap_or_default();
        ManifestDescriptor {
            digest: value.digest,
            os: platform.os,
            architecture: platform.architecture,
            variant: platform.variant,
        }
    })
}

fn project_image_inspect(image: bollard::models::ImageInspect) -> Result<ImageRecord, DockerError> {
    let os = image.os;
    let architecture = image.architecture;
    let variant = image.variant;
    let manifest_descriptor = image.descriptor.map(|value| {
        let platform = value.platform.unwrap_or_default();
        ManifestDescriptor {
            digest: value.digest,
            os: platform.os.or_else(|| os.clone()),
            architecture: platform.architecture.or_else(|| architecture.clone()),
            variant: platform.variant.or_else(|| variant.clone()),
        }
    });
    Ok(ImageRecord {
        id: image
            .id
            .ok_or_else(|| DockerError::new(DockerErrorKind::ObservationFailed))?,
        manifest_descriptor,
        repo_digests: image.repo_digests.unwrap_or_default(),
        os: os.unwrap_or_default(),
        architecture: architecture.unwrap_or_default(),
        variant,
    })
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

fn is_not_found(error: &BollardError) -> bool {
    matches!(
        error,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
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

    #[test]
    fn container_projection_reports_sorted_effective_dns_names() {
        let inspect: ContainerInspectResponse = serde_json::from_value(serde_json::json!({
            "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Name": "/example-app-1",
            "Config": { "Labels": {}, "Image": "example:latest" },
            "State": { "Status": "running" },
            "NetworkSettings": {
                "Networks": {
                    "shared": {
                        "IPAddress": "172.20.0.2",
                        "Aliases": ["api", "example-app-1"],
                        "DNSNames": ["short-id", "api"]
                    }
                }
            }
        }))
        .unwrap();
        let projected = project_inspect(inspect);
        assert_eq!(projected.networks[0].name, "shared");
        assert_eq!(
            projected.networks[0].aliases,
            ["api", "example-app-1", "short-id"]
        );
    }

    #[test]
    fn descriptor_projection_preserves_present_invalid_and_absent_states() {
        let inspect: ContainerInspectResponse = serde_json::from_value(serde_json::json!({
            "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Config": { "Labels": {}, "Image": "example@sha256:manifest" },
            "State": { "Status": "running" },
            "ImageManifestDescriptor": {
                "digest": "sha256:manifest",
                "platform": { "os": "linux", "architecture": "amd64" }
            }
        }))
        .unwrap();
        let projected = project_inspect(inspect);
        assert_eq!(
            projected.manifest_descriptor,
            Some(ManifestDescriptor {
                digest: Some("sha256:manifest".into()),
                os: Some("linux".into()),
                architecture: Some("amd64".into()),
                variant: None,
            })
        );

        let malformed = project_manifest_descriptor(Some(bollard::models::OciDescriptor {
            digest: None,
            platform: None,
            ..Default::default()
        }));
        assert_eq!(malformed, Some(ManifestDescriptor::default()));
        assert_eq!(project_manifest_descriptor(None), None);
    }

    fn image_inspect(value: serde_json::Value) -> ImageRecord {
        project_image_inspect(serde_json::from_value(value).unwrap()).unwrap()
    }

    #[test]
    fn image_inspect_projection_completes_docker_29_descriptor_from_same_response() {
        let projected = image_inspect(serde_json::json!({
            "Id": "sha256:config",
            "RepoDigests": ["postgres@sha256:manifest"],
            "Os": "linux",
            "Architecture": "amd64",
            "Variant": "v3",
            "Descriptor": { "digest": "sha256:manifest" }
        }));

        assert_eq!(projected.os, "linux");
        assert_eq!(projected.architecture, "amd64");
        assert_eq!(projected.variant.as_deref(), Some("v3"));
        assert_eq!(
            projected.manifest_descriptor,
            Some(ManifestDescriptor {
                digest: Some("sha256:manifest".into()),
                os: Some("linux".into()),
                architecture: Some("amd64".into()),
                variant: Some("v3".into()),
            })
        );
    }

    #[test]
    fn image_inspect_projection_preserves_absent_descriptor() {
        let projected = image_inspect(serde_json::json!({
            "Id": "sha256:config",
            "Os": "linux",
            "Architecture": "amd64"
        }));

        assert_eq!(projected.manifest_descriptor, None);
    }

    #[test]
    fn image_inspect_projection_only_completes_missing_platform_fields() {
        let complete = image_inspect(serde_json::json!({
            "Id": "sha256:config",
            "Os": "windows",
            "Architecture": "arm64",
            "Variant": "v9",
            "Descriptor": {
                "digest": "sha256:manifest",
                "platform": { "os": "linux", "architecture": "amd64", "variant": "v3" }
            }
        }));
        assert_eq!(
            complete.manifest_descriptor,
            Some(ManifestDescriptor {
                digest: Some("sha256:manifest".into()),
                os: Some("linux".into()),
                architecture: Some("amd64".into()),
                variant: Some("v3".into()),
            })
        );

        let partial = image_inspect(serde_json::json!({
            "Id": "sha256:config",
            "Os": "linux",
            "Architecture": "amd64",
            "Variant": "v8",
            "Descriptor": {
                "digest": "sha256:manifest",
                "platform": { "os": "freebsd", "variant": "v7" }
            }
        }));
        assert_eq!(
            partial.manifest_descriptor,
            Some(ManifestDescriptor {
                digest: Some("sha256:manifest".into()),
                os: Some("freebsd".into()),
                architecture: Some("amd64".into()),
                variant: Some("v7".into()),
            })
        );
    }

    #[test]
    fn image_inspect_projection_does_not_invent_digest_or_missing_top_level_platform() {
        let projected = image_inspect(serde_json::json!({
            "Id": "sha256:config",
            "Descriptor": {}
        }));

        assert_eq!(projected.os, "");
        assert_eq!(projected.architecture, "");
        assert_eq!(
            projected.manifest_descriptor,
            Some(ManifestDescriptor::default())
        );
    }

    #[test]
    fn network_member_signature_tracks_endpoint_reconnects_and_rejects_missing_identity() {
        let container_id = "a".repeat(64);
        let first = network_member_signature(HashMap::from([(
            container_id.clone(),
            bollard::models::EndpointResource {
                endpoint_id: Some("b".repeat(64)),
                ..Default::default()
            },
        )]))
        .unwrap();
        let reconnected = network_member_signature(HashMap::from([(
            container_id.clone(),
            bollard::models::EndpointResource {
                endpoint_id: Some("c".repeat(64)),
                ..Default::default()
            },
        )]))
        .unwrap();
        assert_ne!(first, reconnected);
        assert!(
            network_member_signature(HashMap::from([(
                container_id,
                bollard::models::EndpointResource::default(),
            )]))
            .is_err()
        );
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
