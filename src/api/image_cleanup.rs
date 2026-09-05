use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    AppState,
    auth::MutationAuthenticated,
    mutations::{
        M3Services, finish_error, finish_json, idempotency_key, interrupt_internal,
        replay_recorded, services,
    },
};
use crate::{
    db::{format_time, parse_time},
    docker::image_cleanup::{ExactImageId, RemoveImageResult},
    error::{ApiError, RequestId},
    image_cleanup::{ImageCandidate, ImagePlan},
    mutation::ClaimResult,
    security::secret::SecretValue,
    storage_cleanup::plan_hash,
};

const ROUTE: &str = "/api/v1/system/image-cleanup/apply";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {}

pub async fn preview(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    MutationAuthenticated(auth): MutationAuthenticated,
    payload: Result<Json<PreviewRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let _ = payload.map_err(|_| ApiError::validation(id))?;
    let m3 = services(&state, id)?;
    let _catalog = m3.coordinator.catalog_lock().await;
    let plan =
        crate::image_cleanup::build_plan(&m3.store, &m3.database, state.image_cleanup.as_ref())
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::CONFLICT,
                    "CLEANUP_INVENTORY_INCOMPLETE",
                    "A complete safe cleanup inventory is unavailable",
                    id,
                )
            })?;
    let json = serde_json::to_string(&plan).map_err(|_| ApiError::internal(id))?;
    let token = SecretValue::random().map_err(|_| ApiError::internal(id))?;
    let hmac = m3.idempotency.fingerprint(token.expose().as_bytes());
    let expires = format_time(OffsetDateTime::now_utc() + time::Duration::minutes(5))
        .map_err(|_| ApiError::internal(id))?;
    sqlx::query("INSERT INTO image_cleanup_previews (token_hmac,session_id,plan_json,expires_at) VALUES (?,?,?,?)")
        .bind(&hmac).bind(&auth.session.id).bind(json).bind(&expires).execute(m3.database.pool()).await.map_err(|_|ApiError::internal(id))?;
    // Do not expose daemon repository/tag strings or internal inventory facts.
    Ok((StatusCode::OK,Json(serde_json::json!({"candidates":plan.candidates.iter().map(|c|serde_json::json!({"image_id":c.image_id,"manifest_digest":c.identity.manifest_digest,"platform_os":c.identity.platform_os,"platform_architecture":c.identity.platform_architecture,"platform_variant":c.identity.platform_variant,"reported_size_bytes":c.reported_size_bytes})).collect::<Vec<_>>(),"protected_count":plan.protected_count,"confirmation_token":token.expose(),"expires_at":expires}))).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    confirmation_token: SecretValue,
    image_ids: Vec<ExactImageId>,
    acknowledge_image_removal: bool,
}

