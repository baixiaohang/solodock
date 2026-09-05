use std::{sync::Arc, time::Duration};

#[cfg(feature = "docker-e2e")]
use std::collections::VecDeque;

use tokio::sync::OwnedSemaphorePermit;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    api::{AppState, mutations::M3Services},
    app_store::{
        AppStore, config_revision,
        releases::{ReleaseTrigger, ReleaseV2},
    },
    compose::{ComposeAction, ComposeError, ComposeRunner, RunContext},
    docker::{
        AppCatalogEntry,
        models::{ContainerRecord, ContainerStatus, DockerReadApi},
        ownership::validate_syntactic_identity,
    },
    mutation::AppMutationGuard,
    registry::{CredentialStore, ImageReference, Platform, RegistryResolver, ResolvedImage},
};

use super::{
    DeploymentLedger, DeploymentPhase, DeploymentRecord, DeploymentStatus, DeploymentTrigger,
    HealthError, HealthVerifier, ImagePuller, PullError,
};

#[derive(Clone)]
pub struct DeploymentEngine {
    pub store: AppStore,
    pub credentials: CredentialStore,
    pub resolver: RegistryResolver,
    pub ledger: DeploymentLedger,
    pub puller: Arc<dyn ImagePuller>,
    pub compose: Arc<dyn ComposeRunner>,
    pub docker: Arc<dyn DockerReadApi>,
    pub health: HealthVerifier,
    pub shutdown: CancellationToken,
    pub tasks: TaskTracker,
    #[cfg(feature = "docker-e2e")]
    pub test_effect_gate: Option<TestEffectGate>,
    #[cfg(feature = "docker-e2e")]
    pub test_candidate_gate: Option<TestEffectGate>,
}

#[cfg(feature = "docker-e2e")]
#[derive(Clone, Copy, Debug)]
pub enum TestEffectAction {
    Continue,
    Pause,
}

#[cfg(feature = "docker-e2e")]
#[derive(Clone)]
pub struct TestEffectGate {
    actions: Arc<std::sync::Mutex<VecDeque<TestEffectAction>>>,
    reached: Arc<tokio::sync::Semaphore>,
    resume: Arc<tokio::sync::Semaphore>,
}

