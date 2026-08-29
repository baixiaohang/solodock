use std::sync::Arc;

use axum::{
    Json,
    extract::{Extension, Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{
    AppState,
    auth::{Authenticated, MutationAuthenticated},
    mutations::M3Services,
};
use crate::{
    deploy::{
        DeploymentEngine, DeploymentLedger, DeploymentRecord, DeploymentStatus,
        DeploymentTransition, DeploymentTrigger,
    },
    docker::ownership::validate_syntactic_identity,
    error::{ApiError, RequestId},
    mutation::{ClaimResult, IdempotencyError},
    registry::{CredentialMetadata, CredentialStore},
    security::secret::SecretValue,
};

pub struct M4Services {
    pub credentials: CredentialStore,
    pub ledger: DeploymentLedger,
    pub engine: DeploymentEngine,
}

#[derive(Serialize)]
pub struct CredentialResponse {
    id: Uuid,
    registry: String,
    username: String,
    revision: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    rotated_at: time::OffsetDateTime,
    referenced_by_apps: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCredential {
    registry: String,
    username: String,
    secret: SecretValue,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CredentialSecretOperation {
    Keep,
    Replace,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCredential {
    expected_revision: Uuid,
    username: String,
    secret_operation: CredentialSecretOperation,
    #[serde(default)]
    secret: Option<SecretValue>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCredential {
    expected_revision: Uuid,
}

pub async fn list_credentials(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    _auth: Authenticated,
) -> Result<Json<Vec<CredentialResponse>>, ApiError> {
    let services = m4(&state, request_id)?;
    let values = services
        .credentials
        .list()
        .map_err(|_| ApiError::internal(request_id))?;
    Ok(Json(
        values
            .into_iter()
            .map(|value| credential_response(&state, value))
            .collect(),
    ))
}

pub async fn create_credential(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<CreateCredential>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = m3(&state, request_id)?;
    let m4 = m4(&state, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let route = "/api/v1/registry-credentials";
    let raw = idempotency_key(&headers, request_id)?;
    let mut canonical = serde_json::to_vec(&serde_json::json!({"method":"POST","route":route,"registry":payload.registry,"username":payload.username,"secret_sha256":format!("{:x}",sha2::Sha256::digest(payload.secret.expose().as_bytes()))})).map_err(|_| ApiError::internal(request_id))?;
    let fingerprint = m3.idempotency.fingerprint(&canonical);
    canonical.zeroize();
    let claim = m3
        .idempotency
        .claim(route, raw, &fingerprint, request_id.0)
        .await
        .map_err(|e| ApiError::idempotency(e, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body);
    }
    let operation = match claim {
        ClaimResult::New(v) | ClaimResult::Resume(v) => v,
        _ => unreachable!(),
    };
    let _catalog = m3.coordinator.catalog_lock().await;
    let metadata = match m4.credentials.create(
        operation,
        operation,
        &payload.registry,
        &payload.username,
        &payload.secret,
    ) {
        Ok(v) => v,
        Err(crate::app_store::StoreError::ContentInvalid)
        | Err(crate::app_store::StoreError::AppConflict) => {
            return finish_error(
                m3,
                route,
                raw,
                "CREDENTIAL_INVALID",
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt_credential(m3, route, raw, request_id).await,
    };
    let _ = crate::api::mutations::refresh(&state, m3).await;
    finish_json(
        m3,
        route,
        raw,
        StatusCode::CREATED,
        &credential_response(&state, metadata),
        request_id,
    )
    .await
}

pub async fn update_credential(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<UpdateCredential>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = m3(&state, request_id)?;
    let m4 = m4(&state, request_id)?;
    let Json(mut payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    if matches!(payload.secret_operation, CredentialSecretOperation::Replace)
        != payload.secret.is_some()
    {
        return Err(ApiError::validation(request_id));
    }
    let route = format!("/api/v1/registry-credentials/{id}");
    let raw = idempotency_key(&headers, request_id)?;
    let mut canonical=serde_json::to_vec(&serde_json::json!({"method":"PUT","route":route,"expected_revision":payload.expected_revision,"username":payload.username,"secret_operation":match payload.secret_operation { CredentialSecretOperation::Keep=>"keep",CredentialSecretOperation::Replace=>"replace"},"secret_sha256":payload.secret.as_ref().map(|v|format!("{:x}",sha2::Sha256::digest(v.expose().as_bytes())))})).map_err(|_|ApiError::internal(request_id))?;
    let fingerprint = m3.idempotency.fingerprint(&canonical);
    canonical.zeroize();
    let claim = m3
        .idempotency
        .claim(&route, raw, &fingerprint, request_id.0)
        .await
        .map_err(|e| ApiError::idempotency(e, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body);
    }
    let operation = match claim {
        ClaimResult::New(v) | ClaimResult::Resume(v) => v,
        _ => unreachable!(),
    };
    let _catalog = m3.coordinator.catalog_lock().await;
    let replacement = payload.secret.take();
    let metadata = match m4.credentials.update(
        id,
        payload.expected_revision,
        operation,
        &payload.username,
        replacement.as_ref(),
    ) {
        Ok(v) => v,
        Err(crate::app_store::StoreError::RevisionStale) => {
            return finish_error(
                m3,
                &route,
                raw,
                "CREDENTIAL_REVISION_STALE",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Err(crate::app_store::StoreError::ContentInvalid)
        | Err(crate::app_store::StoreError::AppConflict) => {
            return finish_error(
                m3,
                &route,
                raw,
                "CREDENTIAL_INVALID",
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt_credential(m3, &route, raw, request_id).await,
    };
    let _ = crate::api::mutations::refresh(&state, m3).await;
    finish_json(
        m3,
        &route,
        raw,
        StatusCode::OK,
        &credential_response(&state, metadata),
        request_id,
    )
    .await
}

pub async fn delete_credential(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<DeleteCredential>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = m3(&state, request_id)?;
    let m4 = m4(&state, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let route = format!("/api/v1/registry-credentials/{id}");
    let raw = idempotency_key(&headers, request_id)?;
    let fingerprint = m3.idempotency.fingerprint(
        serde_json::to_string(&(&route, payload.expected_revision))
            .map_err(|_| ApiError::internal(request_id))?
            .as_bytes(),
    );
    let claim = m3
        .idempotency
        .claim(&route, raw, &fingerprint, request_id.0)
        .await
        .map_err(|e| ApiError::idempotency(e, request_id))?;
    if let ClaimResult::Replay {
        operation_id: _,
        status,
        body,
    } = claim
    {
        let _catalog = m3.coordinator.catalog_lock().await;
        finalize_credential_or_reconcile(m3, m4).await;
        return replay(status, body);
    }
    let operation = match claim {
        ClaimResult::New(v) | ClaimResult::Resume(v) => v,
        _ => unreachable!(),
    };
    let _catalog = m3.coordinator.catalog_lock().await;
    let exact_tombstone = match m4.credentials.exact_tombstone(id, operation) {
        Ok(value) => value,
        Err(_) => return interrupt_credential(m3, &route, raw, request_id).await,
    };
    if exact_tombstone.is_some() {
        let response = serde_json::json!({"id":id,"deleted":true});
        let completed = finish_json(m3, &route, raw, StatusCode::OK, &response, request_id).await?;
        finalize_credential_or_reconcile(m3, m4).await;
        let _ = crate::api::mutations::refresh(&state, m3).await;
        return Ok(completed);
    }
    let metadata = match m4.credentials.list() {
        Ok(values) => values,
        Err(_) => return interrupt_credential(m3, &route, raw, request_id).await,
    }
    .into_iter()
    .find(|v| v.id == id);
    let Some(metadata) = metadata else {
        return finish_error(
            m3,
            &route,
            raw,
            "CREDENTIAL_NOT_FOUND",
            StatusCode::NOT_FOUND,
            request_id,
        )
        .await;
    };
    if metadata.revision != payload.expected_revision {
        return finish_error(
            m3,
            &route,
            raw,
            "CREDENTIAL_REVISION_STALE",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let in_use = match credential_in_use(&state, m3, id) {
        Ok(value) => value,
        Err(code) => {
            return finish_error(m3, &route, raw, code, StatusCode::CONFLICT, request_id).await;
        }
    };
    if in_use {
        return finish_error(
            m3,
            &route,
            raw,
            "CREDENTIAL_IN_USE",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    if m4.credentials.tombstone(id, operation).is_err() {
        return interrupt_credential(m3, &route, raw, request_id).await;
    }
    let _ = crate::api::mutations::refresh(&state, m3).await;
    let response = finish_json(
        m3,
        &route,
        raw,
        StatusCode::OK,
        &serde_json::json!({"id":id,"deleted":true}),
        request_id,
    )
    .await?;
    finalize_credential_or_reconcile(m3, m4).await;
    Ok(response)
}

async fn finalize_credential_or_reconcile(m3: &M3Services, m4: &M4Services) {
    if m3
        .idempotency
        .finalize_succeeded_credential_tombstones(&m4.credentials)
        .await
        .is_err()
    {
        m3.projection_degraded
            .store(true, std::sync::atomic::Ordering::Release);
        m3.reconcile_notify.notify_one();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    expected_draft_revision: Uuid,
    expected_active_release_id: Option<Uuid>,
    expected_pending_release_id: Option<Uuid>,
    expected_actual_release_id: Option<Uuid>,
    expected_actual_container_id: Option<String>,
    acknowledge_non_rollbackable_data: bool,
}
#[derive(Serialize)]
struct ScheduleResponse {
    deployment_id: Uuid,
    status: DeploymentStatus,
    idempotency_replayed: bool,
    detail_url: String,
}

pub async fn schedule(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<ScheduleRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    schedule_inner(
        state,
        request_id,
        app_id,
        headers,
        payload,
        DeploymentTrigger::Manual,
        None,
        None,
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequest {
    expected_active_release_id: Option<Uuid>,
    expected_pending_release_id: Option<Uuid>,
    expected_actual_release_id: Option<Uuid>,
    expected_actual_container_id: Option<String>,
    acknowledge_non_rollbackable_data: bool,
}
pub async fn rollback(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(source_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<RollbackRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(value) = payload.map_err(|_| ApiError::validation(request_id))?;
    if !value.acknowledge_non_rollbackable_data {
        return Err(ApiError::validation(request_id));
    }
    let m4 = m4(&state, request_id)?;
    let source = m4
        .ledger
        .get(source_id)
        .await
        .map_err(|_| ApiError::internal(request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "DEPLOYMENT_NOT_FOUND",
                "The deployment was not found",
                request_id,
            )
        })?;
    let target = source
        .candidate_release_id
        .ok_or_else(|| ApiError::conflict("ROLLBACK_TARGET_INVALID", request_id))?;
    let metadata = m3(&state, request_id)?
        .store
        .read_metadata(source.app_id)
        .map_err(|_| ApiError::app_not_found(request_id))?;
    let request = ScheduleRequest {
        expected_draft_revision: metadata.draft_revision,
        expected_active_release_id: value.expected_active_release_id,
        expected_pending_release_id: value.expected_pending_release_id,
        expected_actual_release_id: value.expected_actual_release_id,
        expected_actual_container_id: value.expected_actual_container_id,
        acknowledge_non_rollbackable_data: true,
    };
    schedule_inner(
        state,
        request_id,
        source.app_id,
        headers,
        request,
        DeploymentTrigger::Rollback,
        Some(target),
        Some(source_id),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn schedule_inner(
    state: AppState,
    request_id: RequestId,
    app_id: Uuid,
    headers: HeaderMap,
    payload: ScheduleRequest,
    trigger: DeploymentTrigger,
    rollback_target: Option<Uuid>,
    rollback_of: Option<Uuid>,
) -> Result<Response, ApiError> {
    let m3 = m3(&state, request_id)?.clone();
    let m4 = m4(&state, request_id)?.clone();
    let route = if trigger == DeploymentTrigger::Manual {
        format!("/api/v1/apps/{app_id}/deployments")
    } else {
        format!(
            "/api/v1/deployments/{}/rollback",
            rollback_of.expect("rollback source")
        )
    };
    let raw = idempotency_key(&headers, request_id)?;
    let canonical=serde_json::to_vec(&serde_json::json!({"route":route,"app_id":app_id,"trigger":trigger,"request":payload,"rollback_target":rollback_target})).map_err(|_|ApiError::internal(request_id))?;
    let fingerprint = m3.idempotency.fingerprint(&canonical);
    if let Some(ClaimResult::Replay { status, body, .. }) = m3
        .idempotency
        .completed(&route, raw, &fingerprint)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?
    {
        return replay(status, body);
    }
    let app_guard = m3
        .coordinator
        .try_app(app_id)
        .map_err(|_| ApiError::conflict("APP_BUSY", request_id))?;
    let global_guard = m3
        .coordinator
        .try_compose_owned()
        .map_err(|_| ApiError::conflict("DEPLOYMENT_BUSY", request_id))?;
    let metadata = m3
        .store
        .read_metadata(app_id)
        .map_err(|_| ApiError::app_not_found(request_id))?;
    let active = m3
        .store
        .read_release_link(app_id, "active")
        .map_err(|_| ApiError::internal(request_id))?;
    let pending = m3
        .store
        .read_release_link(app_id, "pending")
        .map_err(|_| ApiError::internal(request_id))?;
    let actual = actual_fact(&state, app_id, active, request_id).await?;
    if metadata.draft_revision != payload.expected_draft_revision
        || active != payload.expected_active_release_id
        || pending != payload.expected_pending_release_id
        || actual.as_ref().map(|v| v.0) != payload.expected_actual_release_id
        || actual.as_ref().map(|v| v.1.as_str()) != payload.expected_actual_container_id.as_deref()
    {
        return finish_error(
            &m3,
            &route,
            raw,
            "DEPLOYMENT_FACTS_CHANGED",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let draft = crate::app_store::config_revision::load_verified(
        &m3.store.app_directory(app_id),
        metadata.draft_revision,
        m3.store
            .integrity_key()
            .map_err(|_| ApiError::internal(request_id))?,
    )
    .map_err(|_| ApiError::conflict("APP_CONFIG_INVALID", request_id))?;
    let requires_data_ack = !draft.metadata.volumes.is_empty() || !draft.metadata.binds.is_empty();
    if requires_data_ack && !payload.acknowledge_non_rollbackable_data {
        return finish_error(
            &m3,
            &route,
            raw,
            "DATA_ACK_REQUIRED",
            StatusCode::UNPROCESSABLE_ENTITY,
            request_id,
        )
        .await;
    }
    let claim = m3
        .idempotency
        .claim_deployment(
            &route,
            raw,
            &fingerprint,
            request_id.0,
            app_id,
            trigger.as_str(),
            metadata.draft_revision,
            active,
            pending,
            actual.as_ref().map(|value| value.0),
            actual.as_ref().map(|value| value.1.as_str()),
            rollback_target,
            rollback_of,
        )
        .await
        .map_err(|e| ApiError::idempotency(e, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay(status, body);
    }
    let deployment_id = match claim {
        ClaimResult::New(v) => v,
        _ => return Err(ApiError::conflict("DEPLOYMENT_BUSY", request_id)),
    };
    let response = ScheduleResponse {
        deployment_id,
        status: DeploymentStatus::Queued,
        idempotency_replayed: false,
        detail_url: format!("/api/v1/deployments/{deployment_id}"),
    };
    let body = serde_json::to_string(&response).map_err(|_| ApiError::internal(request_id))?;
    m4.engine.spawn(
        state.clone(),
        m3.clone(),
        deployment_id,
        app_guard,
        global_guard,
    );
    Ok((
        StatusCode::ACCEPTED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<usize>,
    cursor: Option<Uuid>,
}
#[derive(Serialize)]
pub struct DeploymentPage {
    items: Vec<DeploymentRecord>,
    next_cursor: Option<Uuid>,
}
pub async fn list(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    _auth: Authenticated,
    Query(query): Query<ListQuery>,
) -> Result<Json<DeploymentPage>, ApiError> {
    let (items, next_cursor) = m4(&state, request_id)?
        .ledger
        .list_page(app_id, query.limit.unwrap_or(20), query.cursor)
        .await
        .map_err(|_| ApiError::internal(request_id))?;
    Ok(Json(DeploymentPage { items, next_cursor }))
}
#[derive(Serialize)]
pub struct DeploymentDetail {
    #[serde(flatten)]
    deployment: DeploymentRecord,
    transitions: Vec<DeploymentTransition>,
    available_actions: Vec<&'static str>,
    safe_release_id: Option<Uuid>,
    current_active_release_id: Option<Uuid>,
    current_pending_release_id: Option<Uuid>,
    current_actual_release_id: Option<Uuid>,
    warnings: Vec<&'static str>,
}
pub async fn detail(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<Uuid>,
    _auth: Authenticated,
) -> Result<Json<DeploymentDetail>, ApiError> {
    let ledger = &m4(&state, request_id)?.ledger;
    let deployment = ledger
        .get(id)
        .await
        .map_err(|_| ApiError::internal(request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "DEPLOYMENT_NOT_FOUND",
                "The deployment was not found",
                request_id,
            )
        })?;
    let transitions = ledger
        .transitions(id)
        .await
        .map_err(|_| ApiError::internal(request_id))?;
    let m3 = m3(&state, request_id)?;
    let active = m3
        .store
        .read_release_link(deployment.app_id, "active")
        .map_err(|_| ApiError::internal(request_id))?;
    let pending = m3
        .store
        .read_release_link(deployment.app_id, "pending")
        .map_err(|_| ApiError::internal(request_id))?;
    let actual_result = actual_fact(&state, deployment.app_id, active, request_id).await;
    let actual_unavailable = actual_result.is_err();
    let actual = actual_result.ok().flatten();
    let nonterminal = ledger
        .list(deployment.app_id, 1)
        .await
        .map_err(|_| ApiError::internal(request_id))?
        .into_iter()
        .next()
        .is_some_and(|value| !value.status.is_terminal());
    let safe_release_id = deployment.candidate_release_id.filter(|release_id| {
        state.m3.as_ref().is_some_and(|m3| {
            m3.store
                .load_v2_release(deployment.app_id, *release_id)
                .is_ok()
        })
    });
    let rollback_available = deployment.status.is_terminal()
        && !nonterminal
        && pending.is_none()
        && actual.as_ref().map(|value| value.0) == active
        && safe_release_id.is_some()
        && safe_release_id != active;
    let mut warnings = Vec::new();
    if pending.is_some() {
        warnings.push("DEPLOYMENT_PENDING");
    }
    if actual.as_ref().map(|value| value.0) != active {
        warnings.push("RUNTIME_DRIFT");
    }
    if nonterminal {
        warnings.push("DEPLOYMENT_BUSY");
    }
    if actual_unavailable {
        warnings.push("RUNTIME_FACTS_UNAVAILABLE");
    }
    Ok(Json(DeploymentDetail {
        deployment,
        transitions,
        available_actions: if rollback_available {
            vec!["rollback"]
        } else {
            Vec::new()
        },
        safe_release_id,
        current_active_release_id: active,
        current_pending_release_id: pending,
        current_actual_release_id: actual.map(|value| value.0),
        warnings,
    }))
}

async fn actual_fact(
    state: &AppState,
    app_id: Uuid,
    active: Option<Uuid>,
    request_id: RequestId,
) -> Result<Option<(Uuid, String)>, ApiError> {
    let project = crate::domain::AppMetadata::project_name(app_id);
    let candidates = state
        .observer
        .api()
        .list_compose_app_containers(&project)
        .await
        .map_err(|e| ApiError::docker(request_id, e.public_code()))?;
    if candidates.len() > 1 {
        return Err(ApiError::conflict("APP_CONTAINER_AMBIGUOUS", request_id));
    }
    let Some(container) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let app = crate::docker::AppCatalogEntry {
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
        poll_interval_seconds: 300,
        draft: None,
    };
    let identity = validate_syntactic_identity(&container.labels, &app)
        .ok_or_else(|| ApiError::conflict("APP_CONTAINER_INVALID", request_id))?;
    Ok(Some((identity.release_id, container.id)))
}

fn credential_response(state: &AppState, value: CredentialMetadata) -> CredentialResponse {
    CredentialResponse {
        id: value.id,
        registry: value.registry,
        username: value.username,
        revision: value.revision,
        created_at: value.created_at,
        rotated_at: value.rotated_at,
        referenced_by_apps: state
            .observer
            .catalog
            .snapshot()
            .apps
            .iter()
            .filter(|app| app.draft.as_ref().and_then(|v| v.credential_ref) == Some(value.id))
            .count(),
    }
}
fn credential_in_use(state: &AppState, m3: &M3Services, id: Uuid) -> Result<bool, &'static str> {
    if state
        .observer
        .catalog
        .snapshot()
        .apps
        .iter()
        .any(|app| app.draft.as_ref().and_then(|v| v.credential_ref) == Some(id))
    {
        return Ok(true);
    }
    let report = m3
        .store
        .scan_read_only()
        .map_err(|_| "CREDENTIAL_REFERENCE_SCAN_FAILED")?;
    if !report.issues.is_empty() {
        return Err("CREDENTIAL_REFERENCE_SCAN_FAILED");
    }
    for app in report.valid_apps {
        let releases = m3.store.app_directory(app.app_id).join("releases");
        for release_id in canonical_release_ids(&releases)? {
            match m3.store.load_v2_release(app.app_id, release_id) {
                Ok(release) if release.credential_ref == Some(id) => return Ok(true),
                Ok(_) => {}
                Err(_) => return Err("CREDENTIAL_REFERENCE_SCAN_FAILED"),
            }
        }
    }
    Ok(false)
}

fn canonical_release_ids(path: &std::path::Path) -> Result<Vec<Uuid>, &'static str> {
    let entries = std::fs::read_dir(path).map_err(|_| "CREDENTIAL_REFERENCE_SCAN_FAILED")?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "CREDENTIAL_REFERENCE_SCAN_FAILED")?;
        let name = entry.file_name();
        let text = name.to_str().ok_or("CREDENTIAL_REFERENCE_SCAN_FAILED")?;
        let release_id = text
            .parse::<Uuid>()
            .map_err(|_| "CREDENTIAL_REFERENCE_SCAN_FAILED")?;
        if text != release_id.to_string() {
            return Err("CREDENTIAL_REFERENCE_SCAN_FAILED");
        }
        ids.push(release_id);
    }
    Ok(ids)
}
fn m3(state: &AppState, id: RequestId) -> Result<&Arc<M3Services>, ApiError> {
    state.m3.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "FEATURE_NOT_AVAILABLE",
            "The feature is not available",
            id,
        )
    })
}
fn m4(state: &AppState, id: RequestId) -> Result<&Arc<M4Services>, ApiError> {
    state.m4.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "FEATURE_NOT_AVAILABLE",
            "The feature is not available",
            id,
        )
    })
}

fn idempotency_key(headers: &HeaderMap, id: RequestId) -> Result<&str, ApiError> {
    headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::idempotency(IdempotencyError::KeyRequired, id))?
        .to_str()
        .map_err(|_| ApiError::idempotency(IdempotencyError::KeyInvalid, id))
}
fn replay(status: u16, body: String) -> Result<Response, ApiError> {
    let status =
        StatusCode::from_u16(status).map_err(|_| ApiError::internal(RequestId(Uuid::nil())))?;
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}
async fn finish_json<T: Serialize>(
    m3: &M3Services,
    route: &str,
    key: &str,
    status: StatusCode,
    value: &T,
    id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::to_string(value).map_err(|_| ApiError::internal(id))?;
    if m3
        .idempotency
        .finish(route, key, status.as_u16(), &body, None, id.0)
        .await
        .is_err()
    {
        let _ = m3.idempotency.mark_interrupted(route, key, id.0).await;
        return Err(ApiError::internal(id));
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}
async fn finish_error(
    m3: &M3Services,
    route: &str,
    key: &str,
    code: &'static str,
    status: StatusCode,
    id: RequestId,
) -> Result<Response, ApiError> {
    let body=serde_json::json!({"code":code,"message":"The operation could not be completed","request_id":id.0}).to_string();
    if m3
        .idempotency
        .finish(route, key, status.as_u16(), &body, Some(code), id.0)
        .await
        .is_err()
    {
        let _ = m3.idempotency.mark_interrupted(route, key, id.0).await;
        return Err(ApiError::internal(id));
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn interrupt_credential(
    m3: &M3Services,
    route: &str,
    key: &str,
    id: RequestId,
) -> Result<Response, ApiError> {
    let _ = m3.idempotency.mark_interrupted(route, key, id.0).await;
    Err(ApiError::internal(id))
}

#[cfg(test)]
mod tests {
    use super::canonical_release_ids;

    #[test]
    fn credential_release_inventory_fails_closed_on_read_and_entry_errors() {
        let root = tempfile::tempdir().unwrap();
        assert!(canonical_release_ids(&root.path().join("missing")).is_err());
        std::fs::write(root.path().join("not-a-release"), b"unexpected").unwrap();
        assert!(canonical_release_ids(root.path()).is_err());
    }
}
