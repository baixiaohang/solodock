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
                let (status, code) = match error {
                    EngineError::Interrupted => {
                        (DeploymentStatus::Interrupted, "DEPLOYMENT_INTERRUPTED")
                    }
                    EngineError::NeedsAttention(code) => (DeploymentStatus::NeedsAttention, code),
                    EngineError::Stable(code) => (DeploymentStatus::Failed, code),
                    EngineError::Internal | EngineError::AlreadyTerminal => {
                        (DeploymentStatus::Interrupted, "DEPLOYMENT_INTERNAL")
                    }
                };
                let _ = engine
                    .ledger
                    .transition(
                        deployment_id,
                        DeploymentPhase::Terminal,
                        status,
                        "failed",
                        Some(code),
                    )
                    .await;
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
        if metadata.draft_revision != record.requested_revision {
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
            crate::domain::revalidate_bind_identity(identity, &m3.allowed_bind_roots)
                .map_err(|_| EngineError::Stable("BIND_CHANGED"))?;
        }
        let compose_result = self
            .compose
            .run(
                ComposeAction::DeployCandidate,
                self.context(record.app_id, candidate_id),
            )
            .await;
        if let Err(error) = compose_result {
            return match error {
                ComposeError::ValidationFailed
                | ComposeError::OutputInvalid
                | ComposeError::UnsafePath => {
                    match self
                        .observe_failed_apply(&record, candidate_id, &candidate_release)
                        .await?
                    {
                        FailedApplyObservation::NoEffect => {
                            self.cleanup_pending(record.app_id, candidate_id)?;
                            Err(EngineError::Stable(error.public_code()))
                        }
                        FailedApplyObservation::Candidate(container) => {
                            self.ledger
                                .mark_effect_observed(deployment_id, &container.id)
                                .await
                                .map_err(|_| EngineError::Internal)?;
                            self.rollback_or_fail(
                                state,
                                m3,
                                &record,
                                active,
                                candidate_id,
                                error.public_code(),
                            )
                            .await
                        }
                    }
                }
                ComposeError::Unavailable
                | ComposeError::Incompatible
                | ComposeError::PermissionDenied
                | ComposeError::Timeout
                | ComposeError::Cancelled
                | ComposeError::UnknownEffect => Err(EngineError::Interrupted),
            };
        }
        let candidate = self
            .observe_candidate(
                record.app_id,
                candidate_id,
                &candidate_release,
                predecessor.as_ref().map(|v| v.id.as_str()),
            )
            .await?;
        self.ledger
            .mark_effect_observed(deployment_id, &candidate.id)
            .await
            .map_err(|_| EngineError::Internal)?;

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
                &candidate_release.local_image_id,
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
                        active,
                        candidate_id,
                        error.public_code(),
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
            .observe_candidate(record.app_id, candidate_id, &candidate_release, None)
            .await?;
        if final_candidate.id != candidate.id {
            return Err(EngineError::NeedsAttention("CONTAINER_CHANGED"));
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
        let image = ImageReference::parse(&metadata.discovery_image_ref)
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
            && old.config_revision == metadata.draft_revision
            && old.config_sha256 == metadata.draft_config_sha256
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
        if source != metadata.discovery_image_ref {
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
            if old.config_revision != metadata.draft_revision
                || old.config_sha256 != metadata.draft_config_sha256
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
        let app = minimal_app(app_id, expected_release);
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
        if container.configured_image_ref.as_deref() != Some(release.runnable_image_ref.as_str())
            || container.image_id.as_deref() != Some(release.local_image_id.as_str())
        {
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

    async fn observe_candidate(
        &self,
        app_id: Uuid,
        release: Uuid,
        target: &ReleaseV2,
        old_id: Option<&str>,
    ) -> Result<ContainerRecord, EngineError> {
        let app = minimal_app(app_id, Some(release));
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
        if identity.release_id != release
            || candidate.configured_image_ref.as_deref() != Some(target.runnable_image_ref.as_str())
            || candidate.image_id.as_deref() != Some(target.local_image_id.as_str())
            || old_id == Some(candidate.id.as_str())
            || !matches!(
                candidate.status,
                ContainerStatus::Running
                    | ContainerStatus::Created
                    | ContainerStatus::Restarting
                    | ContainerStatus::Exited
            )
        {
            return Err(EngineError::NeedsAttention("CANDIDATE_INVALID"));
        }
        Ok(candidate)
    }

    async fn observe_failed_apply(
        &self,
        record: &DeploymentRecord,
        candidate_id: Uuid,
        candidate_release: &ReleaseV2,
    ) -> Result<FailedApplyObservation, EngineError> {
        let app = minimal_app(record.app_id, Some(candidate_id));
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
            let predecessor = minimal_app(record.app_id, record.expected_actual_release_id);
            let identity = validate_syntactic_identity(&container.labels, &predecessor)
                .ok_or(EngineError::Interrupted)?;
            if Some(identity.release_id) == record.expected_actual_release_id {
                return Ok(FailedApplyObservation::NoEffect);
            }
        }
        let identity =
            validate_syntactic_identity(&container.labels, &app).ok_or(EngineError::Interrupted)?;
        if identity.release_id == candidate_id
            && container.configured_image_ref.as_deref()
                == Some(candidate_release.runnable_image_ref.as_str())
            && container.image_id.as_deref() == Some(candidate_release.local_image_id.as_str())
        {
            return Ok(FailedApplyObservation::Candidate(Box::new(container)));
        }
        Err(EngineError::Interrupted)
    }

    async fn rollback_or_fail(
        &self,
        state: &AppState,
        m3: &M3Services,
        record: &DeploymentRecord,
        active: Option<Uuid>,
        candidate: Uuid,
        code: &'static str,
    ) -> Result<(), EngineError> {
        let candidate_release = self
            .store
            .load_v2_release(record.app_id, candidate)
            .map_err(|_| EngineError::NeedsAttention("RELEASE_INVALID"))?;
        let candidate_container = self
            .observe_candidate(record.app_id, candidate, &candidate_release, None)
            .await?;
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
                .observe_candidate(record.app_id, candidate, &final_release, None)
                .await?;
            if final_candidate.id != candidate_container.id {
                return Err(EngineError::Interrupted);
            }
            for identity in &binds {
                crate::domain::revalidate_bind_identity(identity, &m3.allowed_bind_roots)
                    .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
            }
            self.compose
                .run(
                    ComposeAction::Remove,
                    self.context(record.app_id, candidate),
                )
                .await
                .map_err(|_| EngineError::Interrupted)?;
            let remaining = self
                .docker
                .list_compose_app_containers(&crate::domain::AppMetadata::project_name(
                    record.app_id,
                ))
                .await
                .map_err(|_| EngineError::Interrupted)?;
            if !remaining.is_empty() {
                return Err(EngineError::NeedsAttention("CANDIDATE_CLEANUP_FAILED"));
            }
            self.ledger
                .mark_rollback_observed(record.id, None)
                .await
                .map_err(|_| EngineError::Internal)?;
            self.cleanup_pending(record.app_id, candidate)?;
            return Err(EngineError::Stable(code));
        };
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
            .observe_candidate(record.app_id, candidate, &final_candidate_release, None)
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
            crate::domain::revalidate_bind_identity(identity, &m3.allowed_bind_roots)
                .map_err(|_| EngineError::NeedsAttention("BIND_CHANGED"))?;
        }
        self.compose
            .run(
                ComposeAction::DeployCandidate,
                self.context(record.app_id, old_id),
            )
            .await
            .map_err(|error| match error {
                ComposeError::ValidationFailed
                | ComposeError::OutputInvalid
                | ComposeError::UnsafePath => EngineError::NeedsAttention("ROLLBACK_FAILED"),
                _ => EngineError::Interrupted,
            })?;
        let container = self
            .observe_candidate(record.app_id, old_id, &old, None)
            .await?;
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
                &old.local_image_id,
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

    fn context(&self, app_id: Uuid, release: Uuid) -> RunContext {
        RunContext {
            project_name: crate::domain::AppMetadata::project_name(app_id),
            project_directory: self.store.app_directory(app_id),
            compose_file: self.store.release_compose_path(app_id, release),
            timeout: Duration::from_secs(60),
            redaction_patterns: Vec::new(),
        }
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
    fn cleanup_pending(&self, app_id: Uuid, candidate: Uuid) -> Result<(), EngineError> {
        self.store
            .remove_release_link_if(app_id, "pending", candidate)
            .map_err(|_| EngineError::Interrupted)
    }
}

fn minimal_app(id: Uuid, active: Option<Uuid>) -> AppCatalogEntry {
    AppCatalogEntry {
        id,
        slug: String::new(),
        display_name: String::new(),
        project_name: crate::domain::AppMetadata::project_name(id),
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
    }
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
