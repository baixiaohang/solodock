use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    api::{AppState, deployments::M4Services, mutations::M3Services},
    app_store::{AppStore, StoreError},
    deploy::{
        DeploymentStatus, DeploymentTrigger, ScheduleCommand, ScheduleError, ScheduleFacts,
        ScheduleResult, ScheduledResolvedTarget,
    },
    docker::{AppCatalogEntry, ownership::validate_syntactic_identity},
};

use super::{
    CredentialStore, ImageReference, Platform, PollObservation, PollOutcome, PollResolve,
    PollState, PollStateStore, RegistryError, ResolvedImage,
};

const FAILURE_RETRY_SECONDS: u64 = 5;

#[derive(Clone, Default)]
pub struct PollHealth {
    running: Arc<AtomicBool>,
    degraded: Arc<AtomicBool>,
    due: Arc<AtomicUsize>,
    inflight: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PollHealthSnapshot {
    pub status: &'static str,
    pub due: usize,
    pub inflight: usize,
}

impl PollHealth {
    pub fn snapshot(&self) -> PollHealthSnapshot {
        PollHealthSnapshot {
            status: if self.degraded.load(Ordering::Acquire) {
                "degraded"
            } else if self.running.load(Ordering::Acquire) {
                "running"
            } else {
                "stopped"
            },
            due: self.due.load(Ordering::Acquire),
            inflight: self.inflight.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone)]
pub struct PollCoordinator {
    pub store: PollStateStore,
    pub health: PollHealth,
    pub notify: Arc<Notify>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct DueEntry {
    due_unix: i64,
    app_id: Uuid,
    generation: String,
    webhook_sequence: i64,
}

impl PollCoordinator {
    pub fn new(store: PollStateStore, shutdown: CancellationToken, tasks: TaskTracker) -> Self {
        Self {
            store,
            health: PollHealth::default(),
            notify: Arc::new(Notify::new()),
            shutdown,
            tasks,
        }
    }

    pub fn start(
        &self,
        state: AppState,
        m3: Arc<M3Services>,
        m4: Arc<M4Services>,
    ) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        self.tasks.spawn(async move {
            coordinator.health.running.store(true, Ordering::Release);
            let _guard = PollRunGuard {
                health: coordinator.health.clone(),
                shutdown: coordinator.shutdown.clone(),
            };
            coordinator.run(state, m3, m4).await;
        })
    }

    #[cfg(feature = "docker-e2e")]
    pub async fn run_once_for_test(
        &self,
        state: &AppState,
        m3: &Arc<M3Services>,
        m4: &Arc<M4Services>,
        app_id: Uuid,
    ) -> bool {
        let Ok(metadata) = m3.store.read_metadata(app_id) else {
            return false;
        };
        let Ok(generation) = poll_generation(&m3.store, &m4.credentials, &metadata) else {
            return false;
        };
        self.poll_one(
            state,
            m3,
            m4,
            DueEntry {
                due_unix: OffsetDateTime::now_utc().unix_timestamp(),
                app_id,
                generation,
                webhook_sequence: 0,
            },
        )
        .await
        .is_ok()
    }

