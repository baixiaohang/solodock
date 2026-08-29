use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json,
    extract::{
        Extension, Path, State,
        rejection::{BytesRejection, JsonRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;
use zeroize::Zeroize;

use super::{AppState, auth::MutationAuthenticated};
use crate::{
    app_store::{AppStore, StoreError, config_revision},
    compose::{
        ComposeAction, ComposeCapability, ComposeInput, ComposeRunner, ComposeStatus, RunContext,
        generate,
    },
    db::Database,
    docker::{
        AppCatalogEntry,
        models::{ContainerRecord, ContainerStatus},
        ownership::validate_identity,
    },
    domain::{DesiredState, DraftInput, ExistingSecrets, NormalizedDraft, normalize_draft},
    error::{ApiError, RequestId},
    mutation::{
        AppMutationCoordinator, ClaimResult, EffectMarker, IdempotencyError, IdempotencyService,
    },
    security::secret::SecretValue,
};

pub struct M3Services {
    pub store: AppStore,
    pub database: Database,
    pub allowed_bind_roots: Vec<PathBuf>,
    pub runtime_directory: PathBuf,
    pub idempotency: IdempotencyService,
    pub coordinator: AppMutationCoordinator,
    pub compose: Arc<dyn ComposeRunner>,
    pub compose_capability: ComposeCapability,
    pub projection_degraded: Arc<AtomicBool>,
    pub reconcile_notify: Arc<Notify>,
    pub publication_lock: Arc<Mutex<()>>,
}

struct ExactTempDirectory(PathBuf);

impl Drop for ExactTempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Serialize)]
struct MutationResponse {
    app: AppMutationDetail,
    idempotency_replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection_warning: Option<&'static str>,
}