enum Failure {
    Known(&'static str),
    Unknown,
}
impl From<sqlx::Error> for Failure {
    fn from(_: sqlx::Error) -> Self {
        Self::Unknown
    }
}

pub async fn apply(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(auth): MutationAuthenticated,
    payload: Result<Json<ApplyRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = services(&state, id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(id))?;
    let key = idempotency_key(&headers).map_err(|e| ApiError::idempotency(e, id))?;
    let token_hmac = m3
        .idempotency
        .fingerprint(payload.confirmation_token.expose().as_bytes());
    let fingerprint=m3.idempotency.fingerprint(serde_json::json!({"actor":"admin","method":"POST","route":ROUTE,"token_hmac":token_hmac,"image_ids":payload.image_ids,"acknowledge_image_removal":payload.acknowledge_image_removal}).to_string().as_bytes());
    let claim = m3
        .idempotency
        .claim(ROUTE, key, &fingerprint, id.0)
        .await
        .map_err(|e| ApiError::idempotency(e, id))?;
    let operation = match claim {
        ClaimResult::Replay { status, body, .. } => return replay_recorded(status, body),
        ClaimResult::New(op) | ClaimResult::Resume(op) => op,
    };
    match execute(
        &state,
        m3,
        operation,
        &token_hmac,
        &auth.session.id,
        &payload,
        id,
    )
    .await
    {
        Ok(body) => finish_json(m3, ROUTE, key, StatusCode::OK, &body, id).await,
        Err(Failure::Known(code)) => {
            finish_error(m3, ROUTE, key, code, StatusCode::CONFLICT, id).await
        }
        Err(Failure::Unknown) => interrupt_internal(m3, ROUTE, key, id).await,
    }
}

async fn execute(
    state: &AppState,
    m3: &M3Services,
    operation: Uuid,
    token_hmac: &[u8],
    session: &str,
    payload: &ApplyRequest,
    id: RequestId,
) -> Result<serde_json::Value, Failure> {
    let published = sqlx::query(
        "SELECT token_hmac,plan_json,plan_hash FROM image_cleanup_operations WHERE operation_id=?",
    )
    .bind(operation.to_string())
    .fetch_optional(m3.database.pool())
    .await?;
    let reject = |code| {
        if published.is_some() {
            Failure::Unknown
        } else {
            Failure::Known(code)
        }
    };
    let row=sqlx::query("SELECT session_id,plan_json,expires_at,consumed_at FROM image_cleanup_previews WHERE token_hmac=?").bind(token_hmac).fetch_optional(m3.database.pool()).await?.ok_or_else(||reject("CLEANUP_PREVIEW_INVALID"))?;
    if row.get::<String, _>("session_id") != session
        || !payload.acknowledge_image_removal
        || payload.image_ids.is_empty()
        || payload.image_ids.len() > 100
        || payload.image_ids.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(reject("CLEANUP_PREVIEW_INVALID"));
    }
    let json: String = row.get("plan_json");
    let preview: ImagePlan = serde_json::from_str(&json).map_err(|_| Failure::Unknown)?;
    let selected: Vec<ImageCandidate> = payload
        .image_ids
        .iter()
        .map(|id| {
            preview
                .candidates
                .iter()
                .find(|c| &c.image_id == id)
                .cloned()
                .ok_or_else(|| reject("CLEANUP_PREVIEW_INVALID"))
        })
        .collect::<Result<_, _>>()?;
    let selected_json = serde_json::to_string(&selected).map_err(|_| Failure::Unknown)?;
    let hash = plan_hash(&selected_json);
    if let Some(stored) = &published {
        if stored.get::<Vec<u8>, _>("token_hmac") != token_hmac
            || stored.get::<String, _>("plan_json") != selected_json
            || stored.get::<Vec<u8>, _>("plan_hash") != hash
            || row.get::<Option<String>, _>("consumed_at").is_none()
        {
            return Err(Failure::Unknown);
        }
    } else {
        if row.get::<Option<String>, _>("consumed_at").is_some() {
            return Err(reject("CLEANUP_PREVIEW_INVALID"));
        }
        if parse_time(row.get("expires_at")).map_err(|_| Failure::Unknown)?
            <= OffsetDateTime::now_utc()
        {
            return Err(reject("CLEANUP_PREVIEW_EXPIRED"));
        }
    }
    let _catalog = m3.coordinator.catalog_lock().await;
    let app_ids = crate::image_cleanup::current_app_ids(&m3.store, &m3.database)
        .await
        .map_err(|_| reject("CLEANUP_INVENTORY_INCOMPLETE"))?;
    let mut guards = Vec::new();
    for app in app_ids {
        guards.push(
            m3.coordinator
                .try_app(app)
                .map_err(|_| reject("APP_BUSY"))?,
        );
    }
    let _compose = m3
        .coordinator
        .try_compose()
        .map_err(|_| reject("APP_BUSY"))?;
    let current = crate::image_cleanup::build_plan_for_operation(
        &m3.store,
        &m3.database,
        state.image_cleanup.as_ref(),
        Some(operation),
    )
    .await
    .map_err(|_| reject("CLEANUP_INVENTORY_INCOMPLETE"))?;
    if published.is_none() {
        if current != preview {
            return Err(reject("CLEANUP_PREVIEW_STALE"));
        }
        publish(
            m3,
            operation,
            token_hmac,
            &selected_json,
            &hash,
            &selected,
            id,
        )
        .await?;
    }
    // Verify the whole ledger before effects; a mismatched/extra item must never
    // be interpreted as a new authorization to remove an image.
    let rows=sqlx::query("SELECT ordinal,image_id,status FROM image_cleanup_items WHERE operation_id=? ORDER BY ordinal").bind(operation.to_string()).fetch_all(m3.database.pool()).await?;
    if rows.len() != selected.len() {
        return Err(Failure::Unknown);
    }
    for (ordinal, (row, candidate)) in rows.iter().zip(&selected).enumerate() {
        if row.get::<i64, _>("ordinal") != ordinal as i64
            || row.get::<String, _>("image_id") != candidate.image_id.as_str()
            || !matches!(
                row.get::<&str, _>("status"),
                "planned" | "started" | "removed" | "retained"
            )
        {
            return Err(Failure::Unknown);
        }
    }
    for (ordinal, (row, candidate)) in rows.iter().zip(&selected).enumerate() {
        let previous: &str = row.get("status");
        if !matches!(previous, "removed" | "retained") {
            let eligible = current.candidates.iter().any(|value| value == candidate);
            let observed = state
                .image_cleanup
                .inspect(&candidate.image_id)
                .await
                .map_err(|_| Failure::Unknown)?;
            let next = match observed {
                None => {
                    if previous == "started" {
                        "removed"
                    } else {
                        "retained"
                    }
                }
                Some(observed)
                    if !eligible
                        || observed.image.id != candidate.image_id.as_str()
                        || !crate::image_cleanup::matches_inspect(
                            &candidate.identity,
                            &observed,
                        )
                        .map_err(|_| Failure::Unknown)? =>
                {
                    "retained"
                }
                Some(_) => {
                    sqlx::query("UPDATE image_cleanup_items SET status='started' WHERE operation_id=? AND ordinal=? AND status IN ('planned','started')").bind(operation.to_string()).bind(ordinal as i64).execute(m3.database.pool()).await?;
                    match state
                        .image_cleanup
                        .remove(&candidate.image_id)
                        .await
                        .map_err(|_| Failure::Unknown)?
                    {
                        RemoveImageResult::Retained => "retained",
                        RemoveImageResult::Accepted => {
                            if state
                                .image_cleanup
                                .inspect(&candidate.image_id)
                                .await
                                .map_err(|_| Failure::Unknown)?
                                .is_none()
                            {
                                "removed"
                            } else {
                                "retained"
                            }
                        }
                    }
                }
            };
            sqlx::query(
                "UPDATE image_cleanup_items SET status=? WHERE operation_id=? AND ordinal=?",
            )
            .bind(next)
            .bind(operation.to_string())
            .bind(ordinal as i64)
            .execute(m3.database.pool())
            .await?;
        }
    }
    crate::image_cleanup::terminal_result(&m3.database, operation, &selected, &hash)
        .await
        .map_err(|_| Failure::Unknown)?
        .ok_or(Failure::Unknown)
}