    async fn run(&self, state: AppState, m3: Arc<M3Services>, m4: Arc<M4Services>) {
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            let snapshot = state.observer.catalog.snapshot();
            let ids = snapshot.apps.iter().map(|app| app.id).collect::<Vec<_>>();
            if snapshot.recovery_issues.is_empty() && self.store.retain_apps(&ids).await.is_err() {
                self.health.degraded.store(true, Ordering::Release);
                tokio::select! { () = self.shutdown.cancelled() => break, () = tokio::time::sleep(Duration::from_secs(30)) => continue }
            }
            let mut heap = BinaryHeap::new();
            let mut inventory_degraded = false;
            let now = OffsetDateTime::now_utc();
            for app in &snapshot.apps {
                let Some(metadata) = m3.store.read_metadata(app.id).ok() else {
                    inventory_degraded = true;
                    continue;
                };
                let Ok(generation) = poll_generation(&m3.store, &m4.credentials, &metadata) else {
                    inventory_degraded = true;
                    continue;
                };
                let current = self.store.get(app.id).await.ok().flatten();
                if !metadata.auto_deploy_enabled {
                    if self
                        .store
                        .publish(
                            app.id,
                            PollObservation {
                                generation: &generation,
                                enabled: false,
                                next_check_not_before: None,
                                checked_at: None,
                                success: false,
                                replace_observed_fields: false,
                                source_descriptor_digest: None,
                                etag: None,
                                manifest_digest: None,
                                platform: None,
                                outcome: PollOutcome::Disabled,
                                error_class: None,
                                error_code: None,
                                transient_failures: 0,
                            },
                        )
                        .await
                        .is_err()
                    {
                        inventory_degraded = true;
                    }
                    if let Some(current) = current
                        && current.webhook_sequence > current.webhook_processed_sequence
                        && self
                            .store
                            .mark_webhook_processed(app.id, current.webhook_sequence)
                            .await
                            .is_err()
                    {
                        inventory_degraded = true;
                    }
                    continue;
                }
                let pending_webhook = current.as_ref().filter(|value| {
                    value.generation == generation
                        && value.webhook_sequence > value.webhook_processed_sequence
                });
                let raw_due = pending_webhook.map_or_else(
                    || {
                        current
                            .as_ref()
                            .filter(|value| value.generation == generation)
                            .and_then(|value| value.next_check_not_before)
                            .unwrap_or_else(|| {
                                now + time::Duration::seconds(i64::from(initial_jitter(
                                    &m3.store,
                                    app.id,
                                    &generation,
                                    metadata.poll_interval_seconds,
                                )))
                            })
                    },
                    |value| {
                        let wake_due = now
                            + time::Duration::seconds(i64::from(webhook_jitter(
                                app.id,
                                value.webhook_sequence,
                            )));
                        if webhook_must_respect_backoff(value, now) {
                            value
                                .next_check_not_before
                                .map_or(wake_due, |backoff| backoff.max(wake_due))
                        } else {
                            wake_due
                        }
                    },
                );
                let due = clamp_persisted_due(raw_due, now, metadata.poll_interval_seconds);
                heap.push(Reverse(DueEntry {
                    due_unix: due.unix_timestamp(),
                    app_id: app.id,
                    generation,
                    webhook_sequence: pending_webhook.map_or(0, |value| value.webhook_sequence),
                }));
            }
            self.health.due.store(heap.len(), Ordering::Release);
            self.health
                .degraded
                .store(inventory_degraded, Ordering::Release);
            let Some(Reverse(next)) = heap.pop() else {
                tokio::select! { () = self.shutdown.cancelled() => break, () = self.notify.notified() => continue, () = tokio::time::sleep(Duration::from_secs(60)) => continue }
            };
            let delay = (next.due_unix - OffsetDateTime::now_utc().unix_timestamp()).max(0) as u64;
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                () = self.notify.notified() => continue,
                () = tokio::time::sleep(Duration::from_secs(delay)) => {}
            }
            if self.shutdown.is_cancelled() {
                break;
            }
            let now = OffsetDateTime::now_utc().unix_timestamp();
            let mut batch = vec![next];
            while heap
                .peek()
                .is_some_and(|Reverse(entry)| entry.due_unix <= now)
            {
                if let Some(Reverse(entry)) = heap.pop() {
                    batch.push(entry);
                }
            }
            let results = stream::iter(batch)
                .map(|due| async {
                    self.health.inflight.fetch_add(1, Ordering::AcqRel);
                    let app_id = due.app_id;
                    let webhook_sequence = due.webhook_sequence;
                    let mut result = self.poll_one(&state, &m3, &m4, due).await;
                    if result.is_ok()
                        && !self.shutdown.is_cancelled()
                        && webhook_sequence > 0
                        && self
                            .store
                            .mark_webhook_processed(app_id, webhook_sequence)
                            .await
                            .is_err()
                    {
                        result = Err(());
                    }
                    self.health.inflight.fetch_sub(1, Ordering::AcqRel);
                    result
                })
                .buffer_unordered(2)
                .collect::<Vec<_>>()
                .await;
            if results.iter().any(Result::is_err) {
                self.health.degraded.store(true, Ordering::Release);
                if !wait_after_failed_attempt(&self.shutdown).await {
                    break;
                }
            }
        }
    }

