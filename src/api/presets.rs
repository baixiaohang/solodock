use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use zeroize::Zeroize;

use super::{AppState, auth::MutationAuthenticated, mutations};
use crate::{
    app_store::StoreError,
    domain::{ExistingSecrets, normalize_draft, validate_slug},
    error::{ApiError, RequestId},
    mutation::ClaimResult,
    presets::{self, pgadmin, postgresql},
};

const ROUTE: &str = "/api/v1/apps/from-preset";

pub async fn list(
    _authenticated: super::auth::Authenticated,
) -> Json<Vec<presets::PresetDescriptor>> {
    Json(presets::descriptors())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFromPresetRequest {
    slug: String,
    preset_id: String,
    preset_schema_version: u32,
    variables: PresetVariables,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum PresetVariables {
    PostgreSql(postgresql::Variables),
    PgAdmin(pgadmin::Variables),
}

fn render(input: CreateFromPresetRequest) -> Result<crate::domain::DraftInput, &'static str> {
    match (
        input.preset_id.as_str(),
        input.preset_schema_version,
        input.variables,
    ) {
        (
            postgresql::PRESET_ID,
            postgresql::SCHEMA_VERSION,
            PresetVariables::PostgreSql(variables),
        ) => postgresql::render(&input.slug, variables),
        (pgadmin::PRESET_ID, pgadmin::SCHEMA_VERSION, PresetVariables::PgAdmin(variables)) => {
            pgadmin::render(&input.slug, variables)
        }
        _ => Err("PRESET_UNSUPPORTED"),
    }
}

pub async fn create(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(_): MutationAuthenticated,
    payload: Result<Json<CreateFromPresetRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let services = mutations::services(&state, request_id)?;
    let Json(input) = payload.map_err(|_| ApiError::validation(request_id))?;
    validate_slug(&input.slug).map_err(|error| ApiError::domain(error, request_id))?;
    let raw_key = mutations::idempotency_key(&headers)
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    let mut canonical = serde_json::to_vec(&serde_json::json!({
        "actor": "admin",
        "route": ROUTE,
        "slug": input.slug,
        "preset_id": input.preset_id,
        "preset_schema_version": input.preset_schema_version,
        "variables": input.variables,
    }))
    .map_err(|_| ApiError::internal(request_id))?;
    let request_hmac = services.idempotency.fingerprint(&canonical);
    canonical.zeroize();
    let claim = services
        .idempotency
        .claim(ROUTE, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        let _ = mutations::refresh(&state, services).await;
        return mutations::replay_recorded(status, body);
    }
    let operation_id = match claim {
        ClaimResult::New(id) | ClaimResult::Resume(id) => id,
        ClaimResult::Replay { .. } => unreachable!(),
    };
    let slug = input.slug.clone();
    let draft_input = match render(input) {
        Ok(value) => value,
        Err(code) => {
            return mutations::finish_error(
                services,
                ROUTE,
                raw_key,
                code,
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
    };
    let draft = match normalize_draft(
        draft_input,
        &ExistingSecrets::default(),
        &services.idempotency.fingerprint(b"config"),
        &services.store.allowed_bind_roots(),
    ) {
        Ok(value) => value,
        Err(error) => {
            return mutations::finish_error(
                services,
                ROUTE,
                raw_key,
                error.public_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                request_id,
            )
            .await;
        }
    };
    let _catalog = services.coordinator.catalog_lock().await;
    if let Ok(existing) = services.store.read_metadata(operation_id)
        && existing.last_operation_id == operation_id
        && existing.draft_revision == Some(operation_id)
    {
        let warning = mutations::refresh(&state, services).await;
        return mutations::finish_json(
            services,
            ROUTE,
            raw_key,
            StatusCode::CREATED,
            &mutations::MutationResponse {
                app: mutations::detail(&existing, Some(&draft)),
                idempotency_replayed: false,
                projection_warning: warning,
            },
            request_id,
        )
        .await;
    }
    let report = services
        .store
        .scan_read_only()
        .map_err(|_| ApiError::internal(request_id))?;
    if report.valid_apps.iter().any(|app| app.slug == slug) {
        return mutations::finish_error(
            services,
            ROUTE,
            raw_key,
            "APP_SLUG_CONFLICT",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }
    let _app = services.coordinator.try_app(operation_id).map_err(|_| {
        ApiError::new(
            StatusCode::CONFLICT,
            "APP_BUSY",
            "Application is busy",
            request_id,
        )
    })?;
    let metadata = match services.store.create_app(
        operation_id,
        &slug,
        operation_id,
        Some((operation_id, &draft)),
        OffsetDateTime::now_utc(),
    ) {
        Ok(value) => value,
        Err(StoreError::AppConflict) => {
            let existing = services
                .store
                .read_metadata(operation_id)
                .map_err(|_| ApiError::internal(request_id))?;
            if existing.last_operation_id != operation_id {
                return mutations::finish_error(
                    services,
                    ROUTE,
                    raw_key,
                    "APP_SLUG_CONFLICT",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
            existing
        }
        Err(_) => return mutations::interrupt_internal(services, ROUTE, raw_key, request_id).await,
    };
    let warning = mutations::refresh(&state, services).await;
    mutations::finish_json(
        services,
        ROUTE,
        raw_key,
        StatusCode::CREATED,
        &mutations::MutationResponse {
            app: mutations::detail(&metadata, Some(&draft)),
            idempotency_replayed: false,
            projection_warning: warning,
        },
        request_id,
    )
    .await
}