async fn publish(
    m3: &M3Services,
    operation: Uuid,
    token_hmac: &[u8],
    json: &str,
    hash: &[u8],
    selected: &[ImageCandidate],
    id: RequestId,
) -> Result<(), Failure> {
    let now = format_time(OffsetDateTime::now_utc()).map_err(|_| Failure::Unknown)?;
    let mut tx = m3.database.pool().begin().await?;
    if sqlx::query("UPDATE image_cleanup_previews SET consumed_at=? WHERE token_hmac=? AND consumed_at IS NULL").bind(&now).bind(token_hmac).execute(&mut *tx).await?.rows_affected()!=1 {return Err(Failure::Unknown);}
    sqlx::query("INSERT INTO image_cleanup_operations (operation_id,token_hmac,plan_json,plan_hash,created_at) VALUES (?,?,?,?,?)").bind(operation.to_string()).bind(token_hmac).bind(json).bind(hash).bind(&now).execute(&mut *tx).await?;
    for (ordinal, item) in selected.iter().enumerate() {
        sqlx::query("INSERT INTO image_cleanup_items (operation_id,ordinal,image_id,status) VALUES (?,?,?,'planned')").bind(operation.to_string()).bind(ordinal as i64).bind(item.image_id.as_str()).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('admin',?,'image_cleanup_apply','cleanup',?,'planned',?,?)").bind(id.0.to_string()).bind(operation.to_string()).bind(serde_json::json!({"kind":"images","items":selected.len()}).to_string()).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