    async fn poll_one(
        &self,
        state: &AppState,
        m3: &Arc<M3Services>,
        m4: &Arc<M4Services>,
        due: DueEntry,
    ) -> Result<(), ()> {
        let metadata = m3.store.read_metadata(due.app_id).map_err(|_| ())?;
        let generation = poll_generation(&m3.store, &m4.credentials, &metadata).map_err(|_| ())?;
        if !metadata.auto_deploy_enabled || generation != due.generation {
            return Ok(());
        }
        let latest_deployment = m4
            .ledger
            .list(metadata.id, 1)
            .await
            .map_err(|_| ())?
            .into_iter()
            .next();
        if latest_deployment
            .as_ref()
            .is_some_and(|deployment| deployment.status == DeploymentStatus::NeedsAttention)
        {
            return self
                .record_error(
                    &metadata,
                    &generation,
                    PollOutcome::BlockedAttention,
                    "attention",
                    "DEPLOYMENT_NEEDS_ATTENTION",
                    0,
                )
                .await;
        }
        let image = match ImageReference::parse(&metadata.discovery_image_ref) {
            Ok(value) => value,
            Err(error) => {
                return self
                    .record_error(
                        &metadata,
                        &generation,
                        PollOutcome::InvalidSource,
                        "deterministic",
                        error.public_code(),
                        0,
                    )
                    .await;
            }
        };
        let credential = metadata
            .credential_ref
            .map(|id| m4.credentials.load(id))
            .transpose()
            .map_err(|_| ())?;
        if credential
            .as_ref()
            .is_some_and(|value| value.metadata.registry != image.logical_registry)
        {
            return self
                .record_error(
                    &metadata,
                    &generation,
                    PollOutcome::CredentialError,
                    "credential",
                    "REGISTRY_CREDENTIAL_MISMATCH",
                    0,
                )
                .await;
        }
        let probe = state.observer.api().probe().await.map_err(|_| ())?;
        let platform = Platform::canonical(
            probe.os.as_deref().ok_or(())?,
            probe.architecture.as_deref().ok_or(())?,
            None,
        )
        .map_err(|_| ())?;
        let previous = self.store.get(metadata.id).await.map_err(|_| ())?;
        let conditional_etag = previous
            .as_ref()
            .filter(|value| value.generation == generation)
            .and_then(|value| value.last_etag.as_deref());
        let resolved = tokio::select! {
            () = self.shutdown.cancelled() => return Ok(()),
            value = m4.engine.resolver.resolve_poll(&image, &platform, credential.as_ref().map(|value| (value.metadata.username.as_str(), &value.secret)), conditional_etag) => value,
        };
        let (resolved, response_etag) = match resolved {
            Ok(PollResolve::Modified { image, etag }) => (*image, etag),
            Ok(PollResolve::NotModified) => {
                let fresh = m3.store.read_metadata(metadata.id).map_err(|_| ())?;
                if poll_generation(&m3.store, &m4.credentials, &fresh).map_err(|_| ())?
                    != generation
                    || !fresh.auto_deploy_enabled
                {
                    return Ok(());
                }
                let active = m3
                    .store
                    .read_release_link(metadata.id, "active")
                    .map_err(|_| ())?;
                let pending = m3
                    .store
                    .read_release_link(metadata.id, "pending")
                    .map_err(|_| ())?;
                let actual = poll_actual_fact(state, metadata.id, active)
                    .await
                    .ok()
                    .flatten();
                if let (Some(active_id), Some(previous)) = (active, previous.as_ref()) {
                    let release = m3
                        .store
                        .load_v2_release(metadata.id, active_id)
                        .map_err(|_| ())?;
                    if pending.is_none()
                        && actual.as_ref().map(|value| value.0) == Some(active_id)
                        && previous.last_manifest_digest.as_deref()
                            == Some(release.manifest_digest.as_str())
                    {
                        let outcome = if release.config_revision == fresh.draft_revision
                            && release.config_sha256 == fresh.draft_config_sha256
                        {
                            PollOutcome::Unchanged
                        } else {
                            PollOutcome::ConfigPendingManual
                        };
                        return self.record_not_modified(&fresh, &generation, outcome).await;
                    }
                }
                let image = tokio::select! {
                    () = self.shutdown.cancelled() => return Ok(()),
                    value = m4.engine.resolver.resolve(&image, &platform, credential.as_ref().map(|value| (value.metadata.username.as_str(), &value.secret))) => value.map_err(|_| ())?,
                };
                (image, None)
            }
            Err(error) => {
                let transient = matches!(
                    error,
                    RegistryError::Timeout
                        | RegistryError::Unavailable
                        | RegistryError::RateLimited
                );
                let failures = if transient {
                    previous
                        .map(|value| value.consecutive_transient_failures)
                        .unwrap_or(0)
                        .saturating_add(1)
                        .min(5)
                } else {
                    0
                };
                let outcome = if matches!(
                    error,
                    RegistryError::CredentialRequired
                        | RegistryError::CredentialInvalid
                        | RegistryError::Forbidden
                ) {
                    PollOutcome::CredentialError
                } else {
                    PollOutcome::RegistryError
                };
                return self
                    .record_error(
                        &metadata,
                        &generation,
                        outcome,
                        if transient {
                            "transient"
                        } else {
                            "deterministic"
                        },
                        error.public_code(),
                        failures,
                    )
                    .await;
            }
        };
        let fresh = m3.store.read_metadata(metadata.id).map_err(|_| ())?;
        if poll_generation(&m3.store, &m4.credentials, &fresh).map_err(|_| ())? != generation
            || !fresh.auto_deploy_enabled
        {
            return Ok(());
        }
        let active = m3
            .store
            .read_release_link(metadata.id, "active")
            .map_err(|_| ())?;
        let pending = m3
            .store
            .read_release_link(metadata.id, "pending")
            .map_err(|_| ())?;
        let actual = match poll_actual_fact(state, metadata.id, active).await {
            Ok(value) => value,
            Err(_) => {
                return self
                    .record_success(
                        &fresh,
                        &generation,
                        &resolved,
                        response_etag.as_deref(),
                        PollOutcome::BlockedDrift,
                        None,
                    )
                    .await;
            }
        };
        if let Some(active_id) = active {
            let release = m3
                .store
                .load_v2_release(metadata.id, active_id)
                .map_err(|_| ())?;
            if release.manifest_digest == resolved.manifest_digest {
                if pending.is_some() || actual.as_ref().map(|value| value.0) != Some(active_id) {
                    return self
                        .record_success(
                            &fresh,
                            &generation,
                            &resolved,
                            response_etag.as_deref(),
                            PollOutcome::BlockedDrift,
                            None,
                        )
                        .await;
                }
                let outcome = if release.config_revision == fresh.draft_revision
                    && release.config_sha256 == fresh.draft_config_sha256
                {
                    PollOutcome::Unchanged
                } else {
                    PollOutcome::ConfigPendingManual
                };
                let _ = self
                    .store
                    .clear_suppression_if_target(metadata.id, &target_key(&fresh, &resolved))
                    .await;
                return self
                    .record_success(
                        &fresh,
                        &generation,
                        &resolved,
                        response_etag.as_deref(),
                        outcome,
                        None,
                    )
                    .await;
            }
        }
        let target_key = target_key(&fresh, &resolved);
        if self
            .store
            .get(metadata.id)
            .await
            .map_err(|_| ())?
            .as_ref()
            .and_then(|value| value.suppressed_target_key.as_deref())
            == Some(target_key.as_str())
        {
            return self
                .record_success(
                    &fresh,
                    &generation,
                    &resolved,
                    response_etag.as_deref(),
                    PollOutcome::SuppressedFailedTarget,
                    None,
                )
                .await;
        }
        if let Some(pending_id) = pending {
            let pending_release = m3
                .store
                .load_v2_release(metadata.id, pending_id)
                .map_err(|_| ())?;
            let actual_release = actual.as_ref().map(|value| value.0);
            if pending_release.manifest_digest != resolved.manifest_digest
                || (actual_release != Some(pending_id) && actual_release != active)
            {
                return self
                    .record_success(
                        &fresh,
                        &generation,
                        &resolved,
                        response_etag.as_deref(),
                        PollOutcome::BlockedAttention,
                        None,
                    )
                    .await;
            }
        } else if actual.as_ref().is_some_and(|value| Some(value.0) != active) {
            return self
                .record_success(
                    &fresh,
                    &generation,
                    &resolved,
                    response_etag.as_deref(),
                    PollOutcome::BlockedDrift,
                    None,
                )
                .await;
        }
        let convergence_attempt = latest_deployment.as_ref().filter(|deployment| {
            deployment.trigger == DeploymentTrigger::Poll
                && deployment.poll_generation.as_deref() == Some(generation.as_str())
                && deployment.scheduled_target_key.as_deref() == Some(target_key.as_str())
        });
        if convergence_attempt.is_some_and(|deployment| {
            matches!(
                deployment.status,
                DeploymentStatus::Queued | DeploymentStatus::Running
            )
        }) {
            return self
                .record_success(
                    &fresh,
                    &generation,
                    &resolved,
                    response_etag.as_deref(),
                    PollOutcome::BusySkipped,
                    None,
                )
                .await;
        }
        let convergence_suffix = convergence_attempt
            .filter(|deployment| deployment.status == DeploymentStatus::Interrupted)
            .map(|deployment| format!(":converge:{}", deployment.id))
            .unwrap_or_default();
        let key_bytes = m3.idempotency.fingerprint(
            format!(
                "{}:{generation}:{target_key}{convergence_suffix}",
                metadata.id
            )
            .as_bytes(),
        );
        let key = hex(&key_bytes);
        let fingerprint = m3.idempotency.fingerprint(
            format!(
                "poll:{}:{generation}:{target_key}{convergence_suffix}",
                metadata.id
            )
            .as_bytes(),
        );
        let scheduled = ScheduledResolvedTarget {
            image: resolved.clone(),
            generation: generation.clone(),
            target_key: target_key.clone(),
        };
        let command = ScheduleCommand {
            route: format!("/internal/poll/{}", metadata.id),
            idempotency_key: key,
            fingerprint,
            request_id: Uuid::new_v4(),
            app_id: metadata.id,
            trigger: DeploymentTrigger::Poll,
            facts: ScheduleFacts {
                draft_revision: fresh.draft_revision,
                active_release_id: active,
                pending_release_id: pending,
                actual_release_id: actual.as_ref().map(|value| value.0),
                actual_container_id: actual.as_ref().map(|value| value.1.clone()),
                acknowledge_non_rollbackable_data: true,
            },
            rollback_target: None,
            rollback_of: None,
            scheduled: Some(scheduled),
        };
        // Publish the exact target before spawning the deployment worker so a
        // fast terminal transition can durably attach failed-target
        // suppression to this generation in the same ledger transaction.
        self.record_success(
            &fresh,
            &generation,
            &resolved,
            response_etag.as_deref(),
            PollOutcome::Scheduled,
            None,
        )
        .await?;
        let outcome = match m4
            .scheduler
            .schedule(state.clone(), m3.clone(), command)
            .await
        {
            Ok(ScheduleResult::Scheduled(_)) => return Ok(()),
            Ok(ScheduleResult::Replay { .. }) => PollOutcome::BusySkipped,
            Err(ScheduleError::Busy | ScheduleError::FactsChanged) => PollOutcome::BusySkipped,
            Err(ScheduleError::ContainerInvalid) => PollOutcome::BlockedDrift,
            Err(_) => return Err(()),
        };
        self.record_success(
            &fresh,
            &generation,
            &resolved,
            response_etag.as_deref(),
            outcome,
            None,
        )
        .await
    }

