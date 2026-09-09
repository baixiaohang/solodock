use super::{
    AppState,
    auth::{Authenticated, MutationAuthenticated},
    mutations::{finish_error, idempotency_key, interrupt_internal, replay_recorded, services},
};
use crate::{
    error::{ApiError, RequestId},
    mutation::ClaimResult,
    retention::{RetentionPolicy, load},
};
use axum::{
    Json,
    extract::{Extension, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

pub async fn get(
    State(state): State<AppState>,
    Path(app): Path<Uuid>,
    Extension(id): Extension<RequestId>,
    _: Authenticated,
) -> Result<Json<RetentionPolicy>, ApiError> {
    let m3 = services(&state, id)?;
    let _catalog = m3.coordinator.catalog_lock().await;
    m3.store
        .read_metadata(app)
        .map_err(|_| ApiError::app_not_found(id))?;
    Ok(Json(
        load(&m3.database, app)
            .await
            .map_err(|_| ApiError::internal(id))?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    expected_revision: Uuid,
    enabled: bool,
    keep_versions: u16,
}
pub async fn update(
    State(state): State<AppState>,
    Path(app): Path<Uuid>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    _: MutationAuthenticated,
    payload: Result<Json<Update>, JsonRejection>,
) -> Result<Response, ApiError> {
    let m3 = services(&state, id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(id))?;
    if !(1..=100).contains(&payload.keep_versions) {
        return Err(ApiError::validation(id));
    }
    let route = format!("/api/v1/apps/{app}/retention");
    let key = idempotency_key(&headers).map_err(|e| ApiError::idempotency(e, id))?;
    let fingerprint=m3.idempotency.fingerprint(serde_json::json!({"method":"PUT","route":route,"expected_revision":payload.expected_revision,"enabled":payload.enabled,"keep_versions":payload.keep_versions}).to_string().as_bytes());
    let claim = m3
        .idempotency
        .claim(&route, key, &fingerprint, id.0)
        .await
        .map_err(|e| ApiError::idempotency(e, id))?;
    if let ClaimResult::Replay { status, body, .. } = claim {
        return replay_recorded(status, body);
    }
    let _catalog = m3.coordinator.catalog_lock().await;
    if m3.store.read_metadata(app).is_err() {
        return finish_error(m3, &route, key, "APP_NOT_FOUND", StatusCode::NOT_FOUND, id).await;
    }
    let _app = match m3.coordinator.try_app(app) {
        Ok(guard) => guard,
        Err(_) => return finish_error(m3, &route, key, "APP_BUSY", StatusCode::CONFLICT, id).await,
    };
    let current = load(&m3.database, app)
        .await
        .map_err(|_| ApiError::internal(id))?;
    let operation = match claim {
        ClaimResult::New(op) | ClaimResult::Resume(op) => op,
        _ => unreachable!(),
    };
    if current.revision != payload.expected_revision {
        return finish_error(
            m3,
            &route,
            key,
            "RETENTION_REVISION_STALE",
            StatusCode::CONFLICT,
            id,
        )
        .await;
    }
    let at = crate::db::format_time(time::OffsetDateTime::now_utc())
        .map_err(|_| ApiError::internal(id))?;
    let policy = RetentionPolicy {
        enabled: payload.enabled,
        keep_versions: payload.keep_versions,
        revision: operation,
        last_checked_at: current.last_checked_at,
        last_status: Some("pending".into()),
        last_error_code: None,
    };
    let response_body = serde_json::to_string(&policy).map_err(|_| ApiError::internal(id))?;
    // Policy, audit and the exact replay response have one visibility commit.
    // A response failure can never turn an applied authorization into a 4xx.
    let persist=async {
        let mut tx=m3.database.pool().begin().await?;
        sqlx::query("INSERT INTO app_retention(app_id,enabled,keep_versions,revision,updated_at,last_status) VALUES(?,?,?,?,?,'pending') ON CONFLICT(app_id) DO UPDATE SET enabled=excluded.enabled,keep_versions=excluded.keep_versions,revision=excluded.revision,updated_at=excluded.updated_at,last_status='pending',last_error_code=NULL").bind(app.to_string()).bind(payload.enabled).bind(i64::from(payload.keep_versions)).bind(operation.to_string()).bind(&at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO audit_events(actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES('admin',?,'retention_policy_update','app',?,'succeeded',?,?)").bind(id.0.to_string()).bind(app.to_string()).bind(serde_json::json!({"enabled":payload.enabled,"keep_versions":payload.keep_versions,"revision":operation}).to_string()).bind(&at).execute(&mut *tx).await?;
        let changed = sqlx::query("UPDATE idempotency_records SET status='succeeded',response_status=200,response_body=?,error_code=NULL,updated_at=? WHERE actor='admin' AND route=? AND operation_id=? AND request_hmac=? AND status='pending'").bind(&response_body).bind(&at).bind(&route).bind(operation.to_string()).bind(&fingerprint).execute(&mut *tx).await?.rows_affected();
        if changed != 1 { return Err(sqlx::Error::Protocol("retention claim changed".into())); }
        tx.commit().await
    }.await;
    if persist.is_err() {
        return interrupt_internal(m3, &route, key, id).await;
    }
    state.retention_notify.notify_one();
    Ok((StatusCode::OK, Json(policy)).into_response())
}
