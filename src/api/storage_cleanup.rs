use std::collections::{BTreeSet, HashSet};

use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    AppState,
    auth::MutationAuthenticated,
    mutations::{
        finish_error, finish_json, idempotency_key, interrupt_internal, replay_recorded, services,
    },
};
use crate::{
    app_store::cleanup::{CleanupArtifact, DetachResult},
    db::{format_time, parse_time},
    error::{ApiError, RequestId},
    mutation::ClaimResult,
    security::secret::SecretValue,
    storage_cleanup::{
        CleanupCandidate, CleanupError, CleanupPlan, ProtectionReason, build_plan,
        canonical_plan_json, plan_hash,
    },
};

const APPLY_ROUTE: &str = "/api/v1/system/storage-cleanup/apply";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {}

#[derive(Serialize)]
pub struct PreviewResponse {
    candidates: Vec<PreviewCandidateResponse>,
    protected: Vec<ProtectedSummaryResponse>,
    estimated_logical_bytes: u64,
    confirmation_token: String,
    expires_at: String,
}

#[derive(Serialize)]
struct ProtectedSummaryResponse {
    reason: ProtectionReason,
    count: usize,
}

#[derive(Serialize)]
struct PreviewCandidateResponse {
    app_id: Option<Uuid>,
    artifact_kind: &'static str,
    artifact_id: String,
    estimated_logical_bytes: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    release_created_at: Option<OffsetDateTime>,
}