    async fn record_success(
        &self,
        metadata: &crate::domain::AppMetadata,
        generation: &str,
        resolved: &ResolvedImage,
        etag: Option<&str>,
        outcome: PollOutcome,
        _deployment: Option<Uuid>,
    ) -> Result<(), ()> {
        let checked = OffsetDateTime::now_utc();
        let next = checked
            + time::Duration::seconds(i64::from(interval_with_jitter(
                generation.as_bytes(),
                metadata.id,
                generation,
                metadata.poll_interval_seconds,
                0,
            )));
        self.store
            .publish(
                metadata.id,
                PollObservation {
                    generation,
                    enabled: true,
                    next_check_not_before: Some(next),
                    checked_at: Some(checked),
                    success: true,
                    replace_observed_fields: true,
                    source_descriptor_digest: Some(&resolved.source_descriptor_digest),
                    etag,
                    manifest_digest: Some(&resolved.manifest_digest),
                    platform: Some(&format_platform(&resolved.platform)),
                    outcome,
                    error_class: None,
                    error_code: None,
                    transient_failures: 0,
                },
            )
            .await
            .map_err(|_| ())
    }

    async fn record_not_modified(
        &self,
        metadata: &crate::domain::AppMetadata,
        generation: &str,
        outcome: PollOutcome,
    ) -> Result<(), ()> {
        let checked = OffsetDateTime::now_utc();
        let next = checked
            + time::Duration::seconds(i64::from(interval_with_jitter(
                generation.as_bytes(),
                metadata.id,
                generation,
                metadata.poll_interval_seconds,
                0,
            )));
        self.store
            .publish(
                metadata.id,
                PollObservation {
                    generation,
                    enabled: true,
                    next_check_not_before: Some(next),
                    checked_at: Some(checked),
                    success: true,
                    replace_observed_fields: false,
                    source_descriptor_digest: None,
                    etag: None,
                    manifest_digest: None,
                    platform: None,
                    outcome,
                    error_class: None,
                    error_code: None,
                    transient_failures: 0,
                },
            )
            .await
            .map_err(|_| ())
    }

