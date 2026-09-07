use std::{sync::Arc, time::Duration};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    docker::{
        models::{ContainerRecord, ContainerStatus, DockerReadApi, HealthStatus},
        ownership::RELEASE_ID_LABEL,
    },
    domain::HealthPolicy,
    registry::ImageIdentity,
};

#[derive(Clone, Debug, Serialize)]
pub struct HealthResult {
    pub outcome: &'static str,
    pub code: Option<&'static str>,
    pub elapsed_seconds: u64,
    pub restart_count: Option<i64>,
    pub exit_code: Option<i64>,
    #[serde(skip)]
    verified_container: ContainerRecord,
}

impl HealthResult {
    pub(crate) fn recheck(
        &self,
        container: &ContainerRecord,
        policy: &HealthPolicy,
    ) -> Result<(), HealthError> {
        if container.id != self.verified_container.id {
            return Err(HealthError::Changed);
        }
        if container.started_at != self.verified_container.started_at
            || container.restart_count != self.verified_container.restart_count
        {
            return Err(HealthError::Restarted);
        }
        if policy_ready(container, policy)? {
            Ok(())
        } else {
            Err(HealthError::NotReady)
        }
    }
}

// Preserve the existing startup allowance and budget the full configured observation.
pub(crate) fn health_deadline(policy: &HealthPolicy) -> Duration {
    const STARTUP_SECONDS: u64 = 300;
    const OBSERVATION_MARGIN_SECONDS: u64 = 10;
    let seconds = match policy {
        HealthPolicy::Running {
            stable_window_seconds,
        } => STARTUP_SECONDS + u64::from(*stable_window_seconds) + OBSERVATION_MARGIN_SECONDS,
        HealthPolicy::Healthy { http: Some(http) } => STARTUP_SECONDS.max(
            u64::from(http.start_period_seconds)
                + u64::from(http.retries)
                    * (u64::from(http.interval_seconds) + u64::from(http.timeout_seconds))
                + OBSERVATION_MARGIN_SECONDS,
        ),
        _ => STARTUP_SECONDS,
    };
    Duration::from_secs(seconds)
}