#[derive(Serialize)]
struct AppMutationDetail {
    id: Uuid,
    slug: String,
    display_name: String,
    project_name: String,
    config_revision: Uuid,
    config_sha256: String,
    desired_state: crate::domain::DesiredState,
    deployment_status: &'static str,
    warnings: Vec<&'static str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDraftRequest {
    expected_revision: Uuid,
    draft: DraftInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateRequest {
    #[serde(default)]
    draft: Option<DraftInput>,
}

pub async fn create(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<DraftInput>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let Json(input) = payload.map_err(|error| json_error(error, request_id))?;
    if input.auto_deploy_enabled && !input.auto_deploy_acknowledged {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "AUTO_DEPLOY_ACK_REQUIRED",
            "Automatic deployment requires explicit acknowledgement",
            request_id,
        ));
    }
    let draft = normalize_draft(
        input,
        &ExistingSecrets::default(),
        &services.idempotency.fingerprint(b"config"),
        &services.allowed_bind_roots,
    )
    .map_err(|error| ApiError::domain(error, request_id))?;
    validate_registry_credential(&state, &draft, request_id)?;
    let route = "/api/v1/apps";
    let raw_key =
        idempotency_key(&headers).map_err(|error| ApiError::idempotency(error, request_id))?;
    let request_hmac = draft_fingerprint(services, route, &draft)?;
    let claim = services
        .idempotency
        .claim(route, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        let _ = refresh(&state, services).await;
        return replay_recorded(status, body);
    }
    let operation_id = match claim {
        ClaimResult::New(id) | ClaimResult::Resume(id) => id,
        ClaimResult::Replay { .. } => unreachable!(),
    };
    let _catalog = services.coordinator.catalog_lock().await;
    if validate_registry_credential(&state, &draft, request_id).is_err() {
        return finish_error(
            services,
            route,
            raw_key,
            "REGISTRY_CREDENTIAL_INVALID",
            StatusCode::UNPROCESSABLE_ENTITY,
            request_id,
        )
        .await;
    }
    if let Ok(existing) = services.store.read_metadata(operation_id)
        && existing.last_operation_id == operation_id
        && existing.draft_revision == operation_id
    {
        let projection_warning = refresh(&state, services).await;
        let response = MutationResponse {
            app: detail(&existing, &draft),
            idempotency_replayed: false,
            projection_warning,
        };
        return finish_json(
            services,
            route,
            raw_key,
            StatusCode::CREATED,
            &response,
            request_id,
        )
        .await;
    }
    let report = match services.store.scan_read_only() {
        Ok(report) => report,
        Err(_) => return interrupt_internal(services, route, raw_key, request_id).await,
    };
    if report.valid_apps.iter().any(|app| app.slug == draft.slug) {
        return finish_error(
            services,
            route,
            raw_key,
            "APP_SLUG_CONFLICT",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let _app = match services.coordinator.try_app(operation_id) {
        Ok(guard) => guard,
        Err(_) => {
            return finish_error(
                services,
                route,
                raw_key,
                "APP_BUSY",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    };
    let metadata = match services.store.create_app(
        operation_id,
        operation_id,
        operation_id,
        &draft,
        OffsetDateTime::now_utc(),
    ) {
        Ok(metadata) => metadata,
        Err(StoreError::AppConflict) => {
            let existing = match services.store.read_metadata(operation_id) {
                Ok(existing) => existing,
                Err(_) => {
                    return interrupt_internal(services, route, raw_key, request_id).await;
                }
            };
            if existing.last_operation_id != operation_id {
                return finish_error(
                    services,
                    route,
                    raw_key,
                    "APP_SLUG_CONFLICT",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
            existing
        }
        Err(_) => match services.store.read_metadata(operation_id) {
            Ok(existing)
                if existing.last_operation_id == operation_id
                    && existing.draft_revision == operation_id =>
            {
                if services.store.repair_app_durability(operation_id).is_err() {
                    return interrupt_internal(services, route, raw_key, request_id).await;
                }
                existing
            }
            _ => return interrupt_internal(services, route, raw_key, request_id).await,
        },
    };
    let projection_warning = refresh(&state, services).await;
    let response = MutationResponse {
        app: detail(&metadata, &draft),
        idempotency_replayed: false,
        projection_warning,
    };
    finish_json(
        services,
        route,
        raw_key,
        StatusCode::CREATED,
        &response,
        request_id,
    )
    .await
}

pub async fn update_draft(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<UpdateDraftRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let Json(payload) = payload.map_err(|error| json_error(error, request_id))?;
    let route = format!("/api/v1/apps/{app_id}/draft");
    let raw_key =
        idempotency_key(&headers).map_err(|error| ApiError::idempotency(error, request_id))?;
    let mut canonical = serde_json::to_vec(&serde_json::json!({
        "actor":"admin",
        "method":"PUT",
        "route":route,
        "expected_revision":payload.expected_revision,
        "draft":&payload.draft,
    }))
    .map_err(|_| ApiError::internal(request_id))?;
    let request_hmac = services.idempotency.fingerprint(&canonical);
    canonical.zeroize();
    let claim = services
        .idempotency
        .claim(&route, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        let _ = refresh(&state, services).await;
        return replay_recorded(status, body);
    }
    let operation_id = match claim {
        ClaimResult::New(id) | ClaimResult::Resume(id) => id,
        _ => unreachable!(),
    };
    let _catalog = services.coordinator.catalog_lock().await;
    let _guard = match services.coordinator.try_app(app_id) {
        Ok(guard) => guard,
        Err(_) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_BUSY",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    };
    let current_metadata = match services.store.read_metadata(app_id) {
        Ok(metadata) => metadata,
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_NOT_FOUND",
                StatusCode::NOT_FOUND,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    if !current_metadata.auto_deploy_enabled
        && payload.draft.auto_deploy_enabled
        && !payload.draft.auto_deploy_acknowledged
    {
        return finish_error(
            services,
            &route,
            raw_key,
            "AUTO_DEPLOY_ACK_REQUIRED",
            StatusCode::UNPROCESSABLE_ENTITY,
            request_id,
        )
        .await;
    }
    if current_metadata.last_operation_id == operation_id
        && current_metadata.draft_revision == operation_id
    {
        let loaded = match load_config(services, app_id, operation_id) {
            Ok(loaded) => loaded,
            Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        let draft = match normalize_draft(
            loaded.input(
                current_metadata.slug.clone(),
                current_metadata.display_name.clone(),
                current_metadata.discovery_image_ref.clone(),
                current_metadata.credential_ref,
                current_metadata.auto_deploy_enabled,
                current_metadata.poll_interval_seconds,
            ),
            &loaded.secrets,
            &services.idempotency.fingerprint(b"config"),
            &services.allowed_bind_roots,
        ) {
            Ok(draft) if draft.metadata == loaded.metadata => draft,
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        let projection_warning = refresh(&state, services).await;
        let response = MutationResponse {
            app: detail(&current_metadata, &draft),
            idempotency_replayed: false,
            projection_warning,
        };
        return finish_json(
            services,
            &route,
            raw_key,
            StatusCode::OK,
            &response,
            request_id,
        )
        .await;
    }
    match services.store.read_release_link(app_id, "pending") {
        Ok(Some(_)) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "DEPLOYMENT_PENDING",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Ok(None) => {}
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    }
    if current_metadata.draft_revision != payload.expected_revision {
        return finish_error(
            services,
            &route,
            raw_key,
            "APP_REVISION_STALE",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let loaded = match load_config(services, app_id, payload.expected_revision) {
        Ok(loaded) => loaded,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    let draft = match normalize_draft(
        payload.draft,
        &loaded.secrets,
        &services.idempotency.fingerprint(b"config"),
        &services.allowed_bind_roots,
    ) {
        Ok(draft) => draft,
        Err(error) => {
            return finish_error(
                services,
                &route,
                raw_key,
                error.public_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
    };
    validate_registry_credential(&state, &draft, request_id)?;
    let report = match services.store.scan_read_only() {
        Ok(report) => report,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    if report
        .valid_apps
        .iter()
        .any(|other| other.app_id != app_id && other.slug == draft.slug)
    {
        return finish_error(
            services,
            &route,
            raw_key,
            "APP_SLUG_CONFLICT",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let metadata = match services.store.update_draft(
        app_id,
        payload.expected_revision,
        operation_id,
        operation_id,
        &draft,
        OffsetDateTime::now_utc(),
    ) {
        Ok(metadata) => metadata,
        Err(StoreError::RevisionStale) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_REVISION_STALE",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Err(StoreError::ConfigRevisionConflict) => {
            let existing = match services.store.read_metadata(app_id) {
                Ok(existing) => existing,
                Err(_) => {
                    return interrupt_internal(services, &route, raw_key, request_id).await;
                }
            };
            if existing.last_operation_id != operation_id {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    "APP_REVISION_STALE",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
            existing
        }
        Err(_) => match services.store.read_metadata(app_id) {
            Ok(existing)
                if existing.last_operation_id == operation_id
                    && existing.draft_revision == operation_id =>
            {
                if services.store.repair_app_durability(app_id).is_err() {
                    return interrupt_internal(services, &route, raw_key, request_id).await;
                }
                existing
            }
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        },
    };
    let projection_warning = refresh(&state, services).await;
    let response = MutationResponse {
        app: detail(&metadata, &draft),
        idempotency_replayed: false,
        projection_warning,
    };
    finish_json(
        services,
        &route,
        raw_key,
        StatusCode::OK,
        &response,
        request_id,
    )
    .await
}

#[derive(Serialize)]
struct ValidateResponse {
    plan: crate::compose::ComposePlan,
    compose_yaml: String,
}

pub async fn validate(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<ValidateRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let operation_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    if let Some(code) = compose_unready_code(services.compose_capability.current()) {
        return Err(ApiError::compose(code, request_id));
    }
    let app = state
        .observer
        .catalog
        .get(app_id)
        .ok_or_else(|| ApiError::app_not_found(request_id))?;
    let revision = app
        .draft_revision
        .ok_or_else(|| ApiError::conflict("APP_DEPLOY_REQUIRED", request_id))?;
    let loaded =
        load_config(services, app_id, revision).map_err(|_| ApiError::internal(request_id))?;
    let Json(payload) = payload.map_err(|error| json_error(error, request_id))?;
    let input = payload
        .draft
        .unwrap_or_else(|| current_draft_input(&app, &loaded));
    let draft = normalize_draft(
        input,
        &loaded.secrets,
        &services.idempotency.fingerprint(b"config"),
        &services.allowed_bind_roots,
    )
    .map_err(|error| ApiError::domain(error, request_id))?;
    let bind_identities = validate_runtime_paths(&state, services, &draft.metadata)
        .await
        .map_err(|code| ApiError::conflict(code, request_id))?;
    validate_resources(&state, app_id, &draft.metadata, request_id).await?;
    for identity in &bind_identities {
        crate::domain::revalidate_bind_identity(identity, &services.allowed_bind_roots)
            .map_err(|error| ApiError::domain(error, request_id))?;
    }
    let operation_id = Uuid::new_v4();
    let temp = services
        .runtime_directory
        .join("compose")
        .join(operation_id.to_string());
    crate::security::permissions::ensure_private_directory(&temp)
        .map_err(|_| ApiError::internal(request_id))?;
    let temp_guard = ExactTempDirectory(temp.clone());
    let file = temp.join("compose.yaml");
    let staged = (|| {
        config_revision::publish(&temp, operation_id, &draft)
            .map_err(|_| ApiError::internal(request_id))?;
        let revision_directory = temp.join("config-revisions").join(operation_id.to_string());
        let (runtime_yaml, plan) = generate(
            ComposeInput {
                app_id,
                release_id: operation_id,
                image_ref: &draft.discovery_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
            },
            false,
        )
        .map_err(|_| ApiError::internal(request_id))?;
        crate::app_store::atomic::AtomicWriter::write(&file, runtime_yaml.as_bytes(), 0o600)
            .map_err(|_| ApiError::internal(request_id))?;
        Ok::<_, ApiError>((runtime_yaml, plan))
    })();
    let (yaml, plan) = match staged {
        Ok(value) => value,
        Err(error) => return Err(error),
    };
    // Draft secrets are scoped to this validation only. They must not enter
    // the active/draft global provider until a filesystem revision commits.
    for identity in &bind_identities {
        crate::domain::revalidate_bind_identity(identity, &services.allowed_bind_roots)
            .map_err(|error| ApiError::domain(error, request_id))?;
    }
    let compose_timeout = operation_deadline
        .checked_duration_since(tokio::time::Instant::now())
        .ok_or_else(|| ApiError::compose("COMPOSE_TIMEOUT", request_id))?;
    let run = services
        .compose
        .run(
            ComposeAction::Validate,
            RunContext {
                project_name: app.project_name,
                project_directory: temp.clone(),
                compose_file: file,
                timeout: compose_timeout,
                redaction_patterns: draft.known_secrets(),
            },
        )
        .await;
    run.map_err(|error| ApiError::compose(error.public_code(), request_id))?;
    drop(temp_guard);
    Ok((
        StatusCode::OK,
        Json(ValidateResponse {
            plan,
            compose_yaml: yaml,
        }),
    )
        .into_response())
}

fn services(state: &AppState, request_id: RequestId) -> Result<&M3Services, ApiError> {
    state
        .m3
        .as_deref()
        .ok_or_else(|| ApiError::compose("FEATURE_NOT_AVAILABLE", request_id))
}
fn validate_registry_credential(
    state: &AppState,
    draft: &NormalizedDraft,
    request_id: RequestId,
) -> Result<(), ApiError> {
    let Some(id) = draft.credential_ref else {
        return Ok(());
    };
    let image = crate::registry::ImageReference::parse(&draft.discovery_image_ref)
        .map_err(|_| ApiError::validation(request_id))?;
    let services = state
        .m4
        .as_deref()
        .ok_or_else(|| ApiError::compose("FEATURE_NOT_AVAILABLE", request_id))?;
    let credential = services.credentials.load(id).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CREDENTIAL_NOT_FOUND",
            "The registry credential was not found",
            request_id,
        )
    })?;
    if credential.metadata.registry != image.logical_registry {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "REGISTRY_CREDENTIAL_MISMATCH",
            "The registry credential does not match the image registry",
            request_id,
        ));
    }
    Ok(())
}
fn idempotency_key(headers: &HeaderMap) -> Result<&str, IdempotencyError> {
    headers
        .get("idempotency-key")
        .ok_or(IdempotencyError::KeyRequired)?
        .to_str()
        .map_err(|_| IdempotencyError::KeyInvalid)
}
fn draft_fingerprint(
    services: &M3Services,
    route: &str,
    draft: &NormalizedDraft,
) -> Result<Vec<u8>, ApiError> {
    let canonical = serde_json::to_vec(&serde_json::json!({"actor":"admin","route":route,"slug":draft.slug,"display_name":draft.display_name,"image":draft.discovery_image_ref,"credential_ref":draft.credential_ref,"auto_deploy_enabled":draft.auto_deploy_enabled,"auto_deploy_acknowledged":draft.auto_deploy_acknowledged,"poll":draft.poll_interval_seconds,"config":draft.metadata})).map_err(|_| ApiError::internal(RequestId(Uuid::nil())))?;
    Ok(services.idempotency.fingerprint(&canonical))
}
fn detail(metadata: &crate::domain::AppMetadata, draft: &NormalizedDraft) -> AppMutationDetail {
    AppMutationDetail {
        id: metadata.id,
        slug: metadata.slug.clone(),
        display_name: metadata.display_name.clone(),
        project_name: metadata.project_name.clone(),
        config_revision: metadata.draft_revision,
        config_sha256: metadata.draft_config_sha256.clone(),
        desired_state: metadata.desired_state,
        deployment_status: "DEPLOY_REQUIRED",
        warnings: if draft.binds.iter().any(|bind| !bind.readonly) || !draft.volumes.is_empty() {
            vec!["DATA_NOT_ROLLED_BACK"]
        } else {
            vec![]
        },
    }
}
#[derive(Clone, Copy)]
struct PublicationOutcome {
    warning: Option<&'static str>,
    catalog_published: bool,
}

async fn refresh_outcome(state: &AppState, services: &M3Services) -> PublicationOutcome {
    let _publication = services.publication_lock.lock().await;
    let report = match services.store.scan_read_only() {
        Ok(report) => report,
        Err(_) => {
            services.projection_degraded.store(true, Ordering::Release);
            services.reconcile_notify.notify_one();
            return PublicationOutcome {
                warning: Some("FILESYSTEM_RESCAN_FAILED"),
                catalog_published: false,
            };
        }
    };
    let secrets = match collect_secret_inventory(state, services, &report) {
        Ok(values) => values,
        Err(_) => {
            services.projection_degraded.store(true, Ordering::Release);
            services.reconcile_notify.notify_one();
            return PublicationOutcome {
                warning: Some("SECRET_INVENTORY_FAILED"),
                catalog_published: false,
            };
        }
    };
    state.observer.catalog.replace(&report);
    if let Some(m4) = state.m4.as_ref() {
        m4.poller.notify.notify_one();
    }
    // A degraded report may omit an app whose already-running container and
    // log stream still exist. Never remove known patterns until the complete
    // filesystem inventory has been proven readable again.
    if report.issues.is_empty() {
        state.redactor.replace(secrets);
    } else {
        state.redactor.extend(secrets);
    }
    let sqlite_warning = services
        .database
        .refresh_app_index(&report)
        .await
        .err()
        .map(|_| "SQLITE_PROJECTION_DEGRADED");
    let warning = if !report.issues.is_empty() {
        Some("FILESYSTEM_RECOVERY_DEGRADED")
    } else {
        sqlite_warning
    };
    services
        .projection_degraded
        .store(warning.is_some(), Ordering::Release);
    if warning.is_some() {
        services.reconcile_notify.notify_one();
    }
    PublicationOutcome {
        warning,
        catalog_published: true,
    }
}

fn collect_secret_inventory(
    state: &AppState,
    services: &M3Services,
    report: &crate::app_store::recovery::RecoveryReport,
) -> Result<Vec<Vec<u8>>, StoreError> {
    let mut secrets = Vec::new();
    for app in &report.valid_apps {
        let mut revisions = [
            app.draft_revision,
            app.active_config_revision,
            app.pending_config_revision,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        revisions.sort_unstable();
        revisions.dedup();
        for revision in revisions {
            secrets.extend(load_config(services, app.app_id, revision)?.known_secrets());
        }
    }
    if let Some(m4) = state.m4.as_ref() {
        for metadata in m4.credentials.list()? {
            secrets.push(
                m4.credentials
                    .load(metadata.id)?
                    .secret
                    .expose()
                    .as_bytes()
                    .to_vec(),
            );
        }
    }
    Ok(secrets)
}

pub(crate) async fn refresh(state: &AppState, services: &M3Services) -> Option<&'static str> {
    refresh_outcome(state, services).await.warning
}

struct DeletionPublication {
    warning: Option<&'static str>,
    can_finalize: bool,
}

async fn publish_deletion(
    state: &AppState,
    services: &M3Services,
    app_id: Uuid,
) -> DeletionPublication {
    let outcome = refresh_outcome(state, services).await;
    DeletionPublication {
        warning: outcome.warning,
        can_finalize: outcome.catalog_published && state.observer.catalog.get(app_id).is_none(),
    }
}

fn finalize_deletion_or_reconcile(
    services: &M3Services,
    app_id: Uuid,
    operation_id: Uuid,
    publication: &DeletionPublication,
) {
    let finalized = publication.can_finalize
        && services
            .store
            .finalize_tombstone(app_id, operation_id)
            .is_ok();
    if !finalized {
        // A reconciler may have repaired the projection before the
        // idempotency record reached succeeded. Reassert the work after the
        // response is durable so cleanup cannot be lost in that race.
        services.projection_degraded.store(true, Ordering::Release);
        services.reconcile_notify.notify_one();
    }
}

pub fn start_projection_reconciler(
    state: AppState,
    cancellation: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = state.m3.as_ref().expect("M3 services configured").reconcile_notify.notified() => {},
                () = tokio::time::sleep(Duration::from_secs(2)) => {},
            }
            let Some(services) = state.m3.as_deref() else {
                break;
            };
            if services.projection_degraded.load(Ordering::Acquire) {
                let outcome = refresh_outcome(&state, services).await;
                if outcome.catalog_published
                    && services
                        .idempotency
                        .finalize_succeeded_tombstones(&services.store)
                        .await
                        .is_err()
                {
                    services.projection_degraded.store(true, Ordering::Release);
                    services.reconcile_notify.notify_one();
                }
                if let Some(m4) = state.m4.as_deref()
                    && services
                        .idempotency
                        .finalize_succeeded_credential_tombstones(&m4.credentials)
                        .await
                        .is_err()
                {
                    services.projection_degraded.store(true, Ordering::Release);
                    services.reconcile_notify.notify_one();
                }
            }
        }
    })
}
fn replay(status: u16, body: String) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

fn replay_recorded(status: u16, body: String) -> Result<Response, ApiError> {
    let body = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("idempotency_replayed".into(), serde_json::Value::Bool(true));
            }
            value.to_string()
        }
        Err(_) => body,
    };
    replay(status, body)
}
async fn finish_json<T: Serialize>(
    services: &M3Services,
    route: &str,
    key: &str,
    status: StatusCode,
    value: &T,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = match serde_json::to_string(value) {
        Ok(body) => body,
        Err(_) => {
            let _ = services
                .idempotency
                .mark_interrupted(route, key, request_id.0)
                .await;
            return Err(ApiError::internal(request_id));
        }
    };
    if services
        .idempotency
        .finish(route, key, status.as_u16(), &body, None, request_id.0)
        .await
        .is_err()
    {
        let _ = services
            .idempotency
            .mark_interrupted(route, key, request_id.0)
            .await;
        return Err(ApiError::internal(request_id));
    }
    replay(status.as_u16(), body)
}

pub async fn start(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    body: Result<bytes::Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    require_empty_body(body, request_id)?;
    lifecycle(&state, request_id, app_id, headers, LifecycleAction::Start).await
}
pub async fn stop(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    body: Result<bytes::Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    require_empty_body(body, request_id)?;
    lifecycle(&state, request_id, app_id, headers, LifecycleAction::Stop).await
}
pub async fn restart(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    body: Result<bytes::Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    require_empty_body(body, request_id)?;
    lifecycle(
        &state,
        request_id,
        app_id,
        headers,
        LifecycleAction::Restart,
    )
    .await
}

fn require_empty_body(
    body: Result<bytes::Bytes, BytesRejection>,
    request_id: RequestId,
) -> Result<(), ApiError> {
    let body = body.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "REQUEST_BODY_TOO_LARGE",
            "The request body is too large",
            request_id,
        )
    })?;
    if body.is_empty() {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "REQUEST_BODY_TOO_LARGE",
            "The request body is too large",
            request_id,
        ))
    }
}