    async fn record_error(
        &self,
        metadata: &crate::domain::AppMetadata,
        generation: &str,
        outcome: PollOutcome,
        class: &str,
        code: &str,
        failures: u8,
    ) -> Result<(), ()> {
        let checked = OffsetDateTime::now_utc();
        let seconds = if failures > 0 {
            [60, 120, 300, 600, 1800][usize::from(failures.saturating_sub(1))]
        } else if outcome == PollOutcome::CredentialError {
            metadata.poll_interval_seconds.max(1800)
        } else {
            metadata.poll_interval_seconds
        };
        let seconds = interval_with_jitter(
            generation.as_bytes(),
            metadata.id,
            generation,
            seconds,
            u64::from(failures),
        );
        self.store
            .publish(
                metadata.id,
                PollObservation {
                    generation,
                    enabled: true,
                    next_check_not_before: Some(
                        checked + time::Duration::seconds(i64::from(seconds)),
                    ),
                    checked_at: Some(checked),
                    success: false,
                    replace_observed_fields: false,
                    source_descriptor_digest: None,
                    etag: None,
                    manifest_digest: None,
                    platform: None,
                    outcome,
                    error_class: Some(class),
                    error_code: Some(code),
                    transient_failures: failures,
                },
            )
            .await
            .map_err(|_| ())
    }
}