#[cfg(feature = "docker-e2e")]
impl TestEffectGate {
    pub fn new(actions: impl IntoIterator<Item = TestEffectAction>) -> Self {
        Self {
            actions: Arc::new(std::sync::Mutex::new(actions.into_iter().collect())),
            reached: Arc::new(tokio::sync::Semaphore::new(0)),
            resume: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    pub async fn wait_until_reached(&self) {
        self.reached
            .acquire()
            .await
            .expect("test effect gate remains open")
            .forget();
    }

    pub fn resume(&self) {
        self.resume.add_permits(1);
    }

    async fn enter(&self, shutdown: &CancellationToken) -> Result<(), EngineError> {
        let action = self
            .actions
            .lock()
            .expect("test effect gate lock is not poisoned")
            .pop_front();
        if !matches!(action, Some(TestEffectAction::Pause)) {
            return Ok(());
        }
        self.reached.add_permits(1);
        tokio::select! {
            () = shutdown.cancelled() => Err(EngineError::Interrupted),
            permit = self.resume.acquire() => {
                permit.expect("test effect resume gate remains open").forget();
                Ok(())
            }
        }
    }
}

impl DeploymentEngine {
    pub fn spawn(
        &self,
        state: AppState,
        m3: Arc<M3Services>,
        deployment_id: Uuid,
        app_guard: AppMutationGuard,
        global_guard: OwnedSemaphorePermit,
    ) {
        let engine = self.clone();
        self.tasks.spawn(async move {
            let _guards = (app_guard, global_guard);
            if let Err(error) = engine.run(&state, &m3, deployment_id).await {
                let _ = crate::api::mutations::refresh(&state, &m3).await;
                record_terminal_error(&engine.ledger, deployment_id, error).await;
                m3.reconcile_notify.notify_one();
            }
        });
    }

    async fn run(
        &self,
        state: &AppState,
        m3: &M3Services,
        deployment_id: Uuid,
    ) -> Result<(), EngineError> {
        let record = self
            .ledger
            .get(deployment_id)
            .await
            .map_err(|_| EngineError::Internal)?
            .ok_or(EngineError::Internal)?;
        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Resolving,
                DeploymentStatus::Running,
                "started",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let metadata = self
            .store
            .read_metadata(record.app_id)
            .map_err(|_| EngineError::Stable("APP_NOT_FOUND"))?;
        if metadata.draft_revision != Some(record.requested_revision) {
            return Err(EngineError::Stable("DEPLOYMENT_FACTS_CHANGED"));
        }
        let active = self
            .store
            .read_release_link(record.app_id, "active")
            .map_err(|_| EngineError::Internal)?;
        let pending = self
            .store
            .read_release_link(record.app_id, "pending")
            .map_err(|_| EngineError::Internal)?;
        if active != record.from_release_id || pending != record.expected_pending_release_id {
            return Err(EngineError::Stable("DEPLOYMENT_FACTS_CHANGED"));
        }
        self.schedule_predecessor(&record).await?;

        let (resolved, candidate_id, candidate_release) = match record.trigger {
            DeploymentTrigger::Manual => {
                let result = if let Some(existing_pending) = record.expected_pending_release_id {
                    self.resume_pending(&record, existing_pending).await
                } else {
                    self.resolve_manual(&record, &metadata, active).await
                };
                match result {
                    Ok(value) => value,
                    Err(EngineError::AlreadyTerminal) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            DeploymentTrigger::Rollback => {
                self.resolve_rollback(&record, &metadata, active).await?
            }
            DeploymentTrigger::Poll => {
                let result = if let Some(existing_pending) = record.expected_pending_release_id {
                    self.resume_poll_pending(&record, existing_pending).await
                } else {
                    self.resolve_poll(&record, &metadata, active).await
                };
                match result {
                    Ok(value) => value,
                    Err(EngineError::AlreadyTerminal) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        };
        let pending_before = self
            .store
            .read_release_link(record.app_id, "pending")
            .map_err(|_| EngineError::Internal)?;
        if pending_before != Some(candidate_id) {
            self.store
                .set_pending(record.app_id, candidate_id)
                .map_err(|_| EngineError::Internal)?;
        }
        let _ = crate::api::mutations::refresh(state, m3).await;

        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Pulling,
                DeploymentStatus::Running,
                "candidate_published",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let image = ImageReference::parse(&candidate_release.source_image_ref)
            .map_err(|error| EngineError::Stable(error.public_code()))?;
        let credential = self.load_credential_ref(candidate_release.credential_ref, &image)?;
        let redaction = credential
            .as_ref()
            .map(|v| vec![v.secret.expose().as_bytes().to_vec()])
            .unwrap_or_default();
        if let Err(error) = self
            .puller
            .pull(deployment_id, &resolved, credential.as_ref(), redaction)
            .await
        {
            if matches!(error, PullError::Interrupted) {
                return Err(EngineError::Interrupted);
            }
            if matches!(error, PullError::CleanupFailed | PullError::OutputUnsafe) {
                return Err(EngineError::NeedsAttention(error.public_code()));
            }
            self.cleanup_pending(record.app_id, candidate_id)?;
            return Err(EngineError::Stable(error.public_code()));
        }

        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Applying,
                DeploymentStatus::Running,
                "image_verified",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let predecessor = self.schedule_predecessor(&record).await?;
        self.ledger
            .mark_effect_started(
                deployment_id,
                predecessor.as_ref().map(|value| value.id.as_str()),
                predecessor
                    .as_ref()
                    .and_then(|value| value.started_at.as_deref()),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.test_effect_boundary().await?;
        self.verify_release_links(record.app_id, active, candidate_id)?;
        let final_release = self
            .store
            .load_v2_release(record.app_id, candidate_id)
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        if final_release != candidate_release {
            return Err(EngineError::NeedsAttention("RELEASE_INVALID"));
        }
        let loaded = config_revision::load_verified(
            &self.store.app_directory(record.app_id),
            final_release.config_revision,
            self.store
                .integrity_key()
                .map_err(|_| EngineError::Internal)?,
        )
        .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        let binds = self
            .validate_runtime_paths_fresh(m3, &loaded.metadata)
            .await?;
        crate::api::mutations::validate_bind_plan_against_live_apps(
            state,
            m3,
            record.app_id,
            &loaded.metadata,
        )
        .await
        .map_err(EngineError::Stable)?;
        let final_predecessor = self.schedule_predecessor(&record).await?;
        if final_predecessor.as_ref().map(|value| value.id.as_str())
            != predecessor.as_ref().map(|value| value.id.as_str())
        {
            return Err(EngineError::NeedsAttention("CONTAINER_CHANGED"));
        }
        crate::api::mutations::validate_resources(
            state,
            record.app_id,
            &loaded.metadata,
            final_predecessor.as_ref().map(|value| value.id.as_str()),
            crate::error::RequestId(record.request_id),
        )
        .await
        .map_err(|error| EngineError::NeedsAttention(error.code_and_status().0))?;
        for identity in &binds {
            crate::domain::revalidate_bind_identity(identity, &m3.store.allowed_bind_roots())
                .map_err(|_| EngineError::Stable("BIND_CHANGED"))?;
        }
        if loaded.metadata.service_discovery_enabled {
            let app = state
                .observer
                .catalog
                .get(record.app_id)
                .ok_or(EngineError::NeedsAttention("APP_NOT_FOUND"))?;
            let allowed = final_predecessor
                .as_ref()
                .map(|container| container.id.as_str())
                .into_iter()
                .collect::<Vec<_>>();
            crate::docker::platform_network::ensure_for_app(
                self.docker.as_ref(),
                &app.slug,
                &allowed,
            )
            .await
            .map_err(|error| EngineError::Stable(error.public_code()))?;
        }
        let predecessor_context = record
            .expected_actual_release_id
            .map(|release| self.context(record.app_id, release))
            .transpose()?;
        let predecessor_binds = if let Some(release_id) = record.expected_actual_release_id {
            let release = self
                .store
                .load_v2_release(record.app_id, release_id)
                .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
            let predecessor_config = config_revision::load_verified(
                &self.store.app_directory(record.app_id),
                release.config_revision,
                self.store
                    .integrity_key()
                    .map_err(|_| EngineError::Internal)?,
            )
            .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
            self.validate_runtime_paths_fresh(m3, &predecessor_config.metadata)
                .await?
        } else {
            Vec::new()
        };
        if let Some(predecessor_context) = predecessor_context {
            if self
                .compose
                .run(ComposeAction::Stop, predecessor_context)
                .await
                .is_err()
            {
                return Err(EngineError::Interrupted);
            }
            let stopped = self.schedule_predecessor(&record).await?;
            if !matches!(stopped, Some(ref value) if value.status == ContainerStatus::Exited) {
                return Err(EngineError::Interrupted);
            }
        }
        let post_stop_binds = self
            .validate_runtime_paths_fresh(m3, &loaded.metadata)
            .await?;
        if post_stop_binds != binds {
            return Err(EngineError::Stable("BIND_CHANGED"));
        }
        for identity in &post_stop_binds {
            crate::domain::revalidate_bind_identity(identity, &m3.store.allowed_bind_roots())
                .map_err(|_| EngineError::Stable("BIND_CHANGED"))?;
        }
        crate::api::mutations::validate_bind_plan_against_live_apps(
            state,
            m3,
            record.app_id,
            &loaded.metadata,
        )
        .await
        .map_err(EngineError::Stable)?;
        let compose_result = self
            .compose
            .run(
                ComposeAction::DeployCandidate,
                self.context(record.app_id, candidate_id)?,
            )
            .await;
        if let Err(error) = compose_result {
            if matches!(error, ComposeError::Cancelled) {
                return Err(EngineError::Interrupted);
            }
            return match self.observe_failed_apply(&record, candidate_id).await? {
                FailedApplyObservation::Candidate(container) => {
                    self.ledger
                        .mark_effect_observed(deployment_id, &container.id)
                        .await
                        .map_err(|_| EngineError::Internal)?;
                    self.rollback_or_fail(
                        state,
                        m3,
                        &record,
                        CompensationRequest {
                            active,
                            candidate: candidate_id,
                            container: &container,
                            code: error.public_code(),
                        },
                    )
                    .await
                }
                FailedApplyObservation::NoEffect
                    if matches!(
                        error,
                        ComposeError::ValidationFailed
                            | ComposeError::OutputInvalid
                            | ComposeError::UnsafePath
                    ) =>
                {
                    self.restore_predecessor(
                        state,
                        m3,
                        &record,
                        predecessor.as_ref(),
                        &predecessor_binds,
                    )
                    .await?;
                    self.cleanup_pending(record.app_id, candidate_id)?;
                    Err(EngineError::Stable(error.public_code()))
                }
                FailedApplyObservation::NoEffect => {
                    self.restore_predecessor(
                        state,
                        m3,
                        &record,
                        predecessor.as_ref(),
                        &predecessor_binds,
                    )
                    .await?;
                    Err(EngineError::Interrupted)
                }
            };
        }
        self.test_candidate_boundary().await?;
        let candidate = self
            .observe_owned_candidate(
                record.app_id,
                candidate_id,
                predecessor.as_ref().map(|v| v.id.as_str()),
            )
            .await?;
        self.ledger
            .mark_effect_observed(deployment_id, &candidate.id)
            .await
            .map_err(|_| EngineError::Internal)?;
        if !candidate_matches_release(&candidate, &candidate_release) {
            return self
                .rollback_or_fail(
                    state,
                    m3,
                    &record,
                    CompensationRequest {
                        active,
                        candidate: candidate_id,
                        container: &candidate,
                        code: "CANDIDATE_INVALID",
                    },
                )
                .await;
        }

        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Verifying,
                DeploymentStatus::Running,
                "candidate_applied",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let health = match self
            .health
            .verify(
                &candidate.id,
                candidate_id,
                &candidate_release.runnable_image_ref,
                &candidate_release
                    .image_identity()
                    .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?,
                &loaded.metadata.health,
                Duration::from_secs(300),
            )
            .await
        {
            Ok(value) => value,
            Err(HealthError::Interrupted | HealthError::Observation | HealthError::Changed) => {
                return Err(EngineError::Interrupted);
            }
            Err(error) => {
                return self
                    .rollback_or_fail(
                        state,
                        m3,
                        &record,
                        CompensationRequest {
                            active,
                            candidate: candidate_id,
                            container: &candidate,
                            code: error.public_code(),
                        },
                    )
                    .await;
            }
        };
        self.ledger
            .set_health(
                deployment_id,
                &serde_json::to_string(&loaded.metadata.health)
                    .map_err(|_| EngineError::Internal)?,
                &serde_json::to_string(&health).map_err(|_| EngineError::Internal)?,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Committing,
                DeploymentStatus::Running,
                "health_passed",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.verify_release_links(record.app_id, active, candidate_id)?;
        let final_candidate = self
            .observe_owned_candidate(record.app_id, candidate_id, None)
            .await?;
        if final_candidate.id != candidate.id {
            return Err(EngineError::NeedsAttention("CONTAINER_CHANGED"));
        }
        if !candidate_matches_release(&final_candidate, &candidate_release) {
            return self
                .rollback_or_fail(
                    state,
                    m3,
                    &record,
                    CompensationRequest {
                        active,
                        candidate: candidate_id,
                        container: &final_candidate,
                        code: "CANDIDATE_INVALID",
                    },
                )
                .await;
        }
        let desired = if matches!(
            loaded.metadata.health,
            crate::domain::HealthPolicy::Completed
        ) {
            crate::domain::DesiredState::Stopped
        } else {
            crate::domain::DesiredState::Running
        };
        self.store
            .finalize_active(
                record.app_id,
                active,
                candidate_id,
                desired,
                deployment_id,
                time::OffsetDateTime::now_utc(),
            )
            .map_err(|_| EngineError::NeedsAttention("ACTIVE_FINALIZE_FAILED"))?;
        let _ = crate::api::mutations::refresh(state, m3).await;
        self.ledger
            .transition(
                deployment_id,
                DeploymentPhase::Terminal,
                DeploymentStatus::Succeeded,
                "committed",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        Ok(())
    }

    async fn resolve_manual(
        &self,
        record: &DeploymentRecord,
        metadata: &crate::domain::AppMetadata,
        active: Option<Uuid>,
    ) -> Result<(ResolvedImage, Uuid, ReleaseV2), EngineError> {
        let image = ImageReference::parse(
            metadata
                .discovery_image_ref
                .as_deref()
                .ok_or(EngineError::Stable("APP_UNCONFIGURED"))?,
        )
        .map_err(|error| EngineError::Stable(error.public_code()))?;
        let probe = self
            .docker
            .probe()
            .await
            .map_err(|error| EngineError::Stable(error.public_code()))?;
        let platform = Platform::canonical(
            &probe
                .os
                .ok_or(EngineError::Stable("DOCKER_API_INCOMPATIBLE"))?,
            &probe
                .architecture
                .ok_or(EngineError::Stable("DOCKER_API_INCOMPATIBLE"))?,
            None,
        )
        .map_err(|_| EngineError::Stable("DOCKER_API_INCOMPATIBLE"))?;
        let credential = self.load_credential(metadata, &image)?;
        let resolved = self
            .resolver
            .resolve(
                &image,
                &platform,
                credential
                    .as_ref()
                    .map(|v| (v.metadata.username.as_str(), &v.secret)),
            )
            .await
            .map_err(|error| EngineError::Stable(error.public_code()))?;
        if let Some(active_id) = active
            && let Ok(old) = self.store.load_v2_release(record.app_id, active_id)
            && old.manifest_digest == resolved.manifest_digest
            && Some(old.config_revision) == metadata.draft_revision
            && Some(old.config_sha256.as_str()) == metadata.draft_config_sha256.as_deref()
            && record.expected_pending_release_id.is_none()
            && self
                .no_op_runtime_converged(record, active_id, &old)
                .await?
        {
            let loaded = config_revision::load_verified(
                &self.store.app_directory(record.app_id),
                old.config_revision,
                self.store
                    .integrity_key()
                    .map_err(|_| EngineError::Internal)?,
            )
            .map_err(|_| EngineError::Stable("RELEASE_INVALID"))?;
            let desired = if matches!(
                loaded.metadata.health,
                crate::domain::HealthPolicy::Completed
            ) {
                crate::domain::DesiredState::Stopped
            } else {
                crate::domain::DesiredState::Running
            };
            self.store
                .finalize_active(
                    record.app_id,
                    Some(active_id),
                    active_id,
                    desired,
                    record.id,
                    time::OffsetDateTime::now_utc(),
                )
                .map_err(|_| EngineError::NeedsAttention("ACTIVE_FINALIZE_FAILED"))?;
            self.ledger
                .transition(
                    record.id,
                    DeploymentPhase::Terminal,
                    DeploymentStatus::NoOp,
                    "no_op",
                    None,
                )
                .await
                .map_err(|_| EngineError::Internal)?;
            return Err(EngineError::AlreadyTerminal);
        }
        let candidate_id = Uuid::new_v4();
        self.ledger
            .resolved(
                record.id,
                candidate_id,
                active,
                &resolved.source_image_ref,
                &resolved.source_descriptor_digest,
                &resolved.manifest_digest,
                &format!(
                    "{}/{}",
                    resolved.platform.os, resolved.platform.architecture
                ),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::Preparing,
                DeploymentStatus::Running,
                "resolved",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let release = self
            .store
            .publish_v2_release(
                metadata,
                candidate_id,
                &resolved,
                ReleaseTrigger::Manual,
                None,
            )
            .map_err(|_| EngineError::Stable("RELEASE_INVALID"))?;
        Ok((resolved, candidate_id, release))
    }

    async fn resolve_rollback(
        &self,
        record: &DeploymentRecord,
        _metadata: &crate::domain::AppMetadata,
        active: Option<Uuid>,
    ) -> Result<(ResolvedImage, Uuid, ReleaseV2), EngineError> {
        let target = record
            .rollback_target_release_id
            .ok_or(EngineError::Stable("ROLLBACK_TARGET_INVALID"))?;
        let release = self
            .store
            .load_v2_release(record.app_id, target)
            .map_err(|_| EngineError::Stable("ROLLBACK_TARGET_INVALID"))?;
        let resolved = resolved_from_release(&release);
        self.ledger
            .resolved(
                record.id,
                target,
                active,
                &release.source_image_ref,
                &release.source_descriptor_digest,
                &release.manifest_digest,
                &format!("{}/{}", release.platform_os, release.platform_architecture),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::Preparing,
                DeploymentStatus::Running,
                "rollback_target_verified",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        Ok((resolved, target, release))
    }

    async fn resolve_poll(
        &self,
        record: &DeploymentRecord,
        metadata: &crate::domain::AppMetadata,
        active: Option<Uuid>,
    ) -> Result<(ResolvedImage, Uuid, ReleaseV2), EngineError> {
        if !metadata.auto_deploy_enabled {
            return Err(EngineError::Stable("AUTO_DEPLOY_DISABLED"));
        }
        let source = record
            .scheduled_source_image_ref
            .as_deref()
            .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?;
        if Some(source) != metadata.discovery_image_ref.as_deref() {
            return Err(EngineError::Stable("DEPLOYMENT_FACTS_CHANGED"));
        }
        let image = ImageReference::parse(source)
            .map_err(|_| EngineError::Stable("POLL_TARGET_INVALID"))?;
        let manifest = record
            .scheduled_manifest_digest
            .as_deref()
            .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?;
        let platform = crate::registry::Platform::canonical(
            record
                .scheduled_platform_os
                .as_deref()
                .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?,
            record
                .scheduled_platform_architecture
                .as_deref()
                .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?,
            record.scheduled_platform_variant.as_deref(),
        )
        .map_err(|_| EngineError::Stable("POLL_TARGET_INVALID"))?;
        let resolved = ResolvedImage {
            source_image_ref: source.to_owned(),
            logical_registry: image.logical_registry.clone(),
            repository: record
                .scheduled_repository
                .clone()
                .filter(|value| value == &image.repository)
                .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?,
            source_tag: image.tag.clone(),
            source_descriptor_digest: record
                .scheduled_source_descriptor_digest
                .clone()
                .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?,
            index_digest: record.scheduled_index_digest.clone(),
            manifest_digest: manifest.to_owned(),
            runnable_image_ref: image
                .runnable(manifest)
                .map_err(|_| EngineError::Stable("POLL_TARGET_INVALID"))?,
            platform,
            local_image_id: record
                .scheduled_local_image_id
                .clone()
                .ok_or(EngineError::Stable("POLL_TARGET_INVALID"))?,
        };
        if let Some(active_id) = active
            && let Ok(old) = self.store.load_v2_release(record.app_id, active_id)
            && old.manifest_digest == resolved.manifest_digest
        {
            if Some(old.config_revision) != metadata.draft_revision
                || Some(old.config_sha256.as_str()) != metadata.draft_config_sha256.as_deref()
            {
                return Err(EngineError::Stable("CONFIG_PENDING_MANUAL"));
            }
            if record.expected_pending_release_id.is_none()
                && self
                    .no_op_runtime_converged(record, active_id, &old)
                    .await?
            {
                self.ledger
                    .transition(
                        record.id,
                        DeploymentPhase::Terminal,
                        DeploymentStatus::NoOp,
                        "no_op",
                        None,
                    )
                    .await
                    .map_err(|_| EngineError::Internal)?;
                return Err(EngineError::AlreadyTerminal);
            }
        }
        let candidate_id = Uuid::new_v4();
        self.ledger
            .resolved(
                record.id,
                candidate_id,
                active,
                &resolved.source_image_ref,
                &resolved.source_descriptor_digest,
                &resolved.manifest_digest,
                &format!(
                    "{}/{}",
                    resolved.platform.os, resolved.platform.architecture
                ),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::Preparing,
                DeploymentStatus::Running,
                "poll_target_verified",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let release = self
            .store
            .publish_v2_release(
                metadata,
                candidate_id,
                &resolved,
                ReleaseTrigger::Poll,
                None,
            )
            .map_err(|_| EngineError::Stable("RELEASE_INVALID"))?;
        Ok((resolved, candidate_id, release))
    }

    async fn resume_pending(
        &self,
        record: &DeploymentRecord,
        pending: Uuid,
    ) -> Result<(ResolvedImage, Uuid, ReleaseV2), EngineError> {
        let release = self
            .store
            .load_v2_release(record.app_id, pending)
            .map_err(|_| EngineError::NeedsAttention("PENDING_RELEASE_INVALID"))?;
        if release.config_revision != record.requested_revision {
            return Err(EngineError::Stable("DEPLOYMENT_FACTS_CHANGED"));
        }
        let resolved = resolved_from_release(&release);
        self.ledger
            .resolved(
                record.id,
                pending,
                record.from_release_id,
                &release.source_image_ref,
                &release.source_descriptor_digest,
                &release.manifest_digest,
                &format!("{}/{}", release.platform_os, release.platform_architecture),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::Preparing,
                DeploymentStatus::Running,
                "pending_resumed",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        Ok((resolved, pending, release))
    }

    async fn resume_poll_pending(
        &self,
        record: &DeploymentRecord,
        pending: Uuid,
    ) -> Result<(ResolvedImage, Uuid, ReleaseV2), EngineError> {
        let value = self.resume_pending(record, pending).await?;
        if record.scheduled_manifest_digest.as_deref() != Some(value.2.manifest_digest.as_str())
            || record.scheduled_source_image_ref.as_deref()
                != Some(value.2.source_image_ref.as_str())
            || record.scheduled_target_key.is_none()
            || record.poll_generation.is_none()
        {
            return Err(EngineError::NeedsAttention("POLL_PENDING_TARGET_CHANGED"));
        }
        Ok(value)
    }

    fn load_credential(
        &self,
        app: &crate::domain::AppMetadata,
        image: &ImageReference,
    ) -> Result<Option<crate::registry::LoadedCredential>, EngineError> {
        let value = app
            .credential_ref
            .map(|id| {
                self.credentials
                    .load(id)
                    .map_err(|_| EngineError::Stable("REGISTRY_CREDENTIAL_INVALID"))
            })
            .transpose()?;
        if value
            .as_ref()
            .is_some_and(|v| v.metadata.registry != image.logical_registry)
        {
            return Err(EngineError::Stable("REGISTRY_CREDENTIAL_MISMATCH"));
        }
        Ok(value)
    }

    fn load_credential_ref(
        &self,
        credential_ref: Option<Uuid>,
        image: &ImageReference,
    ) -> Result<Option<crate::registry::LoadedCredential>, EngineError> {
        let value = credential_ref
            .map(|id| {
                self.credentials
                    .load(id)
                    .map_err(|_| EngineError::Stable("REGISTRY_CREDENTIAL_INVALID"))
            })
            .transpose()?;
        if value
            .as_ref()
            .is_some_and(|v| v.metadata.registry != image.logical_registry)
        {
            return Err(EngineError::Stable("REGISTRY_CREDENTIAL_MISMATCH"));
        }
        Ok(value)
    }

    fn verify_release_links(
        &self,
        app_id: Uuid,
        active: Option<Uuid>,
        pending: Uuid,
    ) -> Result<(), EngineError> {
        if self
            .store
            .read_release_link(app_id, "active")
            .map_err(|_| EngineError::Internal)?
            != active
            || self
                .store
                .read_release_link(app_id, "pending")
                .map_err(|_| EngineError::Internal)?
                != Some(pending)
        {
            return Err(EngineError::NeedsAttention("DEPLOYMENT_FACTS_CHANGED"));
        }
        self.store
            .load_v2_release(app_id, pending)
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        Ok(())
    }

    async fn preflight(
        &self,
        app_id: Uuid,
        expected_release: Option<Uuid>,
        expected_container_id: Option<&str>,
    ) -> Result<Option<ContainerRecord>, EngineError> {
        let app = minimal_app(&self.store, app_id, expected_release)?;
        let candidates = self
            .docker
            .list_compose_app_containers(&app.project_name)
            .await
            .map_err(|_| EngineError::Stable("DOCKER_UNAVAILABLE"))?;
        if candidates.len() > 1 {
            return Err(EngineError::NeedsAttention("APP_CONTAINER_AMBIGUOUS"));
        }
        let Some(candidate) = candidates.into_iter().next() else {
            return if expected_release.is_none() && expected_container_id.is_none() {
                Ok(None)
            } else {
                Err(EngineError::NeedsAttention("CONTAINER_CHANGED"))
            };
        };
        let identity = validate_syntactic_identity(&candidate.labels, &app)
            .ok_or(EngineError::NeedsAttention("APP_CONTAINER_INVALID"))?;
        if Some(identity.release_id) != expected_release
            || expected_container_id != Some(candidate.id.as_str())
        {
            return Err(EngineError::NeedsAttention("APP_CONTAINER_INVALID"));
        }
        Ok(Some(candidate))
    }

    async fn schedule_predecessor(
        &self,
        record: &DeploymentRecord,
    ) -> Result<Option<ContainerRecord>, EngineError> {
        self.preflight(
            record.app_id,
            record.expected_actual_release_id,
            record.expected_actual_container_id.as_deref(),
        )
        .await
    }

    async fn no_op_runtime_converged(
        &self,
        record: &DeploymentRecord,
        active: Uuid,
        release: &ReleaseV2,
    ) -> Result<bool, EngineError> {
        if record.expected_actual_release_id != Some(active) {
            return Ok(false);
        }
        let Some(container) = self.schedule_predecessor(record).await? else {
            return Ok(false);
        };
        if !candidate_matches_release(&container, release) {
            return Ok(false);
        }
        let loaded = config_revision::load_verified(
            &self.store.app_directory(record.app_id),
            release.config_revision,
            self.store
                .integrity_key()
                .map_err(|_| EngineError::Internal)?,
        )
        .map_err(|_| EngineError::Stable("RELEASE_INVALID"))?;
        Ok(match loaded.metadata.health {
            crate::domain::HealthPolicy::Completed => {
                container.status == ContainerStatus::Exited && container.exit_code == Some(0)
            }
            crate::domain::HealthPolicy::Healthy { .. } => {
                container.status == ContainerStatus::Running
                    && container.health == crate::docker::models::HealthStatus::Healthy
            }
            crate::domain::HealthPolicy::Running { .. }
            | crate::domain::HealthPolicy::Disabled { .. } => {
                container.status == ContainerStatus::Running
            }
        })
    }

    async fn observe_owned_candidate(
        &self,
        app_id: Uuid,
        release: Uuid,
        old_id: Option<&str>,
    ) -> Result<ContainerRecord, EngineError> {
        let app = minimal_app(&self.store, app_id, Some(release))?;
        let candidates = self
            .docker
            .list_compose_app_containers(&app.project_name)
            .await
            .map_err(|_| EngineError::Interrupted)?;
        if candidates.len() != 1 {
            return Err(EngineError::NeedsAttention("APP_CONTAINER_AMBIGUOUS"));
        }
        let candidate = candidates.into_iter().next().expect("one candidate");
        let identity = validate_syntactic_identity(&candidate.labels, &app)
            .ok_or(EngineError::NeedsAttention("APP_CONTAINER_INVALID"))?;
        if identity.release_id != release {
            return Err(EngineError::NeedsAttention("APP_CONTAINER_INVALID"));
        }
        if old_id == Some(candidate.id.as_str()) {
            return Err(EngineError::NeedsAttention("CONTAINER_CHANGED"));
        }
        Ok(candidate)
    }

    async fn observe_failed_apply(
        &self,
        record: &DeploymentRecord,
        candidate_id: Uuid,
    ) -> Result<FailedApplyObservation, EngineError> {
        let app = minimal_app(&self.store, record.app_id, Some(candidate_id))?;
        let candidates = self
            .docker
            .list_compose_app_containers(&app.project_name)
            .await
            .map_err(|_| EngineError::Interrupted)?;
        if candidates.len() != 1 {
            if candidates.is_empty()
                && record.expected_actual_container_id.is_none()
                && record.expected_actual_release_id.is_none()
            {
                return Ok(FailedApplyObservation::NoEffect);
            }
            return Err(EngineError::Interrupted);
        }
        let container = candidates.into_iter().next().expect("one candidate");
        if container.id
            == record
                .expected_actual_container_id
                .as_deref()
                .unwrap_or_default()
        {
            let predecessor = minimal_app(
                &self.store,
                record.app_id,
                record.expected_actual_release_id,
            )?;
            let identity = validate_syntactic_identity(&container.labels, &predecessor)
                .ok_or(EngineError::Interrupted)?;
            if Some(identity.release_id) == record.expected_actual_release_id {
                return Ok(FailedApplyObservation::NoEffect);
            }
        }
        let identity =
            validate_syntactic_identity(&container.labels, &app).ok_or(EngineError::Interrupted)?;
        if identity.release_id == candidate_id {
            return Ok(FailedApplyObservation::Candidate(Box::new(container)));
        }
        Err(EngineError::Interrupted)
    }

    async fn rollback_or_fail(
        &self,
        state: &AppState,
        m3: &M3Services,
        record: &DeploymentRecord,
        request: CompensationRequest<'_>,
    ) -> Result<(), EngineError> {
        let CompensationRequest {
            active,
            candidate,
            container: candidate_container,
            code,
        } = request;
        let candidate_release = self
            .store
            .load_v2_release(record.app_id, candidate)
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::RollingBack,
                DeploymentStatus::Running,
                "candidate_failed",
                Some(code),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        let Some(old_id) = active else {
            self.ledger
                .mark_rollback_started(record.id, &candidate_container.id)
                .await
                .map_err(|_| EngineError::Internal)?;
            self.test_effect_boundary().await?;
            self.verify_release_links(record.app_id, None, candidate)?;
            let final_release = self
                .store
                .load_v2_release(record.app_id, candidate)
                .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
            if final_release != candidate_release {
                return Err(EngineError::NeedsAttention("RELEASE_INVALID"));
            }
            let loaded = config_revision::load_verified(
                &self.store.app_directory(record.app_id),
                final_release.config_revision,
                self.store
                    .integrity_key()
                    .map_err(|_| EngineError::Internal)?,
            )
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
            let binds = self
                .validate_runtime_paths_fresh(m3, &loaded.metadata)
                .await?;
            crate::api::mutations::validate_resources_for_detach(
                state,
                record.app_id,
                &loaded.metadata,
                Some(&candidate_container.id),
                crate::error::RequestId(record.request_id),
            )
            .await
            .map_err(|error| EngineError::NeedsAttention(error.code_and_status().0))?;
            let final_candidate = self
                .observe_owned_candidate(record.app_id, candidate, None)
                .await?;
            if final_candidate.id != candidate_container.id {
                return Err(EngineError::Interrupted);
            }
            for identity in &binds {
                crate::domain::revalidate_bind_identity(identity, &m3.store.allowed_bind_roots())
                    .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
            }
            remove_first_deploy_candidate(
                &self.store,
                &self.ledger,
                self.compose.as_ref(),
                self.docker.as_ref(),
                record,
                candidate,
                self.context(record.app_id, candidate)?,
            )
            .await?;
            return Err(EngineError::Stable(code));
        };
        let old = self
            .store
            .load_v2_release(record.app_id, old_id)
            .map_err(|_| EngineError::NeedsAttention("ROLLBACK_TARGET_INVALID"))?;
        let old_image = ImageReference::parse(&old.source_image_ref)
            .map_err(|_| EngineError::NeedsAttention("ROLLBACK_TARGET_INVALID"))?;
        let old_credential = self.load_credential_ref(old.credential_ref, &old_image)?;
        let old_redaction = old_credential
            .as_ref()
            .map(|value| vec![value.secret.expose().as_bytes().to_vec()])
            .unwrap_or_default();
        self.puller
            .pull(
                record.id,
                &resolved_from_release(&old),
                old_credential.as_ref(),
                old_redaction,
            )
            .await
            .map_err(|error| match error {
                PullError::Interrupted => EngineError::Interrupted,
                _ => EngineError::NeedsAttention(error.public_code()),
            })?;
        self.ledger
            .mark_rollback_started(record.id, &candidate_container.id)
            .await
            .map_err(|_| EngineError::Internal)?;
        self.test_effect_boundary().await?;
        self.verify_release_links(record.app_id, Some(old_id), candidate)?;
        let final_old = self
            .store
            .load_v2_release(record.app_id, old_id)
            .map_err(|_| EngineError::NeedsAttention("ROLLBACK_TARGET_INVALID"))?;
        let final_candidate_release = self
            .store
            .load_v2_release(record.app_id, candidate)
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        if final_old != old || final_candidate_release != candidate_release {
            return Err(EngineError::NeedsAttention("RELEASE_INVALID"));
        }
        let loaded = config_revision::load_verified(
            &self.store.app_directory(record.app_id),
            final_old.config_revision,
            self.store
                .integrity_key()
                .map_err(|_| EngineError::Internal)?,
        )
        .map_err(|_| EngineError::NeedsAttention("ROLLBACK_TARGET_INVALID"))?;
        let binds = self
            .validate_runtime_paths_fresh(m3, &loaded.metadata)
            .await?;
        let fresh_candidate = self
            .observe_owned_candidate(record.app_id, candidate, None)
            .await?;
        if fresh_candidate.id != candidate_container.id {
            return Err(EngineError::Interrupted);
        }
        crate::api::mutations::validate_resources(
            state,
            record.app_id,
            &loaded.metadata,
            Some(&fresh_candidate.id),
            crate::error::RequestId(record.request_id),
        )
        .await
        .map_err(|error| EngineError::NeedsAttention(error.code_and_status().0))?;
        for identity in &binds {
            crate::domain::revalidate_bind_identity(identity, &m3.store.allowed_bind_roots())
                .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
        }
        crate::api::mutations::validate_bind_plan_against_live_apps(
            state,
            m3,
            record.app_id,
            &loaded.metadata,
        )
        .await
        .map_err(EngineError::NeedsAttention)?;
        if loaded.metadata.service_discovery_enabled {
            let app = state
                .observer
                .catalog
                .get(record.app_id)
                .ok_or(EngineError::NeedsAttention("APP_NOT_FOUND"))?;
            crate::docker::platform_network::ensure_for_app(
                self.docker.as_ref(),
                &app.slug,
                &[fresh_candidate.id.as_str()],
            )
            .await
            .map_err(|error| EngineError::NeedsAttention(error.public_code()))?;
        }
        self.compose
            .run(ComposeAction::Stop, self.context(record.app_id, candidate)?)
            .await
            .map_err(|_| EngineError::NeedsAttention("ROLLBACK_FAILED"))?;
        let stopped_candidate = self
            .observe_owned_candidate(record.app_id, candidate, None)
            .await?;
        if stopped_candidate.id != fresh_candidate.id
            || stopped_candidate.status != ContainerStatus::Exited
        {
            return Err(EngineError::NeedsAttention("ROLLBACK_FAILED"));
        }
        let post_stop_binds = self
            .validate_runtime_paths_fresh(m3, &loaded.metadata)
            .await?;
        if post_stop_binds != binds {
            return Err(EngineError::NeedsAttention("BIND_CHANGED"));
        }
        for identity in &post_stop_binds {
            crate::domain::revalidate_bind_identity(identity, &m3.store.allowed_bind_roots())
                .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
        }
        crate::api::mutations::validate_bind_plan_against_live_apps(
            state,
            m3,
            record.app_id,
            &loaded.metadata,
        )
        .await
        .map_err(EngineError::NeedsAttention)?;
        self.compose
            .run(
                ComposeAction::DeployCandidate,
                self.context(record.app_id, old_id)?,
            )
            .await
            .map_err(|error| match error {
                ComposeError::ValidationFailed
                | ComposeError::OutputInvalid
                | ComposeError::UnsafePath => EngineError::NeedsAttention("ROLLBACK_FAILED"),
                _ => EngineError::Interrupted,
            })?;
        let container = self
            .observe_owned_candidate(record.app_id, old_id, Some(&fresh_candidate.id))
            .await?;
        if !candidate_matches_release(&container, &old) {
            return Err(EngineError::NeedsAttention("ROLLBACK_FAILED"));
        }
        self.ledger
            .mark_rollback_observed(record.id, Some(&container.id))
            .await
            .map_err(|_| EngineError::Internal)?;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::VerifyingRollback,
                DeploymentStatus::Running,
                "rollback_applied",
                None,
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        self.health
            .verify(
                &container.id,
                old_id,
                &old.runnable_image_ref,
                &old.image_identity()
                    .map_err(|_| EngineError::NeedsAttention("ROLLBACK_TARGET_INVALID"))?,
                &loaded.metadata.health,
                Duration::from_secs(300),
            )
            .await
            .map_err(|_| EngineError::NeedsAttention("ROLLBACK_HEALTH_FAILED"))?;
        self.cleanup_pending(record.app_id, candidate)?;
        let _ = crate::api::mutations::refresh(state, m3).await;
        self.ledger
            .transition(
                record.id,
                DeploymentPhase::Terminal,
                DeploymentStatus::RolledBack,
                "rolled_back",
                Some(code),
            )
            .await
            .map_err(|_| EngineError::Internal)?;
        Ok(())
    }

    fn context(&self, app_id: Uuid, release: Uuid) -> Result<RunContext, EngineError> {
        release_run_context(&self.store, app_id, release)
    }

    async fn restore_predecessor(
        &self,
        state: &AppState,
        services: &M3Services,
        record: &DeploymentRecord,
        predecessor: Option<&ContainerRecord>,
        expected_bind_identities: &[crate::domain::BindIdentity],
    ) -> Result<(), EngineError> {
        let Some(predecessor) = predecessor else {
            return Ok(());
        };
        if predecessor.status == ContainerStatus::Exited {
            return Ok(());
        }
        let release_id = record
            .expected_actual_release_id
            .ok_or(EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
        let release = self
            .store
            .load_v2_release(record.app_id, release_id)
            .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
        let loaded = config_revision::load_verified(
            &self.store.app_directory(record.app_id),
            release.config_revision,
            self.store
                .integrity_key()
                .map_err(|_| EngineError::Internal)?,
        )
        .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
        let identities = self
            .validate_runtime_paths_fresh(services, &loaded.metadata)
            .await?;
        if identities != expected_bind_identities {
            return Err(EngineError::NeedsAttention("BIND_CHANGED"));
        }
        for identity in &identities {
            crate::domain::revalidate_bind_identity(identity, &services.store.allowed_bind_roots())
                .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
        }
        crate::api::mutations::validate_bind_plan_against_live_apps(
            state,
            services,
            record.app_id,
            &loaded.metadata,
        )
        .await
        .map_err(EngineError::NeedsAttention)?;
        self.compose
            .run(
                ComposeAction::Start,
                self.context(record.app_id, release_id)?,
            )
            .await
            .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
        let restored = self
            .preflight(
                record.app_id,
                Some(release_id),
                Some(predecessor.id.as_str()),
            )
            .await
            .map_err(|_| EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?
            .ok_or(EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"))?;
        if restored.status != ContainerStatus::Running {
            return Err(EngineError::NeedsAttention("PREDECESSOR_RESTORE_FAILED"));
        }
        Ok(())
    }

    async fn validate_runtime_paths_fresh(
        &self,
        services: &M3Services,
        metadata: &crate::domain::ConfigMetadata,
    ) -> Result<Vec<crate::domain::BindIdentity>, EngineError> {
        let probe = self
            .docker
            .probe()
            .await
            .map_err(|_| EngineError::Interrupted)?;
        crate::api::mutations::validate_runtime_paths_for_docker_root(
            services,
            metadata,
            probe.docker_root_directory.as_deref(),
        )
        .map_err(EngineError::NeedsAttention)
    }

    async fn test_effect_boundary(&self) -> Result<(), EngineError> {
        #[cfg(feature = "docker-e2e")]
        if let Some(gate) = &self.test_effect_gate {
            gate.enter(&self.shutdown).await?;
        }
        Ok(())
    }

    async fn test_candidate_boundary(&self) -> Result<(), EngineError> {
        #[cfg(feature = "docker-e2e")]
        if let Some(gate) = &self.test_candidate_gate {
            gate.enter(&self.shutdown).await?;
        }
        Ok(())
    }
    fn cleanup_pending(&self, app_id: Uuid, candidate: Uuid) -> Result<(), EngineError> {
        self.store
            .remove_release_link_if(app_id, "pending", candidate)
            .map_err(|_| EngineError::Interrupted)
    }
}

fn release_run_context(
    store: &AppStore,
    app_id: Uuid,
    release: Uuid,
) -> Result<RunContext, EngineError> {
    let metadata = store
        .read_metadata(app_id)
        .map_err(|_| EngineError::Interrupted)?;
    let stop_grace_period_seconds = store
        .load_v2_release(app_id, release)
        .map_err(|_| EngineError::Interrupted)?
        .stop_grace_period_seconds;
    Ok(RunContext {
        project_name: metadata.resource_names().project_name,
        project_directory: store.app_directory(app_id),
        compose_file: store.release_compose_path(app_id, release),
        timeout: Duration::from_secs(u64::from(stop_grace_period_seconds) + 60),
        stop_grace_period_seconds,
        redaction_patterns: Vec::new(),
    })
}

async fn record_terminal_error(ledger: &DeploymentLedger, deployment_id: Uuid, error: EngineError) {
    let (status, code) = match error {
        EngineError::Interrupted => (DeploymentStatus::Interrupted, "DEPLOYMENT_INTERRUPTED"),
        EngineError::NeedsAttention(code) => (DeploymentStatus::NeedsAttention, code),
        EngineError::Stable(code) => (DeploymentStatus::Failed, code),
        EngineError::Internal | EngineError::AlreadyTerminal => {
            (DeploymentStatus::Interrupted, "DEPLOYMENT_INTERNAL")
        }
    };
    let _ = ledger
        .transition(
            deployment_id,
            DeploymentPhase::Terminal,
            status,
            "failed",
            Some(code),
        )
        .await;
}

async fn remove_first_deploy_candidate(
    store: &AppStore,
    ledger: &DeploymentLedger,
    compose: &dyn ComposeRunner,
    docker: &dyn DockerReadApi,
    record: &DeploymentRecord,
    candidate: Uuid,
    context: RunContext,
) -> Result<(), EngineError> {
    let project_name = context.project_name.clone();
    compose
        .run(ComposeAction::Stop, context.clone())
        .await
        .map_err(|_| EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED"))?;
    compose
        .run(ComposeAction::Remove, context)
        .await
        .map_err(|_| EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED"))?;
    let remaining = docker
        .list_compose_app_containers(&project_name)
        .await
        .map_err(|_| EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED"))?;
    if !remaining.is_empty() {
        return Err(EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED"));
    }
    ledger
        .mark_rollback_observed(record.id, None)
        .await
        .map_err(|_| EngineError::Internal)?;
    store
        .remove_release_link_if(record.app_id, "pending", candidate)
        .map_err(|_| EngineError::Interrupted)
}

fn minimal_app(
    store: &AppStore,
    id: Uuid,
    active: Option<Uuid>,
) -> Result<AppCatalogEntry, EngineError> {
    let metadata = store
        .read_metadata(id)
        .map_err(|_| EngineError::Interrupted)?;
    let project_name = metadata.resource_names().project_name;
    Ok(AppCatalogEntry {
        id,
        slug: metadata.slug,
        display_name: metadata.display_name,
        resource_name_schema_version: metadata.resource_name_schema_version,
        project_name,
        active_release_id: active,
        active_image_ref: None,
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
        desired_state: crate::domain::DesiredState::Stopped,
        auto_deploy_enabled: false,
        poll_interval_seconds: 300,
        draft: None,
    })
}
fn resolved_from_release(value: &ReleaseV2) -> ResolvedImage {
    ResolvedImage {
        source_image_ref: value.source_image_ref.clone(),
        logical_registry: value.logical_registry.clone(),
        repository: value.repository.clone(),
        source_tag: value.source_tag.clone(),
        source_descriptor_digest: value.source_descriptor_digest.clone(),
        index_digest: value.index_digest.clone(),
        manifest_digest: value.manifest_digest.clone(),
        runnable_image_ref: value.runnable_image_ref.clone(),
        platform: Platform {
            os: value.platform_os.clone(),
            architecture: value.platform_architecture.clone(),
            variant: value.platform_variant.clone(),
        },
        local_image_id: value.local_image_id.clone(),
    }
}

fn candidate_matches_release(container: &ContainerRecord, release: &ReleaseV2) -> bool {
    let Ok(identity) = release.image_identity() else {
        return false;
    };
    container.configured_image_ref.as_deref() == Some(release.runnable_image_ref.as_str())
        && identity.matches_observation(
            container.image_id.as_deref(),
            container.manifest_descriptor.as_ref(),
        )
        && matches!(
            container.status,
            ContainerStatus::Running
                | ContainerStatus::Created
                | ContainerStatus::Restarting
                | ContainerStatus::Exited
        )
}

#[derive(Debug)]
enum EngineError {
    Stable(&'static str),
    NeedsAttention(&'static str),
    Interrupted,
    Internal,
    AlreadyTerminal,
}

enum FailedApplyObservation {
    NoEffect,
    Candidate(Box<ContainerRecord>),
}

struct CompensationRequest<'a> {
    active: Option<Uuid>,
    candidate: Uuid,
    container: &'a ContainerRecord,
    code: &'static str,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        os::unix::fs::PermissionsExt,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::{
        app_store::atomic::AtomicWriter,
        compose::ComposeOutput,
        db::Database,
        docker::models::{
            DockerError, DockerErrorKind, DockerStream, HealthStatus, LogChunk, LogRequest,
            ProbeSnapshot, RawDockerEvent, RawStats,
        },
        registry::ManifestDescriptor,
    };

    fn release() -> ReleaseV2 {
        let manifest = format!("sha256:{}", "a".repeat(64));
        ReleaseV2 {
            schema_version: 3,
            compose_schema_version: 2,
            stop_grace_period_seconds: 10,
            service_discovery_enabled: false,
            id: Uuid::new_v4(),
            app_id: Uuid::new_v4(),
            config_revision: Uuid::new_v4(),
            config_sha256: "0".repeat(64),
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: manifest.clone(),
            index_digest: None,
            manifest_digest: manifest.clone(),
            runnable_image_ref: format!("registry.example/app@{manifest}"),
            platform_os: "linux".into(),
            platform_architecture: "amd64".into(),
            platform_variant: None,
            local_image_id: format!("sha256:{}", "b".repeat(64)),
            compose_sha256: "1".repeat(64),
            credential_ref: None,
            trigger: ReleaseTrigger::Manual,
            source_release_id: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            integrity_hmac: "signature".into(),
        }
    }

    fn container(release: &ReleaseV2, image_id: String) -> ContainerRecord {
        ContainerRecord {
            id: "c".repeat(64),
            name: "app".into(),
            labels: HashMap::new(),
            status: ContainerStatus::Running,
            health: HealthStatus::None,
            exit_code: None,
            restart_count: Some(0),
            started_at: Some("2026-08-30T00:00:00Z".into()),
            finished_at: None,
            configured_image_ref: Some(release.runnable_image_ref.clone()),
            image_id: Some(image_id),
            manifest_descriptor: None,
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        }
    }

    #[test]
    fn release_matcher_covers_noop_candidate_health_and_rollback_identity() {
        let release = release();
        let classic = container(&release, release.local_image_id.clone());
        assert!(candidate_matches_release(&classic, &release));

        let mut containerd = container(&release, release.manifest_digest.clone());
        containerd.manifest_descriptor = Some(ManifestDescriptor {
            digest: Some(release.manifest_digest.clone()),
            os: Some("linux".into()),
            architecture: Some("amd64".into()),
            variant: None,
        });
        assert!(candidate_matches_release(&containerd, &release));

        let mut wrong_descriptor = containerd.clone();
        wrong_descriptor
            .manifest_descriptor
            .as_mut()
            .unwrap()
            .digest = Some(format!("sha256:{}", "d".repeat(64)));
        assert!(!candidate_matches_release(&wrong_descriptor, &release));
        let mut mutable_ref = containerd.clone();
        mutable_ref.configured_image_ref = Some("registry.example/app:stable".into());
        assert!(!candidate_matches_release(&mutable_ref, &release));
        let mut paused = containerd;
        paused.status = ContainerStatus::Paused;
        assert!(!candidate_matches_release(&paused, &release));
    }

    #[test]
    fn run_context_uses_each_stopped_release_grace_period() {
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("apps");
        std::fs::create_dir(&apps).unwrap();
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = vec![9; 32];
        let store = AppStore::initialize_verified(apps, key.clone()).unwrap();
        let app_id = Uuid::new_v4();
        let first_revision = Uuid::new_v4();
        let draft = |stop_grace_period_seconds| {
            crate::domain::normalize_draft(
                crate::domain::DraftInput {
                    security_profile: None,
                    display_name: "Example".into(),
                    discovery_image_ref: "registry.example/app:stable".into(),
                    credential_ref: None,
                    auto_deploy_enabled: false,
                    auto_deploy_acknowledged: false,
                    poll_interval_seconds: 300,
                    stop_grace_period_seconds,
                    environment: Default::default(),
                    files: vec![],
                    ports: vec![],
                    volumes: vec![],
                    binds: vec![],
                    owned_default_network: true,
                    service_discovery_enabled: true,
                    networks: vec![],
                    health: Default::default(),
                },
                &Default::default(),
                &key,
                &[],
            )
            .unwrap()
        };
        let mut metadata = store
            .create_app(
                app_id,
                "example",
                Uuid::new_v4(),
                Some((first_revision, &draft(10))),
                time::OffsetDateTime::now_utc(),
            )
            .unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let resolved = crate::registry::ResolvedImage {
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: digest.clone(),
            index_digest: None,
            manifest_digest: digest.clone(),
            runnable_image_ref: format!("registry.example/app@{digest}"),
            platform: crate::registry::Platform::canonical("linux", "amd64", None).unwrap(),
            local_image_id: digest,
        };
        let predecessor = Uuid::new_v4();
        store
            .publish_v2_release(
                &metadata,
                predecessor,
                &resolved,
                ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        let candidate_revision = Uuid::new_v4();
        metadata = store
            .update_draft(
                app_id,
                Some(first_revision),
                candidate_revision,
                Uuid::new_v4(),
                &draft(60),
                time::OffsetDateTime::now_utc(),
            )
            .unwrap();
        let candidate = Uuid::new_v4();
        store
            .publish_v2_release(
                &metadata,
                candidate,
                &resolved,
                ReleaseTrigger::Manual,
                None,
            )
            .unwrap();

        assert_eq!(
            release_run_context(&store, app_id, predecessor)
                .unwrap()
                .stop_grace_period_seconds,
            10
        );
        assert_eq!(
            release_run_context(&store, app_id, candidate)
                .unwrap()
                .stop_grace_period_seconds,
            60
        );
    }

    struct CleanupCompose {
        fail_remove: bool,
        calls: AtomicUsize,
        actions: Mutex<Vec<(ComposeAction, u16)>>,
    }

    #[async_trait::async_trait]
    impl ComposeRunner for CleanupCompose {
        async fn run(
            &self,
            action: ComposeAction,
            _context: RunContext,
        ) -> Result<ComposeOutput, ComposeError> {
            self.actions
                .lock()
                .unwrap()
                .push((action, _context.stop_grace_period_seconds));
            self.calls.fetch_add(1, Ordering::SeqCst);
            if action == ComposeAction::Remove && self.fail_remove {
                Err(ComposeError::UnknownEffect)
            } else {
                Ok(ComposeOutput {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
    }

    enum CleanupObservation {
        Error,
        Containers(Vec<ContainerRecord>),
    }

    struct CleanupDocker {
        observation: CleanupObservation,
        list_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl DockerReadApi for CleanupDocker {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }

        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            self.list_compose_app_containers("unused").await
        }

        async fn list_compose_app_containers(
            &self,
            _project_name: &str,
        ) -> Result<Vec<ContainerRecord>, DockerError> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            match &self.observation {
                CleanupObservation::Error => {
                    Err(DockerError::new(DockerErrorKind::ObservationFailed))
                }
                CleanupObservation::Containers(containers) => Ok(containers.clone()),
            }
        }

        async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
            Err(DockerError::new(DockerErrorKind::ContainerChanged))
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

    struct CleanupFixture {
        _root: tempfile::TempDir,
        store: AppStore,
        ledger: DeploymentLedger,
        record: DeploymentRecord,
        candidate: Uuid,
        context: RunContext,
    }

    async fn cleanup_fixture() -> CleanupFixture {
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("apps");
        std::fs::create_dir(&apps).unwrap();
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = AppStore::initialize_verified(apps, vec![7; 32]).unwrap();
        let app_id = Uuid::new_v4();
        let candidate = Uuid::new_v4();
        let app_directory = store.app_directory(app_id);
        let releases_directory = app_directory.join("releases");
        let candidate_directory = releases_directory.join(candidate.to_string());
        for directory in [
            app_directory.as_path(),
            releases_directory.as_path(),
            candidate_directory.as_path(),
        ] {
            std::fs::create_dir(directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        AtomicWriter::switch_release_link(&app_directory, "pending", candidate).unwrap();

        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let ledger = DeploymentLedger::new(database);
        let record = ledger
            .create(
                Uuid::new_v4(),
                app_id,
                DeploymentTrigger::Manual,
                Uuid::new_v4(),
                None,
                None,
                None,
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        ledger
            .transition(
                record.id,
                DeploymentPhase::RollingBack,
                DeploymentStatus::Running,
                "candidate_failed",
                Some("CANDIDATE_INVALID"),
            )
            .await
            .unwrap();
        ledger
            .mark_rollback_started(record.id, &"c".repeat(64))
            .await
            .unwrap();
        let context = RunContext {
            project_name: crate::domain::AppResourceIdentity {
                app_id,
                slug: "example",
                schema_version: crate::domain::RESOURCE_NAME_SCHEMA_CURRENT,
            }
            .resource_names()
            .project_name,
            project_directory: app_directory,
            compose_file: store.release_compose_path(app_id, candidate),
            timeout: Duration::from_secs(60),
            stop_grace_period_seconds: 10,
            redaction_patterns: Vec::new(),
        };
        CleanupFixture {
            _root: root,
            store,
            ledger,
            record,
            candidate,
            context,
        }
    }

    fn leftover_candidate() -> ContainerRecord {
        let release = release();
        container(&release, release.local_image_id.clone())
    }

    async fn assert_cleanup_failure(
        name: &str,
        fail_remove: bool,
        observation: CleanupObservation,
        expected_list_calls: usize,
    ) {
        let fixture = cleanup_fixture().await;
        let compose = CleanupCompose {
            fail_remove,
            calls: AtomicUsize::new(0),
            actions: Mutex::new(Vec::new()),
        };
        let docker = CleanupDocker {
            observation,
            list_calls: AtomicUsize::new(0),
        };
        let error = remove_first_deploy_candidate(
            &fixture.store,
            &fixture.ledger,
            &compose,
            &docker,
            &fixture.record,
            fixture.candidate,
            fixture.context,
        )
        .await
        .expect_err(name);
        assert!(
            matches!(
                &error,
                EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED")
            ),
            "{name}: unexpected compensation result"
        );
        record_terminal_error(&fixture.ledger, fixture.record.id, error).await;

        let terminal = fixture
            .ledger
            .get(fixture.record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.status, DeploymentStatus::NeedsAttention, "{name}");
        assert_eq!(
            terminal.error_code.as_deref(),
            Some("CANDIDATE_CLEANUP_FAILED"),
            "{name}"
        );
        assert_eq!(
            fixture
                .store
                .read_release_link(fixture.record.app_id, "pending")
                .unwrap(),
            Some(fixture.candidate),
            "{name}: cleanup failure must retain pending"
        );
        assert_eq!(compose.calls.load(Ordering::SeqCst), 2, "{name}");
        assert_eq!(
            docker.list_calls.load(Ordering::SeqCst),
            expected_list_calls,
            "{name}"
        );
        let transitions = fixture.ledger.transitions(fixture.record.id).await.unwrap();
        assert!(
            transitions.iter().any(|transition| {
                transition.result == "candidate_failed"
                    && transition.code.as_deref() == Some("CANDIDATE_INVALID")
            }),
            "{name}: original candidate rejection must remain in history"
        );
        let compensation = transitions.last().unwrap();
        assert_eq!(compensation.phase, DeploymentPhase::Terminal, "{name}");
        assert_eq!(compensation.result, "failed", "{name}");
        assert_eq!(
            compensation.code.as_deref(),
            Some("CANDIDATE_CLEANUP_FAILED"),
            "{name}: compensation failure must be recorded"
        );
    }

    #[tokio::test]
    async fn first_deploy_cleanup_failures_retain_pending_and_never_report_failed() {
        assert_cleanup_failure(
            "remove failure",
            true,
            CleanupObservation::Containers(Vec::new()),
            0,
        )
        .await;
        assert_cleanup_failure("observation failure", false, CleanupObservation::Error, 1).await;
        assert_cleanup_failure(
            "container remains",
            false,
            CleanupObservation::Containers(vec![leftover_candidate()]),
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn first_deploy_cleanup_allows_failed_only_after_observed_absence() {
        let fixture = cleanup_fixture().await;
        let compose = CleanupCompose {
            fail_remove: false,
            calls: AtomicUsize::new(0),
            actions: Mutex::new(Vec::new()),
        };
        let docker = CleanupDocker {
            observation: CleanupObservation::Containers(Vec::new()),
            list_calls: AtomicUsize::new(0),
        };
        remove_first_deploy_candidate(
            &fixture.store,
            &fixture.ledger,
            &compose,
            &docker,
            &fixture.record,
            fixture.candidate,
            fixture.context,
        )
        .await
        .unwrap();
        assert_eq!(
            *compose.actions.lock().unwrap(),
            vec![(ComposeAction::Stop, 10), (ComposeAction::Remove, 10)]
        );
        assert_eq!(
            fixture
                .store
                .read_release_link(fixture.record.app_id, "pending")
                .unwrap(),
            None
        );
        record_terminal_error(
            &fixture.ledger,
            fixture.record.id,
            EngineError::Stable("CANDIDATE_INVALID"),
        )
        .await;
        let terminal = fixture
            .ledger
            .get(fixture.record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.status, DeploymentStatus::Failed);
        assert_eq!(terminal.error_code.as_deref(), Some("CANDIDATE_INVALID"));
    }
}
