use std::{sync::Arc, time::Duration};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    docker::{
        models::{ContainerStatus, DockerReadApi, HealthStatus},
        ownership::RELEASE_ID_LABEL,
    },
    domain::HealthPolicy,
};

#[derive(Clone, Debug, Serialize)]
pub struct HealthResult {
    pub outcome: &'static str,
    pub code: Option<&'static str>,
    pub elapsed_seconds: u64,
    pub restart_count: Option<i64>,
    pub exit_code: Option<i64>,
}

#[derive(Clone)]
pub struct HealthVerifier {
    docker: Arc<dyn DockerReadApi>,
    shutdown: CancellationToken,
}

impl HealthVerifier {
    pub fn new(docker: Arc<dyn DockerReadApi>, shutdown: CancellationToken) -> Self {
        Self { docker, shutdown }
    }

    pub async fn verify(
        &self,
        container_id: &str,
        release_id: uuid::Uuid,
        image_id: &str,
        policy: &HealthPolicy,
        deadline: Duration,
    ) -> Result<HealthResult, HealthError> {
        let started = tokio::time::Instant::now();
        let first = self
            .inspect_before(container_id, release_id, image_id, started, deadline)
            .await?;
        let baseline_started = first.started_at.clone();
        let baseline_restarts = first.restart_count;
        let mut running_since = None;
        let stable_required = match policy {
            HealthPolicy::Running {
                stable_window_seconds,
            } => u64::from(*stable_window_seconds),
            HealthPolicy::Disabled { .. } => 5,
            _ => 0,
        };
        loop {
            let container = self
                .inspect_before(container_id, release_id, image_id, started, deadline)
                .await?;
            if container.started_at != baseline_started
                || container.restart_count != baseline_restarts
            {
                return Err(HealthError::Restarted);
            }
            let success = match policy {
                HealthPolicy::Healthy { .. } => match container.health {
                    HealthStatus::Healthy => true,
                    HealthStatus::Starting => false,
                    HealthStatus::Unhealthy => return Err(HealthError::Unhealthy),
                    HealthStatus::None | HealthStatus::Unknown => return Err(HealthError::Missing),
                },
                HealthPolicy::Running { .. } | HealthPolicy::Disabled { .. } => {
                    match container.status {
                        ContainerStatus::Running => {
                            let since = running_since.get_or_insert_with(tokio::time::Instant::now);
                            since.elapsed().as_secs() >= stable_required
                        }
                        ContainerStatus::Exited | ContainerStatus::Dead => {
                            return Err(HealthError::Exited);
                        }
                        ContainerStatus::Paused | ContainerStatus::Removing => {
                            return Err(HealthError::Changed);
                        }
                        _ => {
                            running_since = None;
                            false
                        }
                    }
                }
                HealthPolicy::Completed => match container.status {
                    ContainerStatus::Exited if container.exit_code == Some(0) => true,
                    ContainerStatus::Exited => return Err(HealthError::CompletedNonzero),
                    ContainerStatus::Dead => return Err(HealthError::Exited),
                    _ => false,
                },
            };
            if success {
                return Ok(HealthResult {
                    outcome: "passed",
                    code: None,
                    elapsed_seconds: started.elapsed().as_secs(),
                    restart_count: container.restart_count,
                    exit_code: container.exit_code,
                });
            }
            let sleep = tokio::time::sleep(Duration::from_secs(1));
            tokio::select! {
                () = self.shutdown.cancelled() => return Err(HealthError::Interrupted),
                () = sleep => {}
            }
            if started.elapsed() >= deadline {
                return Err(HealthError::Timeout);
            }
        }
    }

    async fn inspect_before(
        &self,
        id: &str,
        release: uuid::Uuid,
        image: &str,
        started: tokio::time::Instant,
        deadline: Duration,
    ) -> Result<crate::docker::models::ContainerRecord, HealthError> {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .ok_or(HealthError::Timeout)?;
        tokio::select! {
            () = self.shutdown.cancelled() => Err(HealthError::Interrupted),
            result = tokio::time::timeout(remaining, self.inspect_exact(id, release, image)) => {
                result.map_err(|_| HealthError::Timeout)?
            }
        }
    }