fn webhook_must_respect_backoff(state: &PollState, now: OffsetDateTime) -> bool {
    state
        .next_check_not_before
        .is_some_and(|deadline| deadline > now)
        && (state.consecutive_transient_failures > 0
            || state.last_outcome == PollOutcome::CredentialError)
}

fn webhook_jitter(app_id: Uuid, sequence: i64) -> u32 {
    let mut digest = Sha256::new();
    digest.update(app_id.as_bytes());
    digest.update(sequence.to_be_bytes());
    u32::from(digest.finalize()[0] % 6)
}

async fn wait_after_failed_attempt(shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => false,
        () = tokio::time::sleep(Duration::from_secs(FAILURE_RETRY_SECONDS)) => true,
    }
}

struct PollRunGuard {
    health: PollHealth,
    shutdown: CancellationToken,
}

impl Drop for PollRunGuard {
    fn drop(&mut self) {
        self.health.running.store(false, Ordering::Release);
        if !self.shutdown.is_cancelled() {
            self.health.degraded.store(true, Ordering::Release);
        }
    }
}

pub fn poll_generation(
    store: &AppStore,
    credentials: &CredentialStore,
    metadata: &crate::domain::AppMetadata,
) -> Result<String, StoreError> {
    let credential = metadata
        .credential_ref
        .map(|id| credentials.load(id))
        .transpose()?;
    let canonical = serde_json::to_vec(&serde_json::json!({
        "app_id":metadata.id,"draft_revision":metadata.draft_revision,"draft_config_sha256":metadata.draft_config_sha256,
        "source":ImageReference::parse(&metadata.discovery_image_ref).map_err(|_|StoreError::ContentInvalid)?.canonical_tagged_ref,
        "credential":credential.as_ref().map(|value| (&value.metadata.id,&value.metadata.revision,&value.metadata.secret_revision)),
        "auto":metadata.auto_deploy_enabled,"interval":metadata.poll_interval_seconds,
    })).map_err(|_| StoreError::ContentInvalid)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(store.integrity_key()?)
        .map_err(|_| StoreError::ContentInvalid)?;
    mac.update(&canonical);
    Ok(hex(&mac.finalize().into_bytes()))
}