fn json_error(error: JsonRejection, request_id: RequestId) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "REQUEST_BODY_TOO_LARGE",
            "The request body is too large",
            request_id,
        )
    } else {
        ApiError::validation(request_id)
    }
}

#[derive(Clone, Copy)]
enum LifecycleAction {
    Start,
    Stop,
    Restart,
}

impl LifecycleAction {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
    fn desired(self) -> Option<DesiredState> {
        match self {
            Self::Start => Some(DesiredState::Running),
            Self::Stop => Some(DesiredState::Stopped),
            Self::Restart => None,
        }
    }
}

#[derive(Serialize)]
struct LifecycleResponse {
    app_id: Uuid,
    action: &'static str,
    desired_state: DesiredState,
    observed_state: &'static str,
    idempotency_replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection_warning: Option<&'static str>,
}

struct VerifiedActive {
    app: AppCatalogEntry,
    release_id: Uuid,
    loaded: config_revision::LoadedRevision,
    compose_file: PathBuf,
    bind_identities: Vec<crate::domain::BindIdentity>,
}

fn load_verified_active(
    services: &M3Services,
    app_id: Uuid,
) -> Result<VerifiedActive, &'static str> {
    let report = services
        .store
        .scan_read_only()
        .map_err(|_| "FILESYSTEM_RESCAN_FAILED")?;
    let recovered = report
        .valid_apps
        .iter()
        .find(|candidate| candidate.app_id == app_id)
        .ok_or("APP_NOT_FOUND")?;
    let app = AppCatalogEntry::from(recovered);
    let release_id = app.active_release_id.ok_or("APP_DEPLOY_REQUIRED")?;
    let revision = app
        .active_config_revision
        .ok_or("ACTIVE_RELEASE_CONFIG_UNKNOWN")?;
    let loaded =
        load_config(services, app_id, revision).map_err(|_| "ACTIVE_RELEASE_CONFIG_UNKNOWN")?;
    verify_active_compose(services, &app, release_id, &loaded)
        .map_err(|_| "ACTIVE_COMPOSE_INVALID")?;
    let bind_identities =
        crate::domain::validate_binds(&loaded.metadata.binds, &services.allowed_bind_roots)
            .map_err(|_| "BIND_INVALID")?;
    let compose_file = services
        .store
        .app_directory(app_id)
        .join("releases")
        .join(release_id.to_string())
        .join("compose.yaml");
    Ok(VerifiedActive {
        app,
        release_id,
        loaded,
        compose_file,
        bind_identities,
    })
}

fn load_verified_pending(
    services: &M3Services,
    app_id: Uuid,
) -> Result<VerifiedActive, &'static str> {
    let report = services
        .store
        .scan_read_only()
        .map_err(|_| "FILESYSTEM_RESCAN_FAILED")?;
    let recovered = report
        .valid_apps
        .iter()
        .find(|candidate| candidate.app_id == app_id)
        .ok_or("APP_NOT_FOUND")?;
    let app = AppCatalogEntry::from(recovered);
    if app.active_release_id.is_some() {
        return load_verified_active(services, app_id);
    }
    let release_id = app.pending_release_id.ok_or("APP_DEPLOY_REQUIRED")?;
    let release = services
        .store
        .load_v2_release(app_id, release_id)
        .map_err(|_| "PENDING_RELEASE_INVALID")?;
    if app.pending_config_revision != Some(release.config_revision) {
        return Err("PENDING_RELEASE_INVALID");
    }
    let loaded = load_config(services, app_id, release.config_revision)
        .map_err(|_| "PENDING_RELEASE_CONFIG_UNKNOWN")?;
    let bind_identities =
        crate::domain::validate_binds(&loaded.metadata.binds, &services.allowed_bind_roots)
            .map_err(|_| "BIND_INVALID")?;
    Ok(VerifiedActive {
        app,
        release_id,
        loaded,
        compose_file: services.store.release_compose_path(app_id, release_id),
        bind_identities,
    })
}

async fn mutation_container(
    state: &AppState,
    app: &AppCatalogEntry,
) -> Result<Option<ContainerRecord>, &'static str> {
    let candidates = state
        .observer
        .api()
        .list_compose_app_containers(&app.project_name)
        .await
        .map_err(|error| error.public_code())?;
    classify_mutation_candidates(app, candidates)
}

async fn mutation_container_policy(
    state: &AppState,
    app: &AppCatalogEntry,
    allow_pending: bool,
) -> Result<Option<ContainerRecord>, &'static str> {
    if !allow_pending {
        return mutation_container(state, app).await;
    }
    let candidates = state
        .observer
        .api()
        .list_compose_app_containers(&app.project_name)
        .await
        .map_err(|error| error.public_code())?;
    if candidates.len() > 1 {
        return Err("APP_CONTAINER_AMBIGUOUS");
    }
    let Some(container) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let identity = crate::docker::ownership::validate_syntactic_identity(&container.labels, app)
        .ok_or("APP_CONTAINER_INVALID")?;
    if Some(identity.release_id) != app.active_release_id
        && Some(identity.release_id) != app.pending_release_id
    {
        return Err("APP_CONTAINER_INVALID");
    }
    Ok(Some(container))
}

fn classify_mutation_candidates(
    app: &AppCatalogEntry,
    candidates: Vec<ContainerRecord>,
) -> Result<Option<ContainerRecord>, &'static str> {
    if candidates.len() > 1 {
        return Err("APP_CONTAINER_AMBIGUOUS");
    }
    let Some(container) = candidates.into_iter().next() else {
        return Ok(None);
    };
    if validate_identity(&container.labels, app).is_none() {
        return Err("APP_CONTAINER_INVALID");
    }
    Ok(Some(container))
}

pub(crate) async fn validate_runtime_paths(
    state: &AppState,
    services: &M3Services,
    metadata: &crate::domain::ConfigMetadata,
) -> Result<Vec<crate::domain::BindIdentity>, &'static str> {
    let probe = state.observer.supervisor.current().await;
    validate_runtime_paths_for_docker_root(
        services,
        metadata,
        probe.docker_root_directory.as_deref(),
    )
}

pub(crate) fn validate_runtime_paths_for_docker_root(
    services: &M3Services,
    metadata: &crate::domain::ConfigMetadata,
    docker_root: Option<&str>,
) -> Result<Vec<crate::domain::BindIdentity>, &'static str> {
    let identities = crate::domain::validate_binds(&metadata.binds, &services.allowed_bind_roots)
        .map_err(|_| "BIND_INVALID")?;
    if let Some(root) = docker_root.map(PathBuf::from)
        && (services
            .allowed_bind_roots
            .iter()
            .any(|allowed| paths_overlap(allowed, &root))
            || identities
                .iter()
                .any(|identity| paths_overlap(&identity.path, &root)))
    {
        return Err("BIND_ROOT_SENSITIVE");
    }
    Ok(identities)
}