pub async fn preview(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    MutationAuthenticated(authenticated): MutationAuthenticated,
    payload: Result<Json<PreviewRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = services(&state, request_id)?;
    let _ = payload.map_err(|_| ApiError::validation(request_id))?;
    let _catalog = m3.coordinator.catalog_lock().await;
    let plan = build_plan(&m3.store, &m3.database)
        .await
        .map_err(|error| cleanup_error(error, request_id))?;
    let preview_json = canonical_plan_json(&plan).map_err(|_| ApiError::internal(request_id))?;
    let facts_hash = plan_hash(&preview_json);
    let token = SecretValue::random().map_err(|_| ApiError::internal(request_id))?;
    let token_hmac = m3.idempotency.fingerprint(token.expose().as_bytes());
    let now = OffsetDateTime::now_utc();
    let expires_at = now + time::Duration::minutes(5);
    sqlx::query("INSERT INTO storage_cleanup_previews (token_hmac,session_id,cleanup_kind,facts_hash,preview_json,expires_at,created_at) VALUES (?,?,'artifacts',?,?,?,?)")
        .bind(&token_hmac)
        .bind(&authenticated.session.id)
        .bind(&facts_hash)
        .bind(&preview_json)
        .bind(format_time(expires_at).map_err(|_| ApiError::internal(request_id))?)
        .bind(format_time(now).map_err(|_| ApiError::internal(request_id))?)
        .execute(m3.database.pool())
        .await
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::OK,
        Json(PreviewResponse {
            candidates: plan
                .candidates
                .iter()
                .enumerate()
                .map(|(ordinal, candidate)| preview_candidate(ordinal, candidate))
                .collect(),
            protected: protected_summary(&plan),
            estimated_logical_bytes: plan.estimated_logical_bytes,
            confirmation_token: token.expose().to_owned(),
            expires_at: format_time(expires_at).expect("valid cleanup preview expiry"),
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    confirmation_token: SecretValue,
    acknowledge_rollback_loss: bool,
}

#[derive(Serialize)]
pub struct ApplyResponse {
    pub operation_id: Uuid,
    pub plan_hash: String,
    pub status: &'static str,
    pub items: Vec<serde_json::Value>,
    pub idempotency_replayed: bool,
}

pub async fn apply(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    MutationAuthenticated(authenticated): MutationAuthenticated,
    payload: Result<Json<ApplyRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = services(&state, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let raw_key =
        idempotency_key(&headers).map_err(|error| ApiError::idempotency(error, request_id))?;
    let token_hmac = m3
        .idempotency
        .fingerprint(payload.confirmation_token.expose().as_bytes());
    let canonical = serde_json::to_vec(&serde_json::json!({
        "actor": "admin",
        "method": "POST",
        "route": APPLY_ROUTE,
        "token_hmac": token_hmac,
        "acknowledge_rollback_loss": payload.acknowledge_rollback_loss,
    }))
    .map_err(|_| ApiError::internal(request_id))?;
    let request_hmac = m3.idempotency.fingerprint(&canonical);
    let claim = m3
        .idempotency
        .claim(APPLY_ROUTE, raw_key, &request_hmac, request_id.0)
        .await
        .map_err(|error| ApiError::idempotency(error, request_id))?;
    if let ClaimResult::Replay {
        operation_id: _,
        status,
        body,
    } = claim
    {
        let _catalog = m3.coordinator.catalog_lock().await;
        if crate::storage_cleanup::finalize_succeeded(&m3.store, &m3.database)
            .await
            .is_err()
        {
            m3.projection_degraded
                .store(true, std::sync::atomic::Ordering::Release);
            m3.reconcile_notify.notify_one();
        }
        return replay_recorded(status, body);
    }
    let (operation_id, resumed) = match claim {
        ClaimResult::New(operation) => (operation, false),
        ClaimResult::Resume(operation) => (operation, true),
        ClaimResult::Replay { .. } => unreachable!(),
    };

    // A retry may already own irreversible effects. Admission failures on this
    // invocation must leave that operation resumable, including from a different
    // authenticated session or while a deployment holds an application guard.
    let operation_row = match sqlx::query(
        "SELECT plan_hash,plan_json FROM storage_cleanup_operations WHERE operation_id=?",
    )
    .bind(operation_id.to_string())
    .fetch_optional(m3.database.pool())
    .await
    {
        Ok(row) => row,
        Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
    };
    let published = operation_row.is_some();

    let preview_row = match sqlx::query("SELECT session_id,cleanup_kind,facts_hash,preview_json,expires_at,consumed_at FROM storage_cleanup_previews WHERE token_hmac=?")
        .bind(&token_hmac)
        .fetch_optional(m3.database.pool())
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            if published {
                return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
            }
            return finish_error(
                m3,
                APPLY_ROUTE,
                raw_key,
                "CLEANUP_PREVIEW_INVALID",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
    };
    let preview_session: String = preview_row.get(0);
    let cleanup_kind: String = preview_row.get(1);
    let preview_hash: Vec<u8> = preview_row.get(2);
    let preview_json: String = preview_row.get(3);
    let expires_at = match parse_time(preview_row.get(4)) {
        Ok(expires_at) => expires_at,
        Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
    };
    let consumed_at: Option<String> = preview_row.get(5);
    let preview_plan: CleanupPlan = match serde_json::from_str(&preview_json) {
        Ok(plan) if plan_hash(&preview_json) == preview_hash => plan,
        _ => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
    };
    if preview_session != authenticated.session.id
        || cleanup_kind != "artifacts"
        || !payload.acknowledge_rollback_loss
    {
        if published {
            return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
        }
        return finish_error(
            m3,
            APPLY_ROUTE,
            raw_key,
            "CLEANUP_PREVIEW_INVALID",
            StatusCode::CONFLICT,
            request_id,
        )
        .await;
    }

    let _catalog = m3.coordinator.catalog_lock().await;
    let app_ids: BTreeSet<_> = preview_plan
        .candidates
        .iter()
        .filter_map(|candidate| candidate.artifact.app_id())
        .collect();
    let mut app_guards = Vec::with_capacity(app_ids.len());
    for app_id in app_ids {
        match m3.coordinator.try_app(app_id) {
            Ok(guard) => app_guards.push(guard),
            Err(_) => {
                if published {
                    return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
                }
                return finish_error(
                    m3,
                    APPLY_ROUTE,
                    raw_key,
                    "APP_BUSY",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
        }
    }

    let plan = if let Some(row) = operation_row {
        if !resumed || consumed_at.is_none() {
            return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
        }
        let stored_hash: Vec<u8> = row.get(0);
        let stored_json: String = row.get(1);
        if stored_hash != preview_hash || stored_json != preview_json {
            return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
        }
        preview_plan
    } else {
        if consumed_at.is_some() || expires_at <= OffsetDateTime::now_utc() {
            let code = if consumed_at.is_some() {
                "CLEANUP_PREVIEW_INVALID"
            } else {
                "CLEANUP_PREVIEW_EXPIRED"
            };
            return finish_error(
                m3,
                APPLY_ROUTE,
                raw_key,
                code,
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        let current = match build_plan(&m3.store, &m3.database).await {
            Ok(plan) => plan,
            Err(CleanupError::InventoryIncomplete | CleanupError::RecordInvalid) => {
                return finish_error(
                    m3,
                    APPLY_ROUTE,
                    raw_key,
                    "CLEANUP_INVENTORY_INCOMPLETE",
                    StatusCode::CONFLICT,
                    request_id,
                )
                .await;
            }
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        };
        let current_json = match canonical_plan_json(&current) {
            Ok(json) => json,
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        };
        if current_json != preview_json {
            return finish_error(
                m3,
                APPLY_ROUTE,
                raw_key,
                "CLEANUP_PREVIEW_STALE",
                StatusCode::CONFLICT,
                request_id,
            )
            .await;
        }
        if publish_plan(
            m3,
            operation_id,
            &token_hmac,
            &preview_hash,
            &preview_json,
            &current,
            request_id,
        )
        .await
        .is_err()
        {
            return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
        }
        current
    };

    let eligible = if published {
        match crate::storage_cleanup::resume_candidates(&m3.store, &m3.database, operation_id).await
        {
            Ok(candidates) => Some(candidates),
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        }
    } else {
        None
    };
    let artifacts: Vec<_> = plan
        .candidates
        .iter()
        .map(|candidate| candidate.artifact.clone())
        .collect();
    if m3
        .store
        .prepare_cleanup_tombstone(operation_id, &preview_hash, &artifacts)
        .is_err()
    {
        return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
    }
    if sqlx::query("UPDATE storage_cleanup_operations SET status='running' WHERE operation_id=? AND status='planned'")
        .bind(operation_id.to_string())
        .execute(m3.database.pool())
        .await
        .is_err()
    {
        return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
    }

    let mut failed_revisions = HashSet::new();
    for (ordinal, candidate) in plan.candidates.iter().enumerate() {
        let stored_status: String = match sqlx::query_scalar(
            "SELECT status FROM storage_cleanup_items WHERE operation_id=? AND ordinal=?",
        )
        .bind(operation_id.to_string())
        .bind(ordinal as i64)
        .fetch_one(m3.database.pool())
        .await
        {
            Ok(status) => status,
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        };
        if stored_status == "detached" || stored_status == "failed" {
            if stored_status == "failed"
                && let CleanupArtifact::Release {
                    app_id,
                    config_revision_id,
                    ..
                } = candidate.artifact
            {
                failed_revisions.insert((app_id, config_revision_id));
            }
            continue;
        }
        // A canonical artifact may have gained a reference while this operation
        // was interrupted. Retain it in this exact plan instead of detaching it.
        // A rename already completed before a failed progress write is resumed
        // separately so its directory durability barriers are still repeated.
        let newly_protected = if let Some(eligible) = &eligible {
            if eligible.contains(&candidate.artifact) {
                false
            } else {
                match m3.store.cleanup_artifact_is_detached(
                    operation_id,
                    ordinal,
                    &candidate.artifact,
                ) {
                    Ok(detached) => !detached,
                    Err(_) => {
                        return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
                    }
                }
            }
        } else {
            false
        };
        if newly_protected {
            if let CleanupArtifact::Release {
                app_id,
                config_revision_id,
                ..
            } = candidate.artifact
            {
                failed_revisions.insert((app_id, config_revision_id));
            }
            if record_item_failure(m3, operation_id, ordinal, "CLEANUP_ITEM_PROTECTED")
                .await
                .is_err()
            {
                return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
            }
            continue;
        }
        if let CleanupArtifact::ConfigRevision {
            app_id,
            revision_id,
        } = candidate.artifact
            && failed_revisions.contains(&(app_id, revision_id))
        {
            if record_item_failure(m3, operation_id, ordinal, "RELEASE_RETAINED")
                .await
                .is_err()
            {
                return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
            }
            continue;
        }
        match m3
            .store
            .detach_cleanup_artifact(operation_id, ordinal, &candidate.artifact)
        {
            Ok(DetachResult::Detached | DetachResult::AlreadyDetached) => {
                if record_detached(m3, operation_id, ordinal, candidate)
                    .await
                    .is_err()
                {
                    return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
                }
            }
            Ok(DetachResult::ConfirmedMissing) => {
                return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
            }
            Ok(DetachResult::ConfirmedRetained) => {
                if let CleanupArtifact::Release {
                    app_id,
                    config_revision_id,
                    ..
                } = candidate.artifact
                {
                    failed_revisions.insert((app_id, config_revision_id));
                }
                if record_item_failure(m3, operation_id, ordinal, "CLEANUP_ITEM_RETAINED")
                    .await
                    .is_err()
                {
                    return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
                }
            }
            Err(crate::app_store::StoreError::ReleaseConflict) => {
                if let CleanupArtifact::Release {
                    app_id,
                    config_revision_id,
                    ..
                } = candidate.artifact
                {
                    failed_revisions.insert((app_id, config_revision_id));
                }
                if record_item_failure(m3, operation_id, ordinal, "CLEANUP_ITEM_RETAINED")
                    .await
                    .is_err()
                {
                    return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
                }
            }
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        }
    }
    let items =
        match crate::storage_cleanup::exact_terminal_items(&m3.database, operation_id, &plan).await
        {
            Ok(items) => items,
            Err(_) => return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await,
        };
    let has_failures = items.iter().any(|item| item["status"] == "retained");
    let status = if has_failures {
        "completed_with_failures"
    } else {
        "completed"
    };
    let now = format_time(OffsetDateTime::now_utc()).map_err(|_| ApiError::internal(request_id))?;
    if sqlx::query("UPDATE storage_cleanup_operations SET status=?,completed_at=? WHERE operation_id=? AND status IN ('planned','running')")
        .bind(status)
        .bind(&now)
        .bind(operation_id.to_string())
        .execute(m3.database.pool())
        .await
        .is_err()
    {
        return interrupt_internal(m3, APPLY_ROUTE, raw_key, request_id).await;
    }
    let response_body = ApplyResponse {
        operation_id,
        plan_hash: crate::app_store::cleanup::encode_hex(&preview_hash),
        status,
        items,
        idempotency_replayed: false,
    };
    let response = finish_json(
        m3,
        APPLY_ROUTE,
        raw_key,
        StatusCode::OK,
        &response_body,
        request_id,
    )
    .await?;
    if crate::storage_cleanup::finalize_succeeded(&m3.store, &m3.database)
        .await
        .is_err()
    {
        m3.projection_degraded
            .store(true, std::sync::atomic::Ordering::Release);
        m3.reconcile_notify.notify_one();
    }
    drop(app_guards);
    Ok(response)
}

async fn publish_plan(
    m3: &super::mutations::M3Services,
    operation_id: Uuid,
    token_hmac: &[u8],
    plan_hash: &[u8],
    plan_json: &str,
    plan: &CleanupPlan,
    request_id: RequestId,
) -> Result<(), sqlx::Error> {
    let now = format_time(OffsetDateTime::now_utc())
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let mut tx = m3.database.pool().begin().await?;
    let consumed = sqlx::query("UPDATE storage_cleanup_previews SET consumed_at=? WHERE token_hmac=? AND consumed_at IS NULL")
        .bind(&now)
        .bind(token_hmac)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if consumed != 1 {
        return Err(sqlx::Error::Protocol(
            "cleanup preview was consumed concurrently".into(),
        ));
    }
    sqlx::query("INSERT INTO storage_cleanup_operations (operation_id,cleanup_kind,plan_hash,plan_json,status,created_at) VALUES (?,'artifacts',?,?,'planned',?)")
        .bind(operation_id.to_string())
        .bind(plan_hash)
        .bind(plan_json)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    for (ordinal, candidate) in plan.candidates.iter().enumerate() {
        let config_revision = match candidate.artifact {
            CleanupArtifact::Release {
                config_revision_id, ..
            } => Some(config_revision_id.to_string()),
            CleanupArtifact::ConfigRevision { revision_id, .. } => Some(revision_id.to_string()),
            CleanupArtifact::Temporary { .. } => None,
        };
        sqlx::query("INSERT INTO storage_cleanup_items (operation_id,ordinal,app_id,artifact_kind,artifact_id,config_revision_id,status) VALUES (?,?,?,?,?,?,'planned')")
            .bind(operation_id.to_string())
            .bind(ordinal as i64)
            .bind(candidate.artifact.app_id().map(|id| id.to_string()))
            .bind(candidate.artifact.kind_name())
            .bind(candidate.artifact.public_id())
            .bind(config_revision)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('admin',?,'storage_cleanup_apply','cleanup',?,'planned',?,?)")
        .bind(request_id.0.to_string())
        .bind(operation_id.to_string())
        .bind(serde_json::json!({"kind":"artifacts","items":plan.candidates.len()}).to_string())
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

async fn record_detached(
    m3: &super::mutations::M3Services,
    operation_id: Uuid,
    ordinal: usize,
    candidate: &CleanupCandidate,
) -> Result<(), sqlx::Error> {
    let mut tx = m3.database.pool().begin().await?;
    let changed = sqlx::query("UPDATE storage_cleanup_items SET status='detached',error_code=NULL WHERE operation_id=? AND ordinal=? AND status='planned'")
        .bind(operation_id.to_string())
        .bind(ordinal as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed != 1 {
        return Err(sqlx::Error::Protocol("cleanup item state changed".into()));
    }
    if let (
        CleanupArtifact::Release {
            app_id, release_id, ..
        },
        Some(record),
    ) = (&candidate.artifact, &candidate.release_record)
    {
        sqlx::query("INSERT INTO cleaned_releases (app_id,release_id,cleanup_operation_id,removed_at,manifest_digest,local_image_id,platform_os,platform_architecture,platform_variant) VALUES (?,?,?,?,?,?,?,?,?)")
            .bind(app_id.to_string())
            .bind(release_id.to_string())
            .bind(operation_id.to_string())
            .bind(
                format_time(OffsetDateTime::now_utc())
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?,
            )
            .bind(&record.manifest_digest)
            .bind(&record.local_image_id)
            .bind(&record.platform_os)
            .bind(&record.platform_architecture)
            .bind(&record.platform_variant)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

async fn record_item_failure(
    m3: &super::mutations::M3Services,
    operation_id: Uuid,
    ordinal: usize,
    code: &'static str,
) -> Result<(), sqlx::Error> {
    let changed = sqlx::query("UPDATE storage_cleanup_items SET status='failed',error_code=? WHERE operation_id=? AND ordinal=? AND status='planned'")
        .bind(code)
        .bind(operation_id.to_string())
        .bind(ordinal as i64)
        .execute(m3.database.pool())
        .await?
        .rows_affected();
    if changed == 1 {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol("cleanup item state changed".into()))
    }
}

fn preview_candidate(ordinal: usize, candidate: &CleanupCandidate) -> PreviewCandidateResponse {
    PreviewCandidateResponse {
        app_id: candidate.artifact.app_id(),
        artifact_kind: candidate.artifact.kind_name(),
        artifact_id: public_artifact_id(ordinal, &candidate.artifact),
        estimated_logical_bytes: candidate.estimated_logical_bytes,
        release_created_at: candidate.release_created_at,
    }
}

fn protected_summary(plan: &CleanupPlan) -> Vec<ProtectedSummaryResponse> {
    let mut counts = std::collections::BTreeMap::new();
    for item in &plan.protected {
        *counts.entry(item.reason).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .map(|(reason, count)| ProtectedSummaryResponse { reason, count })
        .collect()
}

fn public_artifact_id(ordinal: usize, artifact: &CleanupArtifact) -> String {
    match artifact {
        CleanupArtifact::Temporary { .. } => format!("temporary-{}", ordinal + 1),
        _ => artifact.public_id(),
    }
}

fn cleanup_error(error: CleanupError, request_id: RequestId) -> ApiError {
    match error {
        CleanupError::InventoryIncomplete | CleanupError::RecordInvalid => ApiError::new(
            StatusCode::CONFLICT,
            "CLEANUP_INVENTORY_INCOMPLETE",
            "The cleanup inventory is incomplete",
            request_id,
        ),
        CleanupError::Store(_) | CleanupError::Database(_) => ApiError::internal(request_id),
    }
}