pub(crate) fn policy_ready(
    container: &ContainerRecord,
    policy: &HealthPolicy,
) -> Result<bool, HealthError> {
    if matches!(policy, HealthPolicy::Completed) {
        return match container.status {
            ContainerStatus::Exited if container.exit_code == Some(0) => Ok(true),
            ContainerStatus::Exited => Err(HealthError::CompletedNonzero),
            ContainerStatus::Dead => Err(HealthError::Exited),
            _ => Ok(false),
        };
    }
    match container.status {
        ContainerStatus::Exited | ContainerStatus::Dead => return Err(HealthError::Exited),
        ContainerStatus::Paused | ContainerStatus::Removing => return Err(HealthError::NotReady),
        ContainerStatus::Running => {}
        _ => return Ok(false),
    }
    if matches!(policy, HealthPolicy::Healthy { .. }) {
        match container.health {
            HealthStatus::Healthy => Ok(true),
            HealthStatus::Starting => Ok(false),
            HealthStatus::Unhealthy => Err(HealthError::Unhealthy),
            HealthStatus::None | HealthStatus::Unknown => Err(HealthError::Missing),
        }
    } else {
        Ok(true)
    }
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
        configured_image_ref: &str,
        image_identity: &ImageIdentity,
        policy: &HealthPolicy,
        deadline: Duration,
    ) -> Result<HealthResult, HealthError> {
        let started = tokio::time::Instant::now();
        let first = self
            .inspect_before(
                container_id,
                release_id,
                configured_image_ref,
                image_identity,
                started,
                deadline,
            )
            .await?;
        let mut baseline_started = first.started_at.clone();
        let mut awaiting_first_start = first.status == ContainerStatus::Created
            && first.started_at.is_none()
            && first.restart_count == Some(0);
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
                .inspect_before(
                    container_id,
                    release_id,
                    configured_image_ref,
                    image_identity,
                    started,
                    deadline,
                )
                .await?;
            if awaiting_first_start
                && container.started_at.is_some()
                && matches!(
                    container.status,
                    ContainerStatus::Running | ContainerStatus::Exited
                )
                && container.restart_count == Some(0)
            {
                baseline_started = container.started_at.clone();
                awaiting_first_start = false;
            }
            if container.started_at != baseline_started
                || container.restart_count != baseline_restarts
            {
                return Err(HealthError::Restarted);
            }
            let ready = policy_ready(&container, policy)?;
            let success = if ready {
                let since = running_since.get_or_insert_with(tokio::time::Instant::now);
                since.elapsed().as_secs() >= stable_required
            } else {
                running_since = None;
                false
            };
            if success {
                return Ok(HealthResult {
                    outcome: "passed",
                    code: None,
                    elapsed_seconds: started.elapsed().as_secs(),
                    restart_count: container.restart_count,
                    exit_code: container.exit_code,
                    verified_container: container,
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
        configured_image_ref: &str,
        image_identity: &ImageIdentity,
        started: tokio::time::Instant,
        deadline: Duration,
    ) -> Result<crate::docker::models::ContainerRecord, HealthError> {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .ok_or(HealthError::Timeout)?;
        tokio::select! {
            () = self.shutdown.cancelled() => Err(HealthError::Interrupted),
            result = tokio::time::timeout(
                remaining,
                self.inspect_exact(id, release, configured_image_ref, image_identity),
            ) => {
                result.map_err(|_| HealthError::Timeout)?
            }
        }
    }

    async fn inspect_exact(
        &self,
        id: &str,
        release: uuid::Uuid,
        configured_image_ref: &str,
        image_identity: &ImageIdentity,
    ) -> Result<crate::docker::models::ContainerRecord, HealthError> {
        let value = self
            .docker
            .inspect_container(id)
            .await
            .map_err(|_| HealthError::Observation)?;
        if value.id != id
            || value.labels.get(RELEASE_ID_LABEL).map(String::as_str)
                != Some(release.to_string().as_str())
            || value.configured_image_ref.as_deref() != Some(configured_image_ref)
        {
            return Err(HealthError::Changed);
        }
        if !image_identity.matches_observation(
            value.image_id.as_deref(),
            value.manifest_descriptor.as_ref(),
        ) {
            return Err(HealthError::IdentityMismatch);
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
    #[error("container is not ready")]
    NotReady,
    #[error("container identity changed")]
    Changed,
    #[error("container image identity mismatched")]
    IdentityMismatch,
    #[error("Docker observation failed")]
    Observation,
    #[error("health verification interrupted")]
    Interrupted,
}
impl HealthError {
    pub const fn public_code(self) -> &'static str {
        match self {
            Self::NotReady => "CONTAINER_NOT_READY",
            Self::Missing => "HEALTHCHECK_MISSING",
            Self::Unhealthy => "HEALTH_UNHEALTHY",
            Self::Timeout => "HEALTH_TIMEOUT",
            Self::Exited => "CONTAINER_EXITED",
            Self::Restarted => "CONTAINER_RESTARTED",
            Self::CompletedNonzero => "COMPLETED_NONZERO",
            Self::Changed => "CONTAINER_CHANGED",
            Self::IdentityMismatch => "CANDIDATE_INVALID",
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
            configured_image_ref: Some(format!("example/app@sha256:{}", "a".repeat(64))),
            image_id: Some(format!("sha256:{}", "b".repeat(64))),
            manifest_descriptor: None,
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        }
    }

    fn image_identity() -> ImageIdentity {
        ImageIdentity::new(
            &format!("sha256:{}", "a".repeat(64)),
            &format!("sha256:{}", "b".repeat(64)),
            &crate::registry::Platform::canonical("linux", "amd64", None).unwrap(),
        )
        .unwrap()
    }

    fn verifier(records: Vec<ContainerRecord>) -> HealthVerifier {
        HealthVerifier::new(
            Arc::new(ScriptedDocker {
                records: Mutex::new(records.into()),
                last: Mutex::new(None),
            }),
            CancellationToken::new(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn maximum_running_window_includes_delayed_start_and_times_out_when_never_ready() {
        let release = uuid::Uuid::new_v4();
        let policy = HealthPolicy::Running {
            stable_window_seconds: 300,
        };
        let mut created = record(release, ContainerStatus::Created);
        created.started_at = None;
        let mut records = vec![created; 250];
        records.push(record(release, ContainerStatus::Running));
        let result = verifier(records)
            .verify(
                "container",
                release,
                &record(release, ContainerStatus::Running)
                    .configured_image_ref
                    .unwrap(),
                &image_identity(),
                &policy,
                health_deadline(&policy),
            )
            .await
            .unwrap();
        assert!(result.elapsed_seconds >= 548);
        let result = verifier(vec![record(release, ContainerStatus::Created)])
            .verify(
                "container",
                release,
                &record(release, ContainerStatus::Running)
                    .configured_image_ref
                    .unwrap(),
                &image_identity(),
                &policy,
                health_deadline(&policy),
            )
            .await;
        assert!(matches!(result, Err(HealthError::Timeout)));
    }

    #[tokio::test(start_paused = true)]
    async fn first_start_does_not_allow_real_restarts_to_reset_the_window() {
        let release = uuid::Uuid::new_v4();
        let mut created = record(release, ContainerStatus::Created);
        created.started_at = None;
        let running = record(release, ContainerStatus::Running);
        for changed_count in [false, true] {
            let mut restarted = running.clone();
            if changed_count {
                restarted.restart_count = Some(1);
            } else {
                restarted.started_at = Some("second-start".into());
            }
            let policy = HealthPolicy::Running {
                stable_window_seconds: 300,
            };
            assert!(matches!(
                verifier(vec![created.clone(), running.clone(), restarted])
                    .verify(
                        "container",
                        release,
                        running.configured_image_ref.as_ref().unwrap(),
                        &image_identity(),
                        &policy,
                        health_deadline(&policy)
                    )
                    .await,
                Err(HealthError::Restarted)
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_budget_covers_custom_probes_and_cancellation() {
        let release = uuid::Uuid::new_v4();
        let policy: HealthPolicy = serde_json::from_value(serde_json::json!({
            "policy": "healthy", "http": {"client": "curl", "scheme": "http",
            "host": "127.0.0.1", "port": 80, "path": "/", "interval_seconds": 300,
            "timeout_seconds": 60, "retries": 10, "start_period_seconds": 300}
        }))
        .unwrap();
        assert_eq!(health_deadline(&policy), Duration::from_secs(3910));
        let mut starting = record(release, ContainerStatus::Running);
        starting.health = HealthStatus::Starting;
        let mut ready = starting.clone();
        ready.health = HealthStatus::Healthy;
        let mut records = vec![starting.clone(); 400];
        records.push(ready);
        let result = verifier(records)
            .verify(
                "container",
                release,
                starting.configured_image_ref.as_ref().unwrap(),
                &image_identity(),
                &policy,
                health_deadline(&policy),
            )
            .await
            .unwrap();
        assert!(result.elapsed_seconds > 300);
        let verifier = verifier(vec![starting.clone()]);
        verifier.shutdown.cancel();
        assert!(matches!(
            verifier
                .verify(
                    "container",
                    release,
                    starting.configured_image_ref.as_ref().unwrap(),
                    &image_identity(),
                    &policy,
                    health_deadline(&policy)
                )
                .await,
            Err(HealthError::Interrupted)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn final_recheck_preserves_policy_and_successful_restart_baseline() {
        let release = uuid::Uuid::new_v4();
        for policy in [
            HealthPolicy::Healthy { http: None },
            HealthPolicy::Running {
                stable_window_seconds: 5,
            },
            HealthPolicy::Disabled {
                acknowledge_reduced_safety: true,
            },
            HealthPolicy::Completed,
        ] {
            let mut ready = record(release, ContainerStatus::Running);
            ready.health = HealthStatus::Healthy;
            if matches!(policy, HealthPolicy::Completed) {
                ready.status = ContainerStatus::Exited;
                ready.exit_code = Some(0);
            }
            let result = verifier(vec![ready.clone()])
                .verify(
                    "container",
                    release,
                    ready.configured_image_ref.as_ref().unwrap(),
                    &image_identity(),
                    &policy,
                    health_deadline(&policy),
                )
                .await
                .unwrap();
            assert!(result.recheck(&ready, &policy).is_ok());
            for change in 0..5 {
                let mut changed = ready.clone();
                match change {
                    0 => changed.restart_count = Some(1),
                    1 => changed.started_at = Some("new-start".into()),
                    2 => changed.status = ContainerStatus::Restarting,
                    3 => {
                        changed.status = ContainerStatus::Exited;
                        changed.exit_code = Some(1);
                    }
                    _ => changed.id = "other".into(),
                }
                assert!(result.recheck(&changed, &policy).is_err());
            }
            if matches!(policy, HealthPolicy::Healthy { .. }) {
                let mut changed = ready.clone();
                changed.health = HealthStatus::Unhealthy;
                assert!(matches!(
                    result.recheck(&changed, &policy),
                    Err(HealthError::Unhealthy)
                ));
                changed.status = ContainerStatus::Exited;
                changed.health = HealthStatus::Healthy;
                assert!(matches!(
                    policy_ready(&changed, &policy),
                    Err(HealthError::Exited)
                ));
            }
        }
        let ready = record(release, ContainerStatus::Running);
        assert!(policy_ready(&ready, &HealthPolicy::default()).unwrap());
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
                    &format!("example/app@sha256:{}", "a".repeat(64)),
                    &image_identity(),
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

    #[tokio::test]
    async fn exact_health_observation_accepts_both_engine_ids_and_fails_closed_on_descriptor() {
        let release = uuid::Uuid::new_v4();
        let config_record = record(release, ContainerStatus::Running);
        let mut manifest_record = config_record.clone();
        manifest_record.image_id = Some(format!("sha256:{}", "a".repeat(64)));
        manifest_record.manifest_descriptor = Some(crate::registry::ManifestDescriptor {
            digest: Some(format!("sha256:{}", "a".repeat(64))),
            os: Some("linux".into()),
            architecture: Some("amd64".into()),
            variant: None,
        });
        let mut wrong_descriptor = manifest_record.clone();
        wrong_descriptor
            .manifest_descriptor
            .as_mut()
            .unwrap()
            .digest = Some(format!("sha256:{}", "c".repeat(64)));
        let verifier = HealthVerifier::new(
            Arc::new(ScriptedDocker {
                records: Mutex::new(VecDeque::from([
                    config_record.clone(),
                    manifest_record,
                    wrong_descriptor,
                ])),
                last: Mutex::new(None),
            }),
            CancellationToken::new(),
        );
        let configured = format!("example/app@sha256:{}", "a".repeat(64));
        assert!(
            verifier
                .inspect_exact("container", release, &configured, &image_identity())
                .await
                .is_ok()
        );
        assert!(
            verifier
                .inspect_exact("container", release, &configured, &image_identity())
                .await
                .is_ok()
        );
        assert!(matches!(
            verifier
                .inspect_exact("container", release, &configured, &image_identity())
                .await,
            Err(HealthError::IdentityMismatch)
        ));
    }
}