fn initial_jitter(store: &AppStore, app_id: Uuid, generation: &str, interval: u32) -> u32 {
    interval_with_jitter(
        store.integrity_key().unwrap_or_default(),
        app_id,
        generation,
        interval,
        0,
    )
}

fn clamp_persisted_due(
    due: OffsetDateTime,
    now: OffsetDateTime,
    configured_interval: u32,
) -> OffsetDateTime {
    let latest_expected =
        now + time::Duration::seconds(i64::from(configured_interval.saturating_add(30)));
    if due > latest_expected {
        now + time::Duration::minutes(30)
    } else {
        due.max(now)
    }
}

fn interval_with_jitter(
    key: &[u8],
    app_id: Uuid,
    generation: &str,
    interval: u32,
    ordinal: u64,
) -> u32 {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts every key length");
    mac.update(app_id.as_bytes());
    mac.update(generation.as_bytes());
    mac.update(&ordinal.to_be_bytes());
    let bytes = mac.finalize().into_bytes();
    let span = (interval / 10).min(30);
    let offset = if span == 0 {
        0
    } else {
        i32::from(bytes[0]) % (i32::try_from(span * 2 + 1).unwrap_or(1))
            - i32::try_from(span).unwrap_or(0)
    };
    u32::try_from((i64::from(interval) + i64::from(offset)).max(1)).unwrap_or(interval)
}

fn target_key(metadata: &crate::domain::AppMetadata, resolved: &ResolvedImage) -> String {
    hex(&Sha256::digest(
        format!(
            "{}:{}:{}:{}/{}/{}",
            metadata.id,
            metadata.draft_config_sha256,
            resolved.manifest_digest,
            resolved.platform.os,
            resolved.platform.architecture,
            resolved.platform.variant.as_deref().unwrap_or("")
        )
        .as_bytes(),
    ))
}
fn format_platform(platform: &Platform) -> String {
    match platform.variant.as_deref() {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}
fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(H[(byte >> 4) as usize] as char);
        out.push(H[(byte & 15) as usize] as char);
    }
    out
}

