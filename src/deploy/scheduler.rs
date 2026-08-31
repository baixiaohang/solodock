use std::sync::Arc;

use uuid::Uuid;

use crate::{
    api::{AppState, mutations::M3Services},
    app_store::config_revision,
    docker::ownership::validate_syntactic_identity,
    mutation::{ClaimResult, IdempotencyError},
};

use super::{DeploymentEngine, DeploymentTrigger, ScheduledResolvedTarget};

#[derive(Clone, Debug)]
pub struct ScheduleFacts {
    pub draft_revision: Uuid,
    pub active_release_id: Option<Uuid>,
    pub pending_release_id: Option<Uuid>,
    pub actual_release_id: Option<Uuid>,
    pub actual_container_id: Option<String>,
    pub acknowledge_non_rollbackable_data: bool,
}

#[derive(Clone, Debug)]
pub struct ScheduleCommand {
    pub route: String,
    pub idempotency_key: String,
    pub fingerprint: Vec<u8>,
    pub request_id: Uuid,
    pub app_id: Uuid,
    pub trigger: DeploymentTrigger,
    pub facts: ScheduleFacts,
    pub rollback_target: Option<Uuid>,
    pub rollback_of: Option<Uuid>,
    pub scheduled: Option<ScheduledResolvedTarget>,
}

#[derive(Debug)]
pub enum ScheduleResult {
    Scheduled(Uuid),
    Replay { status: u16, body: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("application was not found")]
    AppNotFound,
    #[error("deployment is busy")]
    Busy,
    #[error("deployment facts changed")]
    FactsChanged,
    #[error("non-rollbackable data acknowledgement is required")]
    DataAckRequired,
    #[error("container identity is invalid")]
    ContainerInvalid,
    #[error("Docker observation failed: {0}")]
    Docker(&'static str),
    #[error("application configuration is invalid")]
    ConfigInvalid,
    #[error("idempotency failed")]
    Idempotency(#[from] IdempotencyError),
    #[error("deployment scheduling failed")]
    Internal,
}

#[derive(Clone)]
pub struct DeploymentScheduler {
    engine: DeploymentEngine,
}

impl DeploymentScheduler {
    pub fn new(engine: DeploymentEngine) -> Self {
        Self { engine }
    }

    pub async fn schedule(
        &self,
        state: AppState,
        m3: Arc<M3Services>,
        command: ScheduleCommand,
    ) -> Result<ScheduleResult, ScheduleError> {
        if state.shutdown.is_cancelled() {
            return Err(ScheduleError::Busy);
        }
        let actor = if command.trigger == DeploymentTrigger::Poll {
            "system"
        } else {
            "admin"
        };
        if let Some(ClaimResult::Replay { status, body, .. }) = m3
            .idempotency
            .completed_for_actor(
                actor,
                &command.route,
                &command.idempotency_key,
                &command.fingerprint,
            )
            .await?
        {
            return Ok(ScheduleResult::Replay { status, body });
        }
        let app_guard = m3
            .coordinator
            .try_app(command.app_id)
            .map_err(|_| ScheduleError::Busy)?;
        let global_guard = m3
            .coordinator
            .try_compose_owned()
            .map_err(|_| ScheduleError::Busy)?;
        let metadata = m3
            .store
            .read_metadata(command.app_id)
            .map_err(|_| ScheduleError::AppNotFound)?;
        let active = m3
            .store
            .read_release_link(command.app_id, "active")
            .map_err(|_| ScheduleError::Internal)?;
        let pending = m3
            .store
            .read_release_link(command.app_id, "pending")
            .map_err(|_| ScheduleError::Internal)?;
        let actual = actual_fact(&state, command.app_id, active).await?;
        if metadata.draft_revision != command.facts.draft_revision
            || active != command.facts.active_release_id
            || pending != command.facts.pending_release_id
            || actual.as_ref().map(|value| value.0) != command.facts.actual_release_id
            || actual.as_ref().map(|value| value.1.as_str())
                != command.facts.actual_container_id.as_deref()
        {
            return Err(ScheduleError::FactsChanged);
        }
        if command.trigger == DeploymentTrigger::Poll {
            let scheduled = command
                .scheduled
                .as_ref()
                .ok_or(ScheduleError::ConfigInvalid)?;
            if !metadata.auto_deploy_enabled
                || scheduled.generation
                    != crate::registry::poller::poll_generation(
                        &m3.store,
                        &self.engine.credentials,
                        &metadata,
                    )
                    .map_err(|_| ScheduleError::ConfigInvalid)?
            {
                return Err(ScheduleError::FactsChanged);
            }
        }
        let draft = config_revision::load_verified(
            &m3.store.app_directory(command.app_id),
            metadata.draft_revision,
            m3.store
                .integrity_key()
                .map_err(|_| ScheduleError::Internal)?,
        )
        .map_err(|_| ScheduleError::ConfigInvalid)?;
        if (!draft.metadata.volumes.is_empty() || !draft.metadata.binds.is_empty())
            && !command.facts.acknowledge_non_rollbackable_data
        {
            return Err(ScheduleError::DataAckRequired);
        }
        let claim = m3
            .idempotency
            .claim_deployment(
                &command.route,
                &command.idempotency_key,
                &command.fingerprint,
                command.request_id,
                command.app_id,
                command.trigger.as_str(),
                metadata.draft_revision,
                active,
                pending,
                actual.as_ref().map(|value| value.0),
                actual.as_ref().map(|value| value.1.as_str()),
                command.rollback_target,
                command.rollback_of,
                command.scheduled.as_ref(),
            )
            .await?;
        match claim {
            ClaimResult::Replay { status, body, .. } => Ok(ScheduleResult::Replay { status, body }),
            ClaimResult::New(deployment_id) => {
                self.engine
                    .spawn(state, m3, deployment_id, app_guard, global_guard);
                Ok(ScheduleResult::Scheduled(deployment_id))
            }
            ClaimResult::Resume(_) => Err(ScheduleError::Busy),
        }
    }
}

async fn actual_fact(
    state: &AppState,
    app_id: Uuid,
    active: Option<Uuid>,
) -> Result<Option<(Uuid, String)>, ScheduleError> {
    let mut app = state
        .observer
        .catalog
        .get(app_id)
        .ok_or(ScheduleError::AppNotFound)?;
    app.active_release_id = active;
    let candidates = state
        .observer
        .api()
        .list_compose_app_containers(&app.project_name)
        .await
        .map_err(|error| ScheduleError::Docker(error.public_code()))?;
    if candidates.len() > 1 {
        return Err(ScheduleError::ContainerInvalid);
    }
    let Some(container) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let identity = validate_syntactic_identity(&container.labels, &app)
        .ok_or(ScheduleError::ContainerInvalid)?;
    Ok(Some((identity.release_id, container.id)))
}