    async fn inspect_exact(
        &self,
        id: &str,
        release: uuid::Uuid,
        image: &str,
    ) -> Result<crate::docker::models::ContainerRecord, HealthError> {
        let value = self
            .docker
            .inspect_container(id)
            .await
            .map_err(|_| HealthError::Observation)?;
        if value.id != id
            || value.labels.get(RELEASE_ID_LABEL).map(String::as_str)
                != Some(release.to_string().as_str())
            || value.image_id.as_deref() != Some(image)
        {
            return Err(HealthError::Changed);
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum HealthError {
    #[error("healthcheck missing")]
    Missing,
    #[error("container unhealthy")]
    Unhealthy,
    #[error("health deadline exceeded")]
    Timeout,
    #[error("container exited")]
    Exited,
    #[error("container restarted")]
    Restarted,
    #[error("completed container returned nonzero")]
    CompletedNonzero,
    #[error("container identity changed")]
    Changed,
    #[error("Docker observation failed")]
    Observation,
    #[error("health verification interrupted")]
    Interrupted,
}
impl HealthError {
    pub const fn public_code(self) -> &'static str {
        match self {
            Self::Missing => "HEALTHCHECK_MISSING",
            Self::Unhealthy => "HEALTH_UNHEALTHY",
            Self::Timeout => "HEALTH_TIMEOUT",
            Self::Exited => "CONTAINER_EXITED",
            Self::Restarted => "CONTAINER_RESTARTED",
            Self::CompletedNonzero => "COMPLETED_NONZERO",
            Self::Changed => "CONTAINER_CHANGED",
            Self::Observation => "DOCKER_OBSERVATION_FAILED",
            Self::Interrupted => "DEPLOYMENT_INTERRUPTED",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;
    use futures_util::stream;

    use super::*;
    use crate::docker::models::{
        ContainerRecord, DockerError, DockerErrorKind, DockerReadApi, DockerStream, LogChunk,
        LogRequest, ProbeSnapshot, RawDockerEvent, RawStats,
    };

    struct ScriptedDocker {
        records: Mutex<VecDeque<ContainerRecord>>,
        last: Mutex<Option<ContainerRecord>>,
    }

    #[async_trait]
    impl DockerReadApi for ScriptedDocker {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }
        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            Ok(Vec::new())
        }
        async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
            if let Some(value) = self.records.lock().unwrap().pop_front() {
                *self.last.lock().unwrap() = Some(value.clone());
                Ok(value)
            } else {
                Ok(self.last.lock().unwrap().clone().unwrap())
            }
        }
        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            Ok(Box::pin(stream::empty()))
        }
        async fn logs(
            &self,
            _id: &str,
            _request: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            Ok(Box::pin(stream::empty()))
        }
        async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
            Ok(Box::pin(stream::empty()))
        }
    }

    fn record(release: uuid::Uuid, status: ContainerStatus) -> ContainerRecord {
        ContainerRecord {
            id: "container".into(),
            name: "app".into(),
            labels: std::collections::HashMap::from([(
                RELEASE_ID_LABEL.into(),
                release.to_string(),
            )]),
            status,
            health: HealthStatus::None,
            exit_code: None,
            restart_count: Some(0),
            started_at: Some("stable-start".into()),
            finished_at: None,
            configured_image_ref: None,
            image_id: Some("image".into()),
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn running_window_starts_only_after_continuous_running_observation() {
        let release = uuid::Uuid::new_v4();
        let mut records = VecDeque::new();
        for _ in 0..6 {
            records.push_back(record(release, ContainerStatus::Created));
        }
        for _ in 0..6 {
            records.push_back(record(release, ContainerStatus::Running));
        }
        let verifier = HealthVerifier::new(
            Arc::new(ScriptedDocker {
                records: Mutex::new(records),
                last: Mutex::new(None),
            }),
            CancellationToken::new(),
        );
        let task = tokio::spawn(async move {
            verifier
                .verify(
                    "container",
                    release,
                    "image",
                    &HealthPolicy::Running {
                        stable_window_seconds: 3,
                    },
                    Duration::from_secs(20),
                )
                .await
        });
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "non-running time cannot count as stable"
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(task.await.unwrap().is_ok());
    }
}