fn paths_overlap(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

async fn lifecycle(
    state: &AppState,
    request_id: RequestId,
    app_id: Uuid,
    headers: HeaderMap,
    action: LifecycleAction,
) -> Result<Response, ApiError> {
    let services = services(state, request_id)?;
    let route = format!("/api/v1/apps/{app_id}/actions/{}", action.name());
    let raw_key =
        idempotency_key(&headers).map_err(|error| ApiError::idempotency(error, request_id))?;
    let request_hmac = services
        .idempotency
        .fingerprint(format!("admin\nPOST\n{route}").as_bytes());
    let claim = services
        .idempotency
        .claim(&route, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        let _ = refresh(state, services).await;
        return replay_recorded(status, body);
    }
    let (operation_id, resumed) = match claim {
        ClaimResult::New(id) => (id, false),
        ClaimResult::Resume(id) => (id, true),
        _ => unreachable!(),
    };
    let operation_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let _app_guard = match services.coordinator.try_app(app_id) {
        Ok(guard) => guard,
        Err(_) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_BUSY",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    };
    let verified = match load_verified_active(services, app_id) {
        Ok(value) => value,
        Err("APP_DEPLOY_REQUIRED") if matches!(action, LifecycleAction::Stop) => {
            match load_verified_pending(services, app_id) {
                Ok(value) => value,
                Err(code) => {
                    return finish_error(
                        services,
                        &route,
                        raw_key,
                        code,
                        mutation_status(code),
                        request_id,
                    )
                    .await;
                }
            }
        }
        Err(code) => {
            return finish_error(
                services,
                &route,
                raw_key,
                code,
                mutation_status(code),
                request_id,
            )
            .await;
        }
    };
    let app = verified.app.clone();
    if app.pending_release_id.is_some() && !matches!(action, LifecycleAction::Stop) {
        return finish_error(
            services,
            &route,
            raw_key,
            "DEPLOYMENT_PENDING",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let active_release = verified.release_id;
    let active_metadata = verified.loaded.metadata.clone();
    if let Err(code) = validate_runtime_paths(state, services, &active_metadata).await {
        return finish_error(
            services,
            &route,
            raw_key,
            code,
            mutation_status(code),
            request_id,
        )
        .await;
    }
    if let Err(error) = validate_resources(state, app_id, &active_metadata, request_id).await {
        let (code, status) = error.code_and_status();
        return finish_error(services, &route, raw_key, code, status, request_id).await;
    }
    let allow_pending = matches!(action, LifecycleAction::Stop);
    let current = mutation_container_policy(state, &app, allow_pending).await;
    let expected_container_id = current
        .as_ref()
        .ok()
        .and_then(|value| value.as_ref())
        .map(|container| container.id.clone());
    let expected_started_at = current
        .as_ref()
        .ok()
        .and_then(|value| value.as_ref())
        .and_then(|container| container.started_at.clone());
    let (mut compose_action, observed) = match (action, current) {
        (LifecycleAction::Start, Ok(Some(container)))
            if container.status == ContainerStatus::Running =>
        {
            (None, "running")
        }
        (LifecycleAction::Start, Ok(Some(_))) => (Some(ComposeAction::Start), "running"),
        (LifecycleAction::Start, Ok(None)) => (Some(ComposeAction::Recreate), "running"),
        (LifecycleAction::Stop, Ok(None)) => (None, "missing"),
        (LifecycleAction::Stop, Ok(Some(container)))
            if container.status == ContainerStatus::Exited =>
        {
            (None, "stopped")
        }
        (LifecycleAction::Stop, Ok(Some(_))) => (Some(ComposeAction::Stop), "stopped"),
        (LifecycleAction::Restart, Ok(Some(_))) => (Some(ComposeAction::Restart), "running"),
        (LifecycleAction::Restart, Ok(None)) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_CONTAINER_NOT_FOUND",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        (_, Err(code)) => {
            return finish_error(
                services,
                &route,
                raw_key,
                code,
                mutation_status(code),
                request_id,
            )
            .await;
        }
    };
    let previous_metadata = match services.store.read_metadata(app_id) {
        Ok(metadata) => metadata,
        Err(_) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "INTERNAL_ERROR",
                StatusCode::INTERNAL_SERVER_ERROR,
                request_id,
            )
            .await;
        }
    };
    let effect_marker = match services.idempotency.effect_marker(&route, raw_key).await {
        Ok(marker) => marker,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    if resumed && let Some(marker) = &effect_marker {
        let observed = mutation_container_policy(state, &app, allow_pending).await;
        let observed_ref = observed.as_ref().ok().and_then(|value| value.as_ref());
        if effect_completed(action, marker, observed_ref) {
            compose_action = None;
        } else if marker.phase == "started" && observation_matches_marker(marker, observed_ref) {
            // The durable pre-effect observation still matches exactly, so a
            // process crash happened before the Compose effect became visible.
        } else if partial_recreate_can_continue(action, marker, observed_ref) {
            // A previous `up` created this exact active container but did not
            // reach Running before its result became unknown. Continue by
            // starting the same durable full ID; never issue a second create.
            compose_action = Some(ComposeAction::Start);
        } else {
            return finish_error(
                services,
                &route,
                raw_key,
                "CONTAINER_CHANGED",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    }
    let desired_state = action.desired().unwrap_or(previous_metadata.desired_state);
    let metadata = match services.store.set_desired_state(
        app_id,
        desired_state,
        operation_id,
        OffsetDateTime::now_utc(),
    ) {
        Ok(metadata) => metadata,
        Err(_) => match services.store.read_metadata(app_id) {
            Ok(existing)
                if existing.last_operation_id == operation_id
                    && existing.desired_state == desired_state =>
            {
                if services.store.repair_app_durability(app_id).is_err() {
                    return interrupt_internal(services, &route, raw_key, request_id).await;
                }
                existing
            }
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        },
    };
    // The filesystem intent is already authoritative even if a later Docker
    // preflight/effect fails. Publish it now or wake the reconciler.
    let _ = refresh(state, services).await;
    if let Some(compose_action) = compose_action {
        if let Some(code) = compose_unready_code(services.compose_capability.current()) {
            return finish_error(
                services,
                &route,
                raw_key,
                code,
                StatusCode::SERVICE_UNAVAILABLE,
                request_id,
            )
            .await;
        }
        let _compose_guard = match services.coordinator.try_compose() {
            Ok(guard) => guard,
            Err(_) => {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    "APP_BUSY",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
        };
        if let Err(error) = validate_resources(state, app_id, &active_metadata, request_id).await {
            let (code, status) = error.code_and_status();
            return finish_error(services, &route, raw_key, code, status, request_id).await;
        }
        let identity_still_matches = match mutation_container_policy(state, &app, allow_pending)
            .await
        {
            Ok(Some(container)) => expected_container_id.as_deref() == Some(container.id.as_str()),
            Ok(None) => expected_container_id.is_none(),
            Err(_) => false,
        };
        if !identity_still_matches {
            return finish_error(
                services,
                &route,
                raw_key,
                "CONTAINER_CHANGED",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        if effect_marker.is_none()
            && services
                .idempotency
                .mark_effect_started(
                    &route,
                    raw_key,
                    expected_container_id.as_deref(),
                    expected_started_at.as_deref(),
                )
                .await
                .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        // Re-read the filesystem fact and every Compose project candidate
        // after the durable marker. Bind identity is deliberately the final
        // operation before spawning the CLI.
        let final_verified =
            if app.active_release_id.is_none() && matches!(action, LifecycleAction::Stop) {
                load_verified_pending(services, app_id)
            } else {
                load_verified_active(services, app_id)
            };
        let final_active = match final_verified {
            Ok(value) if value.release_id == active_release => value,
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        let final_container =
            match mutation_container_policy(state, &final_active.app, allow_pending).await {
                Ok(value) => value,
                Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
            };
        if final_container.as_ref().map(|value| value.id.as_str())
            != expected_container_id.as_deref()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        for identity in &final_active.bind_identities {
            if crate::domain::revalidate_bind_identity(identity, &services.allowed_bind_roots)
                .is_err()
            {
                return interrupt_internal(services, &route, raw_key, request_id).await;
            }
        }
        let compose_timeout =
            match operation_deadline.checked_duration_since(tokio::time::Instant::now()) {
                Some(timeout) => timeout,
                None => {
                    return finish_error(
                        services,
                        &route,
                        raw_key,
                        "COMPOSE_TIMEOUT",
                        StatusCode::SERVICE_UNAVAILABLE,
                        request_id,
                    )
                    .await;
                }
            };
        if let Err(error) = services
            .compose
            .run(
                compose_action,
                RunContext {
                    project_name: final_active.app.project_name.clone(),
                    project_directory: services.store.app_directory(app_id),
                    compose_file: final_active.compose_file,
                    timeout: compose_timeout,
                    redaction_patterns: Vec::new(),
                },
            )
            .await
        {
            let _ = error;
            if expected_container_id.is_none()
                && let Ok(Some(container)) =
                    mutation_container_policy(state, &final_active.app, allow_pending).await
            {
                let _ = services
                    .idempotency
                    .mark_effect_observed(&route, raw_key, &container.id)
                    .await;
            }
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        let final_container = mutation_container_policy(state, &app, allow_pending).await;
        let final_state_matches = match (compose_action, action, final_container.as_ref()) {
            (ComposeAction::Recreate, LifecycleAction::Start, Ok(Some(container))) => {
                expected_container_id.is_none() && container.status == ContainerStatus::Running
            }
            (_, LifecycleAction::Start, Ok(Some(container))) => {
                expected_container_id.as_deref() == Some(container.id.as_str())
                    && container.status == ContainerStatus::Running
            }
            (_, LifecycleAction::Restart, Ok(Some(container))) => {
                expected_container_id.as_deref() == Some(container.id.as_str())
                    && container.status == ContainerStatus::Running
                    && container.started_at != expected_started_at
            }
            (_, LifecycleAction::Stop, Ok(Some(container))) => {
                expected_container_id.as_deref() == Some(container.id.as_str())
                    && container.status == ContainerStatus::Exited
            }
            _ => false,
        };
        if !final_state_matches {
            if expected_container_id.is_none()
                && let Ok(Some(container)) = &final_container
            {
                let _ = services
                    .idempotency
                    .mark_effect_observed(&route, raw_key, &container.id)
                    .await;
            }
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        if expected_container_id.is_none()
            && let Ok(Some(container)) = &final_container
            && services
                .idempotency
                .mark_effect_observed(&route, raw_key, &container.id)
                .await
                .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        if services
            .idempotency
            .mark_effect_completed(&route, raw_key)
            .await
            .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
    }
    let projection_warning = refresh(state, services).await;
    let response = LifecycleResponse {
        app_id,
        action: action.name(),
        desired_state: metadata.desired_state,
        observed_state: observed,
        idempotency_replayed: false,
        projection_warning,
    };
    finish_json(
        services,
        &route,
        raw_key,
        StatusCode::OK,
        &response,
        request_id,
    )
    .await
}

fn observation_matches_marker(
    marker: &EffectMarker,
    observed: Option<&crate::docker::models::ContainerRecord>,
) -> bool {
    match (marker.pre_container_id.as_deref(), observed) {
        (None, None) => true,
        (Some(expected), Some(container)) => {
            container.id == expected && container.started_at == marker.pre_started_at
        }
        _ => false,
    }
}

fn effect_completed(
    action: LifecycleAction,
    marker: &EffectMarker,
    observed: Option<&crate::docker::models::ContainerRecord>,
) -> bool {
    let final_state = match action {
        LifecycleAction::Start | LifecycleAction::Restart => Some(ContainerStatus::Running),
        LifecycleAction::Stop => Some(ContainerStatus::Exited),
    };
    match (marker.pre_container_id.as_deref(), observed) {
        (None, Some(container)) if matches!(action, LifecycleAction::Start) => {
            container.status == ContainerStatus::Running
                && marker
                    .post_container_id
                    .as_deref()
                    .is_none_or(|expected| expected == container.id)
        }
        (Some(expected), Some(container)) if container.id == expected => {
            container.status == final_state.unwrap()
                && (!matches!(action, LifecycleAction::Restart)
                    || container.started_at != marker.pre_started_at)
        }
        _ => false,
    }
}

fn partial_recreate_can_continue(
    action: LifecycleAction,
    marker: &EffectMarker,
    observed: Option<&crate::docker::models::ContainerRecord>,
) -> bool {
    if !matches!(action, LifecycleAction::Start)
        || marker.phase != "started"
        || marker.pre_container_id.is_some()
    {
        return false;
    }
    let Some(observed) = observed else {
        return false;
    };
    marker
        .post_container_id
        .as_deref()
        .is_none_or(|expected| expected == observed.id)
}

fn mutation_status(code: &str) -> StatusCode {
    match code {
        "APP_NOT_FOUND" => StatusCode::NOT_FOUND,
        "BIND_INVALID" | "BIND_ROOT_SENSITIVE" => StatusCode::UNPROCESSABLE_ENTITY,
        "DOCKER_UNAVAILABLE"
        | "DOCKER_PERMISSION_DENIED"
        | "DOCKER_API_INCOMPATIBLE"
        | "DOCKER_OBSERVATION_FAILED"
        | "DOCKER_TIMEOUT"
        | "COMPOSE_UNAVAILABLE"
        | "COMPOSE_PERMISSION_DENIED"
        | "COMPOSE_INCOMPATIBLE"
        | "COMPOSE_TIMEOUT" => StatusCode::SERVICE_UNAVAILABLE,
        "FILESYSTEM_RESCAN_FAILED" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::CONFLICT,
    }
}

async fn finish_error(
    services: &M3Services,
    route: &str,
    key: &str,
    code: &'static str,
    status: StatusCode,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let body = serde_json::json!({"code":code,"message":"The operation could not be completed","request_id":request_id.0}).to_string();
    if services
        .idempotency
        .finish(route, key, status.as_u16(), &body, Some(code), request_id.0)
        .await
        .is_err()
    {
        let _ = services
            .idempotency
            .mark_interrupted(route, key, request_id.0)
            .await;
        return Err(ApiError::internal(request_id));
    }
    replay(status.as_u16(), body)
}

async fn interrupt_internal(
    services: &M3Services,
    route: &str,
    key: &str,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let _ = services
        .idempotency
        .mark_interrupted(route, key, request_id.0)
        .await;
    Err(ApiError::internal(request_id))
}

async fn cancel_and_join_app_streams(
    state: &AppState,
    app_id: Uuid,
) -> Option<crate::api::streams::AppDeletionBarrier> {
    let barrier = state.stream_gate.begin_app_deletion(app_id);
    if !barrier.wait(Duration::from_secs(5)).await {
        return None;
    }
    if !state
        .stats
        .cancel_app_and_wait(app_id, Duration::from_secs(5))
        .await
    {
        return None;
    }
    Some(barrier)
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DeletionPreviewRequest {
    remove_container: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RetainedResources {
    containers: Vec<String>,
    owned_volumes: Vec<RetainedNamedResource>,
    external_volumes: Vec<RetainedNamedResource>,
    binds: Vec<RetainedBind>,
    networks: Vec<RetainedNamedResource>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RetainedNamedResource {
    name: String,
    configured_in: String,
    exists: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RetainedBind {
    source: String,
    readonly: bool,
    configured_in: String,
    exists: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RetainedManagedFile {
    logical_name: String,
    configured_in: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeletionFacts {
    app_id: Uuid,
    slug: String,
    expected_revision: Uuid,
    project_name: String,
    active_release_id: Option<Uuid>,
    active_config_revision: Option<Uuid>,
    pending_release_id: Option<Uuid>,
    pending_config_revision: Option<Uuid>,
    remove_container: bool,
    container_ids: Vec<String>,
    managed_files: Vec<RetainedManagedFile>,
    retained: RetainedResources,
    orphan_warning: bool,
}

struct DeletionSnapshot {
    app: AppCatalogEntry,
    facts: DeletionFacts,
    container: Option<ContainerRecord>,
}

#[derive(Serialize)]
struct DeletionPreviewResponse {
    #[serde(flatten)]
    facts: DeletionFacts,
    confirmation_token: String,
    expires_at: String,
}

pub async fn deletion_preview(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    MutationAuthenticated(authenticated): MutationAuthenticated,
    payload: Result<Json<DeletionPreviewRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = services(&state, request_id)?;
    let Json(payload) = payload.map_err(|error| json_error(error, request_id))?;
    let _catalog = services.coordinator.catalog_lock().await;
    let _app = services
        .coordinator
        .try_app(app_id)
        .map_err(|_| ApiError::conflict("APP_BUSY", request_id))?;
    let snapshot = canonical_deletion_snapshot(&state, services, app_id, payload.remove_container)
        .await
        .map_err(|code| deletion_error(code, request_id))?;
    let facts_json =
        serde_json::to_string(&snapshot.facts).map_err(|_| ApiError::internal(request_id))?;
    let preview_hash = Sha256::digest(facts_json.as_bytes()).to_vec();
    let token = SecretValue::random().map_err(|_| ApiError::internal(request_id))?;
    let token_hash = token.sha256();
    let now = OffsetDateTime::now_utc();
    let expires = now + time::Duration::minutes(5);
    sqlx::query("INSERT INTO deletion_previews (token_hash,session_id,app_id,slug,revision_id,preview_hash,preview_json,remove_container,container_ids_json,expires_at,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)")
        .bind(token_hash.as_slice())
        .bind(&authenticated.session.id)
        .bind(app_id.to_string())
        .bind(&snapshot.facts.slug)
        .bind(snapshot.facts.expected_revision.to_string())
        .bind(preview_hash)
        .bind(&facts_json)
        .bind(payload.remove_container)
        .bind(serde_json::to_string(&snapshot.facts.container_ids).expect("IDs serialize"))
        .bind(crate::db::format_time(expires).map_err(|_| ApiError::internal(request_id))?)
        .bind(crate::db::format_time(now).map_err(|_| ApiError::internal(request_id))?)
        .execute(services.database.pool())
        .await
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::OK,
        Json(DeletionPreviewResponse {
            facts: snapshot.facts,
            confirmation_token: token.expose().to_owned(),
            expires_at: crate::db::format_time(expires).expect("valid time"),
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    confirmation_token: SecretValue,
    slug: String,
    expected_revision: Uuid,
    #[serde(default)]
    remove_container: bool,
}

#[derive(Serialize)]
struct DeleteResponse {
    app_id: Uuid,
    unregistered: bool,
    container_removed: bool,
    retained: RetainedResources,
    orphan_warning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection_warning: Option<&'static str>,
    idempotency_replayed: bool,
}

pub async fn delete_app(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    MutationAuthenticated(authenticated): MutationAuthenticated,
    payload: Result<Json<DeleteRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    use sqlx::Row;

    let services = services(&state, request_id)?;
    let Json(payload) = payload.map_err(|error| json_error(error, request_id))?;
    let route = format!("/api/v1/apps/{app_id}");
    let raw_key =
        idempotency_key(&headers).map_err(|error| ApiError::idempotency(error, request_id))?;
    let canonical = serde_json::json!({"actor":"admin","method":"DELETE","route":route,"slug":payload.slug,"revision":payload.expected_revision,"remove_container":payload.remove_container,"token_hmac":services.idempotency.fingerprint(payload.confirmation_token.expose().as_bytes())});
    let request_hmac = services
        .idempotency
        .fingerprint(&serde_json::to_vec(&canonical).map_err(|_| ApiError::internal(request_id))?);
    let claim = services
        .idempotency
        .claim(&route, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay {
        operation_id,
        status,
        body,
    } = claim
    {
        let tombstone = services.store.tombstone_path(app_id, operation_id);
        if std::fs::symlink_metadata(&tombstone).is_ok() {
            let publication = publish_deletion(&state, services, app_id).await;
            finalize_deletion_or_reconcile(services, app_id, operation_id, &publication);
        } else {
            let _ = refresh(&state, services).await;
        }
        return replay_recorded(status, body);
    }
    let (operation_id, resumed) = match claim {
        ClaimResult::New(id) => (id, false),
        ClaimResult::Resume(id) => (id, true),
        ClaimResult::Replay { .. } => unreachable!(),
    };
    let operation_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let _catalog = services.coordinator.catalog_lock().await;
    let _guard = match services.coordinator.try_app(app_id) {
        Ok(guard) => guard,
        Err(_) => {
            return finish_error(
                services,
                &route,
                raw_key,
                "APP_BUSY",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    };
    let token_hash = payload.confirmation_token.sha256().to_vec();
    let row = match sqlx::query("SELECT session_id,slug,revision_id,preview_hash,preview_json,remove_container,container_ids_json,expires_at,consumed_at FROM deletion_previews WHERE token_hash=? AND app_id=?")
        .bind(&token_hash)
        .bind(app_id.to_string())
        .fetch_optional(services.database.pool())
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return finish_error(services, &route, raw_key, "DELETION_PREVIEW_INVALID", StatusCode::CONFLICT, request_id).await;
        }
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    let preview_session: String = row.get(0);
    let preview_slug: String = row.get(1);
    let preview_revision: String = row.get(2);
    let preview_hash: Vec<u8> = row.get(3);
    let preview_json: String = row.get(4);
    let preview_facts: DeletionFacts = match serde_json::from_str(&preview_json) {
        Ok(facts) => facts,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    if Sha256::digest(preview_json.as_bytes()).as_slice() != preview_hash.as_slice() {
        return interrupt_internal(services, &route, raw_key, request_id).await;
    }
    let preview_remove: bool = row.get(5);
    let expected_ids: Vec<String> = match serde_json::from_str(row.get(6)) {
        Ok(ids) => ids,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    let expires = match crate::db::parse_time(row.get(7)) {
        Ok(expires) => expires,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    let consumed_at: Option<String> = row.get(8);
    if preview_session != authenticated.session.id
        || preview_slug != payload.slug
        || preview_revision != payload.expected_revision.to_string()
        || preview_remove != payload.remove_container
        || preview_facts.app_id != app_id
        || preview_facts.slug != preview_slug
        || preview_facts.expected_revision != payload.expected_revision
        || preview_facts.remove_container != preview_remove
        || preview_facts.container_ids != expected_ids
    {
        return finish_error(
            services,
            &route,
            raw_key,
            "DELETION_PREVIEW_INVALID",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let app_directory_exists = std::fs::symlink_metadata(services.store.app_directory(app_id))
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false);
    if resumed && !app_directory_exists {
        if consumed_at.is_none() {
            return finish_error(
                services,
                &route,
                raw_key,
                "DELETION_PREVIEW_INVALID",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        let metadata = match services.store.read_tombstone_metadata(app_id, operation_id) {
            Ok(metadata) => metadata,
            Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        if metadata.slug != payload.slug || metadata.draft_revision != payload.expected_revision {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        let Some(stream_barrier) = cancel_and_join_app_streams(&state, app_id).await else {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        };
        stream_barrier.commit();
        let publication = publish_deletion(&state, services, app_id).await;
        let response = DeleteResponse {
            app_id,
            unregistered: true,
            container_removed: payload.remove_container && !expected_ids.is_empty(),
            retained: preview_facts.retained.clone(),
            orphan_warning: !payload.remove_container,
            projection_warning: publication.warning,
            idempotency_replayed: false,
        };
        let response = finish_json(
            services,
            &route,
            raw_key,
            StatusCode::OK,
            &response,
            request_id,
        )
        .await?;
        finalize_deletion_or_reconcile(services, app_id, operation_id, &publication);
        return Ok(response);
    }
    if !app_directory_exists {
        return finish_error(
            services,
            &route,
            raw_key,
            "APP_NOT_FOUND",
            StatusCode::NOT_FOUND,
            request_id,
        )
        .await;
    }
    if consumed_at.is_some() && !resumed {
        return finish_error(
            services,
            &route,
            raw_key,
            "DELETION_PREVIEW_INVALID",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    if consumed_at.is_none() && expires <= OffsetDateTime::now_utc() {
        return finish_error(
            services,
            &route,
            raw_key,
            "DELETION_PREVIEW_EXPIRED",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let snapshot =
        match canonical_deletion_snapshot(&state, services, app_id, payload.remove_container).await
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    "DELETION_PREVIEW_STALE",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
        };
    let current_ids = snapshot.facts.container_ids.clone();
    let current_container = snapshot.container.clone();
    let removal_already_applied = resumed
        && consumed_at.is_some()
        && payload.remove_container
        && current_ids.is_empty()
        && !expected_ids.is_empty();
    if !deletion_facts_match(
        &snapshot.facts,
        &preview_hash,
        removal_already_applied.then_some(expected_ids.as_slice()),
    ) {
        return finish_error(
            services,
            &route,
            raw_key,
            "DELETION_PREVIEW_STALE",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    if consumed_at.is_none() {
        let consumed = match crate::db::format_time(OffsetDateTime::now_utc()) {
            Ok(consumed) => consumed,
            Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        let updated = match sqlx::query(
            "UPDATE deletion_previews SET consumed_at=? WHERE token_hash=? AND consumed_at IS NULL",
        )
        .bind(consumed)
        .bind(&token_hash)
        .execute(services.database.pool())
        .await
        {
            Ok(result) => result.rows_affected(),
            Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        if updated != 1 {
            return finish_error(
                services,
                &route,
                raw_key,
                "DELETION_PREVIEW_INVALID",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
    }
    let retained = preview_facts.retained.clone();
    if payload.remove_container && !current_ids.is_empty() && !removal_already_applied {
        let active = match load_verified_active(services, app_id) {
            Ok(value) => value,
            Err(code) => {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    code,
                    mutation_status(code),
                    request_id,
                )
                .await;
            }
        };
        let _compose = match services.coordinator.try_compose() {
            Ok(guard) => guard,
            Err(_) => {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    "APP_BUSY",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
        };
        if let Some(code) = compose_unready_code(services.compose_capability.current()) {
            return finish_error(
                services,
                &route,
                raw_key,
                code,
                StatusCode::SERVICE_UNAVAILABLE,
                request_id,
            )
            .await;
        }
        let unchanged = mutation_container(&state, &active.app)
            .await
            .ok()
            .flatten()
            .is_some_and(|container| current_ids.as_slice() == [container.id]);
        if !unchanged {
            return finish_error(
                services,
                &route,
                raw_key,
                "DELETION_PREVIEW_STALE",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        let delete_effect_marker =
            load_delete_effect_marker(&services.idempotency, &route, raw_key, request_id).await?;
        if delete_effect_marker.is_none()
            && services
                .idempotency
                .mark_effect_started(
                    &route,
                    raw_key,
                    current_container.as_ref().map(|value| value.id.as_str()),
                    current_container
                        .as_ref()
                        .and_then(|value| value.started_at.as_deref()),
                )
                .await
                .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        let final_active = match load_verified_active(services, app_id) {
            Ok(value) if value.release_id == active.release_id => value,
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        if validate_runtime_paths(&state, services, &final_active.loaded.metadata)
            .await
            .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        let final_container = match mutation_container(&state, &final_active.app).await {
            Ok(Some(container)) if current_ids.as_slice() == [container.id.as_str()] => container,
            _ => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
        if current_container
            .as_ref()
            .map(|container| container.id.as_str())
            != Some(final_container.id.as_str())
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        for identity in &final_active.bind_identities {
            if crate::domain::revalidate_bind_identity(identity, &services.allowed_bind_roots)
                .is_err()
            {
                return finish_error(
                    services,
                    &route,
                    raw_key,
                    "BIND_CHANGED",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
        }
        let compose_timeout =
            match operation_deadline.checked_duration_since(tokio::time::Instant::now()) {
                Some(timeout) => timeout,
                None => {
                    return finish_error(
                        services,
                        &route,
                        raw_key,
                        "COMPOSE_TIMEOUT",
                        StatusCode::SERVICE_UNAVAILABLE,
                        request_id,
                    )
                    .await;
                }
            };
        if let Err(error) = services
            .compose
            .run(
                ComposeAction::Remove,
                RunContext {
                    project_name: final_active.app.project_name,
                    project_directory: services.store.app_directory(app_id),
                    compose_file: final_active.compose_file,
                    timeout: compose_timeout,
                    redaction_patterns: Vec::new(),
                },
            )
            .await
        {
            let _ = error;
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        if !matches!(mutation_container(&state, &active.app).await, Ok(None)) {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
        if services
            .idempotency
            .mark_effect_completed(&route, raw_key)
            .await
            .is_err()
        {
            return interrupt_internal(services, &route, raw_key, request_id).await;
        }
    }
    let Some(stream_barrier) = cancel_and_join_app_streams(&state, app_id).await else {
        return interrupt_internal(services, &route, raw_key, request_id).await;
    };
    // The token binds an exact full-ID set. Recheck immediately before the
    // filesystem tombstone because a replacement could appear while stream
    // producers were joining.
    let final_snapshot =
        match canonical_deletion_snapshot(&state, services, app_id, payload.remove_container).await
        {
            Ok(snapshot) => snapshot,
            Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
        };
    let final_ids = final_snapshot.facts.container_ids.clone();
    let expected_final = if payload.remove_container {
        Vec::new()
    } else {
        expected_ids.clone()
    };
    if final_ids != expected_final
        || !deletion_facts_match(
            &final_snapshot.facts,
            &preview_hash,
            (payload.remove_container && !expected_ids.is_empty())
                .then_some(expected_ids.as_slice()),
        )
    {
        return interrupt_internal(services, &route, raw_key, request_id).await;
    }
    // Resource inventory is asynchronous. Enumerate the complete Compose
    // project one final time after it, then perform no more Docker I/O before
    // the synchronous filesystem tombstone.
    let final_observed = match mutation_container(&state, &final_snapshot.app).await {
        Ok(container) => container,
        Err(_) => return interrupt_internal(services, &route, raw_key, request_id).await,
    };
    let final_observed_ids = final_observed
        .map(|container| vec![container.id])
        .unwrap_or_default();
    if final_observed_ids != expected_final {
        return interrupt_internal(services, &route, raw_key, request_id).await;
    }
    if services.store.tombstone(app_id, operation_id).is_err() {
        // rename may already be visible even when a parent-directory fsync
        // fails. Keep the cancellation generation sealed and let the same
        // idempotent operation publish/finalize that exact tombstone.
        if services
            .store
            .read_tombstone_metadata(app_id, operation_id)
            .is_ok()
        {
            stream_barrier.commit();
        }
        return interrupt_internal(services, &route, raw_key, request_id).await;
    }
    stream_barrier.commit();
    let publication = publish_deletion(&state, services, app_id).await;
    let response = DeleteResponse {
        app_id,
        unregistered: true,
        container_removed: payload.remove_container && !expected_ids.is_empty(),
        retained,
        orphan_warning: !payload.remove_container,
        projection_warning: publication.warning,
        idempotency_replayed: false,
    };
    let response = finish_json(
        services,
        &route,
        raw_key,
        StatusCode::OK,
        &response,
        request_id,
    )
    .await?;
    finalize_deletion_or_reconcile(services, app_id, operation_id, &publication);
    Ok(response)
}

async fn load_delete_effect_marker(
    idempotency: &IdempotencyService,
    route: &str,
    key: &str,
    request_id: RequestId,
) -> Result<Option<EffectMarker>, ApiError> {
    match idempotency.effect_marker(route, key).await {
        Ok(marker) => Ok(marker),
        Err(_) => {
            let _ = idempotency.mark_interrupted(route, key, request_id.0).await;
            Err(ApiError::internal(request_id))
        }
    }
}

fn verify_active_compose(
    services: &M3Services,
    app: &crate::docker::AppCatalogEntry,
    release_id: Uuid,
    loaded: &config_revision::LoadedRevision,
) -> Result<(), ()> {
    let image = app.active_image_ref.as_deref().ok_or(())?;
    crate::domain::validate_runnable_image(image).map_err(|_| ())?;
    let revision = app.active_config_revision.ok_or(())?;
    if app.active_config_sha256.as_deref() != Some(loaded.metadata.config_sha256.as_str()) {
        return Err(());
    }
    let draft = normalize_draft(
        current_draft_input(app, loaded),
        &loaded.secrets,
        &services.idempotency.fingerprint(b"config"),
        &services.allowed_bind_roots,
    )
    .map_err(|_| ())?;
    if draft.metadata != loaded.metadata {
        return Err(());
    }
    let revision_directory = services
        .store
        .app_directory(app.id)
        .join("config-revisions")
        .join(revision.to_string());
    let (canonical, _) = generate(
        ComposeInput {
            app_id: app.id,
            release_id,
            image_ref: image,
            revision_directory: &revision_directory,
            draft: &draft,
        },
        true,
    )
    .map_err(|_| ())?;
    let compose_file = services
        .store
        .app_directory(app.id)
        .join("releases")
        .join(release_id.to_string())
        .join("compose.yaml");
    crate::security::permissions::check_private(&compose_file, false).map_err(|_| ())?;
    let actual = std::fs::read(compose_file).map_err(|_| ())?;
    (actual == canonical.as_bytes()).then_some(()).ok_or(())
}

fn compose_unready_code(status: ComposeStatus) -> Option<&'static str> {
    match status {
        ComposeStatus::Ready => None,
        ComposeStatus::Incompatible => Some("COMPOSE_INCOMPATIBLE"),
        ComposeStatus::PermissionDenied => Some("COMPOSE_PERMISSION_DENIED"),
        ComposeStatus::Starting | ComposeStatus::Unavailable => Some("COMPOSE_UNAVAILABLE"),
    }
}

async fn canonical_deletion_snapshot(
    state: &AppState,
    services: &M3Services,
    app_id: Uuid,
    remove_container: bool,
) -> Result<DeletionSnapshot, &'static str> {
    let report = services
        .store
        .scan_read_only()
        .map_err(|_| "FILESYSTEM_RESCAN_FAILED")?;
    let recovered = report
        .valid_apps
        .iter()
        .find(|candidate| candidate.app_id == app_id)
        .ok_or("APP_NOT_FOUND")?;
    let app = AppCatalogEntry::from(recovered);
    let draft_revision = app.draft_revision.ok_or("APP_DEPLOY_REQUIRED")?;
    let draft =
        load_config(services, app_id, draft_revision).map_err(|_| "CONFIG_REVISION_INVALID")?;
    validate_runtime_paths(state, services, &draft.metadata).await?;

    let active = match (app.active_release_id, app.active_config_revision) {
        (Some(release_id), Some(revision)) => {
            let loaded = load_config(services, app_id, revision)
                .map_err(|_| "ACTIVE_RELEASE_CONFIG_UNKNOWN")?;
            verify_active_compose(services, &app, release_id, &loaded)
                .map_err(|_| "ACTIVE_COMPOSE_INVALID")?;
            validate_runtime_paths(state, services, &loaded.metadata).await?;
            Some(loaded)
        }
        (None, None) => None,
        _ => return Err("ACTIVE_RELEASE_CONFIG_UNKNOWN"),
    };
    let pending = match (app.pending_release_id, app.pending_config_revision) {
        (Some(release_id), Some(revision)) => {
            let release = services
                .store
                .load_v2_release(app_id, release_id)
                .map_err(|_| "PENDING_RELEASE_INVALID")?;
            if release.config_revision != revision {
                return Err("PENDING_RELEASE_INVALID");
            }
            let loaded = load_config(services, app_id, revision)
                .map_err(|_| "PENDING_RELEASE_CONFIG_UNKNOWN")?;
            validate_runtime_paths(state, services, &loaded.metadata).await?;
            Some(loaded)
        }
        (None, None) => None,
        _ => return Err("PENDING_RELEASE_INVALID"),
    };
    let container = mutation_container_policy(state, &app, true).await?;
    let container_ids = container
        .as_ref()
        .map(|container| vec![container.id.clone()])
        .unwrap_or_default();
    let retained = retained_resources(
        state,
        app_id,
        active.as_ref().map(|loaded| &loaded.metadata),
        pending.as_ref().map(|loaded| &loaded.metadata),
        &draft.metadata,
        if remove_container {
            &[]
        } else {
            &container_ids
        },
    )
    .await?;
    let managed_files = managed_file_inventory(
        active.as_ref().map(|loaded| &loaded.metadata),
        pending.as_ref().map(|loaded| &loaded.metadata),
        &draft.metadata,
    );
    Ok(DeletionSnapshot {
        facts: DeletionFacts {
            app_id,
            slug: app.slug.clone(),
            expected_revision: draft_revision,
            project_name: app.project_name.clone(),
            active_release_id: app.active_release_id,
            active_config_revision: app.active_config_revision,
            pending_release_id: app.pending_release_id,
            pending_config_revision: app.pending_config_revision,
            remove_container,
            container_ids,
            managed_files,
            retained,
            orphan_warning: !remove_container,
        },
        app,
        container,
    })
}

fn deletion_error(code: &'static str, request_id: RequestId) -> ApiError {
    match code {
        "APP_NOT_FOUND" => ApiError::app_not_found(request_id),
        "DOCKER_UNAVAILABLE"
        | "DOCKER_PERMISSION_DENIED"
        | "DOCKER_API_INCOMPATIBLE"
        | "DOCKER_OBSERVATION_FAILED"
        | "DOCKER_TIMEOUT" => ApiError::docker(request_id, code),
        _ => ApiError::conflict(code, request_id),
    }
}

fn deletion_facts_match(
    current: &DeletionFacts,
    expected_hash: &[u8],
    own_removed_container_ids: Option<&[String]>,
) -> bool {
    let mut comparable = current.clone();
    if let Some(expected) = own_removed_container_ids {
        comparable.container_ids = expected.to_vec();
    }
    serde_json::to_vec(&comparable)
        .map(|bytes| Sha256::digest(bytes).as_slice() == expected_hash)
        .unwrap_or(false)
}

fn configured_scope(active: bool, pending: bool, draft: bool) -> String {
    match (active, pending, draft) {
        (true, true, true) => "active_pending_and_draft",
        (true, true, false) => "active_and_pending",
        (true, false, true) => "active_and_draft",
        (false, true, true) => "pending_and_draft",
        (true, false, false) => "active",
        (false, true, false) => "pending",
        (false, false, true) => "draft",
        (false, false, false) => unreachable!("inventory entries have a configuration source"),
    }
    .into()
}

fn managed_file_inventory(
    active: Option<&crate::domain::ConfigMetadata>,
    pending: Option<&crate::domain::ConfigMetadata>,
    draft: &crate::domain::ConfigMetadata,
) -> Vec<RetainedManagedFile> {
    let mut files: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
    if let Some(active) = active {
        for file in &active.files {
            files.entry(file.logical_name.clone()).or_default().0 = true;
        }
    }
    if let Some(pending) = pending {
        for file in &pending.files {
            files.entry(file.logical_name.clone()).or_default().1 = true;
        }
    }
    for file in &draft.files {
        files.entry(file.logical_name.clone()).or_default().2 = true;
    }
    files
        .into_iter()
        .map(
            |(logical_name, (active, pending, draft))| RetainedManagedFile {
                logical_name,
                configured_in: configured_scope(active, pending, draft),
            },
        )
        .collect()
}

async fn retained_resources(
    state: &AppState,
    app_id: Uuid,
    active: Option<&crate::domain::ConfigMetadata>,
    pending: Option<&crate::domain::ConfigMetadata>,
    draft: &crate::domain::ConfigMetadata,
    containers: &[String],
) -> Result<RetainedResources, &'static str> {
    tokio::time::timeout(
        Duration::from_secs(5),
        retained_resources_inner(state, app_id, active, pending, draft, containers),
    )
    .await
    .map_err(|_| "DOCKER_TIMEOUT")?
}

async fn retained_resources_inner(
    state: &AppState,
    app_id: Uuid,
    active: Option<&crate::domain::ConfigMetadata>,
    pending: Option<&crate::domain::ConfigMetadata>,
    draft: &crate::domain::ConfigMetadata,
    containers: &[String],
) -> Result<RetainedResources, &'static str> {
    let mut owned: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
    let mut external: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
    let mut networks: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
    let mut binds: BTreeMap<String, (bool, bool, bool, bool)> = BTreeMap::new();
    for (metadata, is_active) in active
        .into_iter()
        .map(|metadata| (metadata, 0_u8))
        .chain(pending.into_iter().map(|metadata| (metadata, 1_u8)))
        .chain(std::iter::once((draft, 2_u8)))
    {
        for volume in &metadata.volumes {
            let entry = match volume {
                crate::domain::VolumeInput::Owned { logical_name, .. } => owned
                    .entry(format!("solodock-{}-{logical_name}", app_id.simple()))
                    .or_default(),
                crate::domain::VolumeInput::External { name, .. } => {
                    external.entry(name.clone()).or_default()
                }
            };
            match is_active {
                0 => entry.0 = true,
                1 => entry.1 = true,
                _ => entry.2 = true,
            }
        }
        let default = networks
            .entry(format!("solodock-{}-default", app_id.simple()))
            .or_default();
        match is_active {
            0 => default.0 = true,
            1 => default.1 = true,
            _ => default.2 = true,
        }
        for network in &metadata.networks {
            if let crate::domain::NetworkInput::External { name } = network {
                let entry = networks.entry(name.clone()).or_default();
                match is_active {
                    0 => entry.0 = true,
                    1 => entry.1 = true,
                    _ => entry.2 = true,
                }
            }
        }
        for bind in &metadata.binds {
            let entry = binds
                .entry(bind.source.clone())
                .or_insert((false, false, false, true));
            match is_active {
                0 => entry.0 = true,
                1 => entry.1 = true,
                _ => entry.2 = true,
            }
            entry.3 &= bind.readonly;
        }
    }

    let mut owned_volumes = Vec::new();
    for (name, (active, pending, draft)) in owned {
        let exists = state
            .observer
            .api
            .inspect_volume(&name)
            .await
            .map_err(|error| error.public_code())?
            .is_some();
        owned_volumes.push(RetainedNamedResource {
            name,
            configured_in: configured_scope(active, pending, draft),
            exists,
        });
    }
    let mut external_volumes = Vec::new();
    for (name, (active, pending, draft)) in external {
        let exists = state
            .observer
            .api
            .inspect_volume(&name)
            .await
            .map_err(|error| error.public_code())?
            .is_some();
        external_volumes.push(RetainedNamedResource {
            name,
            configured_in: configured_scope(active, pending, draft),
            exists,
        });
    }
    let mut retained_networks = Vec::new();
    for (name, (active, pending, draft)) in networks {
        let exists = state
            .observer
            .api
            .inspect_network(&name)
            .await
            .map_err(|error| error.public_code())?
            .is_some();
        retained_networks.push(RetainedNamedResource {
            name,
            configured_in: configured_scope(active, pending, draft),
            exists,
        });
    }
    Ok(RetainedResources {
        containers: containers.to_vec(),
        owned_volumes,
        external_volumes,
        binds: binds
            .into_iter()
            .map(
                |(source, (active, pending, draft, readonly))| RetainedBind {
                    exists: std::fs::symlink_metadata(&source)
                        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                        .unwrap_or(false),
                    source,
                    readonly,
                    configured_in: configured_scope(active, pending, draft),
                },
            )
            .collect(),
        networks: retained_networks,
    })
}

fn load_config(
    services: &M3Services,
    app_id: Uuid,
    revision: Uuid,
) -> Result<config_revision::LoadedRevision, StoreError> {
    config_revision::load_verified(
        &services.store.app_directory(app_id),
        revision,
        &services.idempotency.integrity_key(),
    )
}

fn current_draft_input(
    app: &crate::docker::AppCatalogEntry,
    loaded: &config_revision::LoadedRevision,
) -> DraftInput {
    loaded.input(
        app.slug.clone(),
        app.display_name.clone(),
        app.discovery_image_ref.clone().unwrap_or_default(),
        app.draft.as_ref().and_then(|draft| draft.credential_ref),
        app.auto_deploy_enabled,
        app.poll_interval_seconds,
    )
}

pub(crate) async fn validate_resources(
    state: &AppState,
    app_id: Uuid,
    metadata: &crate::domain::ConfigMetadata,
    request_id: RequestId,
) -> Result<(), ApiError> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        validate_resources_inner(state, app_id, metadata, request_id),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ApiError::docker(request_id, "DOCKER_TIMEOUT")),
    }
}

async fn validate_resources_inner(
    state: &AppState,
    app_id: Uuid,
    metadata: &crate::domain::ConfigMetadata,
    request_id: RequestId,
) -> Result<(), ApiError> {
    let expected = crate::compose::generate::resource_labels(app_id);
    for volume in &metadata.volumes {
        match volume {
            crate::domain::VolumeInput::Owned { logical_name, .. } => {
                let name = format!("solodock-{}-{logical_name}", app_id.simple());
                if let Some(resource) = state
                    .observer
                    .api
                    .inspect_volume(&name)
                    .await
                    .map_err(|error| ApiError::docker(request_id, error.public_code()))?
                    && (resource.name != name || !has_exact_ownership(&resource.labels, &expected))
                {
                    return Err(ApiError::conflict("VOLUME_OWNERSHIP_CONFLICT", request_id));
                }
            }
            crate::domain::VolumeInput::External { name, .. } => {
                if state
                    .observer
                    .api
                    .inspect_volume(name)
                    .await
                    .map_err(|error| ApiError::docker(request_id, error.public_code()))?
                    .is_none()
                {
                    return Err(ApiError::conflict("EXTERNAL_VOLUME_NOT_FOUND", request_id));
                }
            }
        }
    }
    let owned_name = format!("solodock-{}-default", app_id.simple());
    if let Some(resource) = state
        .observer
        .api
        .inspect_network(&owned_name)
        .await
        .map_err(|error| ApiError::docker(request_id, error.public_code()))?
        && (resource.name != owned_name || !has_exact_ownership(&resource.labels, &expected))
    {
        return Err(ApiError::conflict("NETWORK_OWNERSHIP_CONFLICT", request_id));
    }
    for network in &metadata.networks {
        if let crate::domain::NetworkInput::External { name } = network
            && state
                .observer
                .api
                .inspect_network(name)
                .await
                .map_err(|error| ApiError::docker(request_id, error.public_code()))?
                .is_none()
        {
            return Err(ApiError::conflict("EXTERNAL_NETWORK_NOT_FOUND", request_id));
        }
    }
    Ok(())
}

fn has_exact_ownership(
    actual: &std::collections::HashMap<String, String>,
    expected: &std::collections::BTreeMap<String, String>,
) -> bool {
    expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::{
        auth::AuthService,
        db::Database,
        docker::{
            AppCatalog, DockerObserver,
            models::{
                ContainerRecord, DockerError, DockerReadApi, DockerResource, DockerStream,
                HealthStatus, LogChunk, LogRequest, ProbeSnapshot, RawDockerEvent, RawStats,
            },
            probe::DockerSupervisor,
        },
    };
    use async_trait::async_trait;

    fn container(id: &str, status: ContainerStatus, started_at: Option<&str>) -> ContainerRecord {
        ContainerRecord {
            id: id.into(),
            name: "app".into(),
            labels: HashMap::new(),
            status,
            health: HealthStatus::None,
            exit_code: None,
            restart_count: Some(0),
            started_at: started_at.map(str::to_owned),
            finished_at: None,
            configured_image_ref: None,
            image_id: None,
            ports: vec![],
            mounts: vec![],
            networks: vec![],
        }
    }

    #[test]
    fn lifecycle_effect_marker_distinguishes_pre_spawn_completion_and_replacement() {
        let marker = EffectMarker {
            phase: "started".into(),
            pre_container_id: Some("full-original-id".into()),
            pre_started_at: Some("before".into()),
            post_container_id: None,
        };
        let before = container("full-original-id", ContainerStatus::Running, Some("before"));
        assert!(observation_matches_marker(&marker, Some(&before)));
        assert!(!effect_completed(
            LifecycleAction::Restart,
            &marker,
            Some(&before)
        ));

        let restarted = container("full-original-id", ContainerStatus::Running, Some("after"));
        assert!(effect_completed(
            LifecycleAction::Restart,
            &marker,
            Some(&restarted)
        ));
        let replacement = container(
            "full-replacement-id",
            ContainerStatus::Running,
            Some("after"),
        );
        assert!(!observation_matches_marker(&marker, Some(&replacement)));
        assert!(!effect_completed(
            LifecycleAction::Restart,
            &marker,
            Some(&replacement)
        ));
    }

    #[test]
    fn recreate_completion_requires_a_new_exact_container() {
        let marker = EffectMarker {
            phase: "started".into(),
            pre_container_id: None,
            pre_started_at: None,
            post_container_id: Some("new-full-id".into()),
        };
        assert!(observation_matches_marker(&marker, None));
        let created = container("new-full-id", ContainerStatus::Running, Some("after"));
        assert!(effect_completed(
            LifecycleAction::Start,
            &marker,
            Some(&created)
        ));
        let partial = container("new-full-id", ContainerStatus::Exited, Some("after"));
        assert_eq!(
            marker.post_container_id.as_deref(),
            Some(partial.id.as_str())
        );
    }

    #[tokio::test]
    async fn partial_recreate_full_id_survives_interruption_and_same_key_resume() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let idempotency = IdempotencyService::initialize(database, root.path()).unwrap();
        let route = "/api/v1/apps/example/actions/start";
        let key = "partial-recreate-key";
        assert!(matches!(
            idempotency
                .claim(route, key, b"request", Uuid::new_v4())
                .await
                .unwrap(),
            ClaimResult::New(_)
        ));
        idempotency
            .mark_effect_started(route, key, None, None)
            .await
            .unwrap();
        idempotency
            .mark_effect_observed(route, key, "created-full-id")
            .await
            .unwrap();
        idempotency
            .mark_interrupted(route, key, Uuid::new_v4())
            .await
            .unwrap();
        assert!(matches!(
            idempotency
                .claim(route, key, b"request", Uuid::new_v4())
                .await
                .unwrap(),
            ClaimResult::Resume(_)
        ));
        let marker = idempotency
            .effect_marker(route, key)
            .await
            .unwrap()
            .unwrap();
        let partial = container("created-full-id", ContainerStatus::Exited, Some("created"));
        assert!(partial_recreate_can_continue(
            LifecycleAction::Start,
            &marker,
            Some(&partial)
        ));
        let replacement = container(
            "replacement-full-id",
            ContainerStatus::Exited,
            Some("created"),
        );
        assert!(!partial_recreate_can_continue(
            LifecycleAction::Start,
            &marker,
            Some(&replacement)
        ));
    }

    #[tokio::test]
    async fn delete_effect_marker_query_failure_marks_operation_resumable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let idempotency = IdempotencyService::initialize(database, root.path()).unwrap();
        let route = "/api/v1/apps/example";
        let key = "delete-effect-query-failure";
        assert!(matches!(
            idempotency
                .claim(route, key, b"request", Uuid::new_v4())
                .await
                .unwrap(),
            ClaimResult::New(_)
        ));
        idempotency.fail_next_effect_marker_for_test();
        assert!(
            load_delete_effect_marker(&idempotency, route, key, RequestId(Uuid::new_v4()))
                .await
                .is_err()
        );
        assert!(matches!(
            idempotency
                .claim(route, key, b"request", Uuid::new_v4())
                .await
                .unwrap(),
            ClaimResult::Resume(_)
        ));
    }

    #[test]
    fn mutation_candidates_fail_closed_for_stale_invalid_and_project_collisions() {
        use crate::docker::ownership::{
            APP_ID_LABEL, MANAGED_LABEL, ONEOFF_LABEL, PROJECT_LABEL, RELEASE_ID_LABEL,
            SCHEMA_LABEL, SERVICE_LABEL,
        };
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let app = AppCatalogEntry {
            id: app_id,
            slug: "example".into(),
            display_name: "Example".into(),
            project_name: format!("solodock-{}", app_id.simple()),
            active_release_id: Some(release_id),
            active_image_ref: Some(format!("example@sha256:{}", "a".repeat(64))),
            active_config_revision: None,
            active_config_sha256: None,
            pending_release_id: None,
            pending_image_ref: None,
            pending_config_revision: None,
            discovery_image_ref: None,
            draft_revision: None,
            draft_config_sha256: None,
            desired_state: DesiredState::Stopped,
            auto_deploy_enabled: false,
            poll_interval_seconds: 300,
            draft: None,
        };
        let labels = HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (SCHEMA_LABEL.into(), "1".into()),
            (APP_ID_LABEL.into(), app_id.to_string()),
            (RELEASE_ID_LABEL.into(), release_id.to_string()),
            (PROJECT_LABEL.into(), app.project_name.clone()),
            (SERVICE_LABEL.into(), "app".into()),
            (ONEOFF_LABEL.into(), "False".into()),
        ]);
        let mut valid = container(&"a".repeat(64), ContainerStatus::Running, Some("now"));
        valid.labels = labels.clone();
        let mut stale = valid.clone();
        stale
            .labels
            .insert(RELEASE_ID_LABEL.into(), Uuid::new_v4().to_string());
        assert_eq!(
            classify_mutation_candidates(&app, vec![stale]).unwrap_err(),
            "APP_CONTAINER_INVALID"
        );
        let mut unmanaged = valid.clone();
        unmanaged
            .labels
            .insert(MANAGED_LABEL.into(), "false".into());
        assert_eq!(
            classify_mutation_candidates(&app, vec![unmanaged]).unwrap_err(),
            "APP_CONTAINER_INVALID"
        );
        assert_eq!(
            classify_mutation_candidates(
                &app,
                vec![
                    valid,
                    container(&"b".repeat(64), ContainerStatus::Running, None)
                ]
            )
            .unwrap_err(),
            "APP_CONTAINER_AMBIGUOUS"
        );
    }

    struct SlowResources;

    #[async_trait]
    impl DockerReadApi for SlowResources {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            unreachable!()
        }
        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            unreachable!()
        }
        async fn inspect_container(&self, _: &str) -> Result<ContainerRecord, DockerError> {
            unreachable!()
        }
        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            unreachable!()
        }
        async fn logs(
            &self,
            _: &str,
            _: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            unreachable!()
        }
        async fn stats(&self, _: &str) -> Result<DockerStream<RawStats>, DockerError> {
            unreachable!()
        }
        async fn inspect_volume(&self, name: &str) -> Result<Option<DockerResource>, DockerError> {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(Some(DockerResource {
                name: name.into(),
                labels: HashMap::new(),
            }))
        }
        async fn inspect_network(&self, name: &str) -> Result<Option<DockerResource>, DockerError> {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(Some(DockerResource {
                name: name.into(),
                labels: HashMap::new(),
            }))
        }
    }

    #[tokio::test]
    async fn resource_preflight_has_one_deadline_for_twenty_four_inspects() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let auth = AuthService::new(database, root.path().join("bootstrap.token"));
        let mut state =
            AppState::control_plane(auth, "https://example.com".into(), root.path().to_owned());
        state.observer = DockerObserver::new(
            Arc::new(SlowResources),
            AppCatalog::default(),
            DockerSupervisor::new(),
        );
        let metadata = crate::domain::ConfigMetadata {
            schema_version: 1,
            public_env_keys: vec![],
            secret_keys: vec![],
            secret_hmacs: Default::default(),
            files: vec![],
            public_file_sha256s: Default::default(),
            secret_file_hmacs: Default::default(),
            ports: vec![],
            volumes: (0..16)
                .map(|index| crate::domain::VolumeInput::External {
                    name: format!("volume-{index}"),
                    target_path: format!("/volume/{index}"),
                })
                .collect(),
            binds: vec![],
            networks: (0..8)
                .map(|index| crate::domain::NetworkInput::External {
                    name: format!("network-{index}"),
                })
                .collect(),
            health: crate::domain::HealthPolicy::default(),
            config_sha256: "0".repeat(64),
        };
        let started = tokio::time::Instant::now();
        assert!(
            validate_resources(&state, Uuid::new_v4(), &metadata, RequestId(Uuid::new_v4()))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(6));
    }
}