async fn poll_actual_fact(
    state: &AppState,
    app_id: Uuid,
    active: Option<Uuid>,
) -> Result<Option<(Uuid, String)>, ()> {
    let project = crate::domain::AppMetadata::project_name(app_id);
    let candidates = state
        .observer
        .api()
        .list_compose_app_containers(&project)
        .await
        .map_err(|_| ())?;
    if candidates.len() > 1 {
        return Err(());
    }
    let Some(container) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let app = AppCatalogEntry {
        id: app_id,
        slug: String::new(),
        display_name: String::new(),
        project_name: project,
        active_release_id: active,
        active_image_ref: None,
        active_config_revision: None,
        active_config_sha256: None,
        pending_release_id: None,
        pending_image_ref: None,
        pending_config_revision: None,
        discovery_image_ref: None,
        draft_revision: None,
        draft_config_sha256: None,
        desired_state: crate::domain::DesiredState::Stopped,
        auto_deploy_enabled: false,
        poll_interval_seconds: 300,
        draft: None,
    };
    let identity = validate_syntactic_identity(&container.labels, &app).ok_or(())?;
    Ok(Some((identity.release_id, container.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn failed_attempt_backoff_is_bounded_and_cancellation_aware() {
        let shutdown = CancellationToken::new();
        let delayed = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { wait_after_failed_attempt(&shutdown).await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(FAILURE_RETRY_SECONDS - 1)).await;
        assert!(!delayed.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(delayed.await.unwrap());

        let cancelled = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { wait_after_failed_attempt(&shutdown).await }
        });
        tokio::task::yield_now().await;
        shutdown.cancel();
        assert!(!cancelled.await.unwrap());
    }

    #[test]
    fn jitter_is_stable_and_bounded() {
        let app = Uuid::new_v4();
        let a = interval_with_jitter(b"key", app, "generation", 300, 1);
        let b = interval_with_jitter(b"key", app, "generation", 300, 1);
        assert_eq!(a, b);
        assert!((270..=330).contains(&a));
    }

    #[test]
    fn persisted_due_preserves_long_intervals_but_clamps_clock_anomalies() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
        assert_eq!(
            clamp_persisted_due(now + time::Duration::hours(12), now, 43_200),
            now + time::Duration::hours(12)
        );
        assert_eq!(
            clamp_persisted_due(now + time::Duration::days(5), now, 300),
            now + time::Duration::minutes(30)
        );
        assert_eq!(
            clamp_persisted_due(now - time::Duration::hours(1), now, 300),
            now
        );
    }

    #[test]
    fn webhook_does_not_bypass_credential_or_transient_backoff() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
        let mut state = PollState {
            app_id: Uuid::new_v4(),
            generation: "generation".into(),
            enabled: true,
            consecutive_transient_failures: 0,
            next_check_not_before: Some(now + time::Duration::minutes(30)),
            last_checked_at: None,
            last_success_at: None,
            last_source_descriptor_digest: None,
            last_etag: None,
            last_manifest_digest: None,
            last_platform: None,
            last_outcome: PollOutcome::CredentialError,
            last_error_class: Some("credential".into()),
            last_error_code: Some("REGISTRY_CREDENTIAL_INVALID".into()),
            suppressed_target_key: None,
            suppressed_deployment_id: None,
            webhook_sequence: 1,
            webhook_processed_sequence: 0,
            last_webhook_received_at: Some(now),
            last_wake_source: Some("webhook".into()),
            updated_at: now,
        };
        assert!(webhook_must_respect_backoff(&state, now));
        state.last_outcome = PollOutcome::Unchanged;
        assert!(!webhook_must_respect_backoff(&state, now));
        state.consecutive_transient_failures = 1;
        assert!(webhook_must_respect_backoff(&state, now));
        state.next_check_not_before = Some(now);
        assert!(!webhook_must_respect_backoff(&state, now));
    }
}
