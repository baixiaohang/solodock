//! Durable completed-unregistration facts, independent of deployment history.
use std::collections::HashSet;

use sqlx::Row;
use uuid::Uuid;

use crate::{app_store::AppStore, db::Database, mutation::IdempotencyError};

/// Preserve recognizable pre-upgrade deletion proofs before their normal GC.
/// Missing or malformed historical responses never establish a lifecycle fact.
pub(crate) async fn backfill(
    database: &Database,
    store: &AppStore,
) -> Result<(), IdempotencyError> {
    let tombstoned: HashSet<_> = store.tombstones()?.into_iter().map(|v| v.0).collect();
    for row in sqlx::query("SELECT route,operation_id,response_body FROM idempotency_records WHERE actor='admin' AND status='succeeded' AND response_status=200 AND route LIKE '/api/v1/apps/%'")
        .fetch_all(database.pool()).await? {
        let route: &str = row.get("route");
        let Some(app) = route.strip_prefix("/api/v1/apps/") else { continue };
        let Ok(app_id) = canonical_uuid(app) else { continue };
        let body: Option<String> = row.get("response_body");
        let Some(body) = body else { continue };
        if !valid_response(&body, app_id) || tombstoned.contains(&app_id) { continue; }
        // Outstanding tombstones must pass the existing publication and
        // operational-cleanup gates before their shared finalizer runs.
        preserve_proof(database, store, app_id, canonical_uuid(row.get("operation_id"))?).await?;
        mark_finalized(database, store, app_id, canonical_uuid(row.get("operation_id"))?).await?;
    }
    Ok(())
}

/// Check the exact successful response again inside the receipt transaction.
/// The caller retains any tombstone until this transaction commits.
pub(crate) async fn preserve_proof(
    database: &Database,
    store: &AppStore,
    app_id: Uuid,
    operation_id: Uuid,
) -> Result<(), IdempotencyError> {
    store.sync_unregistered_app(app_id)?;
    let route = format!("/api/v1/apps/{app_id}");
    let mut tx = database.pool().begin().await?;
    let proof = sqlx::query("SELECT status,response_status,response_body,updated_at FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
        .bind(&route).bind(operation_id.to_string()).fetch_optional(&mut *tx).await?
        .ok_or(IdempotencyError::RecordInvalid)?;
    let body: Option<String> = proof.get("response_body");
    if proof.get::<&str, _>("status") != "succeeded"
        || proof.get::<Option<i64>, _>("response_status") != Some(200)
        || !body
            .as_deref()
            .is_some_and(|body| valid_response(body, app_id))
    {
        return Err(IdempotencyError::RecordInvalid);
    }
    let completed_at: &str = proof.get("updated_at");
    crate::db::parse_time(completed_at)?;
    sqlx::query("INSERT INTO app_unregistrations (app_id,operation_id,source,completed_at) VALUES (?,?,'deletion',?) ON CONFLICT(app_id) DO NOTHING")
        .bind(app_id.to_string()).bind(operation_id.to_string()).bind(completed_at).execute(&mut *tx).await?;
    let row = sqlx::query("SELECT operation_id,source FROM app_unregistrations WHERE app_id=?")
        .bind(app_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    if row.get::<String, _>(0) != operation_id.to_string() || row.get::<String, _>(1) != "deletion"
    {
        return Err(IdempotencyError::RecordInvalid);
    }
    tx.commit().await?;
    Ok(())
}

fn valid_response(body: &str, app_id: Uuid) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| {
            value.get("unregistered").and_then(|v| v.as_bool()) == Some(true)
                && value.get("app_id").and_then(|v| v.as_str()) == Some(app_id.to_string().as_str())
        })
}

pub(crate) async fn mark_finalized(
    database: &Database,
    store: &AppStore,
    app_id: Uuid,
    operation_id: Uuid,
) -> Result<(), IdempotencyError> {
    store.sync_unregistered_app(app_id)?;
    if store.tombstones()?.iter().any(|(app, _)| *app == app_id) {
        return Err(IdempotencyError::RecordInvalid);
    }
    let changed = sqlx::query("UPDATE app_unregistrations SET finalized=1 WHERE app_id=? AND operation_id=? AND source='deletion'")
        .bind(app_id.to_string()).bind(operation_id.to_string()).execute(database.pool()).await?.rows_affected();
    if changed != 1 {
        return Err(IdempotencyError::RecordInvalid);
    }
    Ok(())
}

pub(crate) async fn pending(database: &Database) -> Result<Vec<(Uuid, Uuid)>, IdempotencyError> {
    let mut pending = Vec::new();
    for row in
        sqlx::query("SELECT app_id,operation_id,source FROM app_unregistrations WHERE finalized=0")
            .fetch_all(database.pool())
            .await?
    {
        let app = canonical_uuid(row.get("app_id"))?;
        let op = canonical_uuid(row.get("operation_id"))?;
        if row.get::<&str, _>("source") != "deletion" {
            return Err(IdempotencyError::RecordInvalid);
        }
        let body: Option<String> = sqlx::query_scalar("SELECT response_body FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=? AND status='succeeded' AND response_status=200")
            .bind(format!("/api/v1/apps/{app}")).bind(op.to_string()).fetch_optional(database.pool()).await?.flatten();
        if !body
            .as_deref()
            .is_some_and(|body| valid_response(body, app))
        {
            return Err(IdempotencyError::RecordInvalid);
        }
        pending.push((app, op));
    }
    Ok(pending)
}

/// A receipt never hides a reappeared app or a deletion still owning files.
pub(crate) async fn completed_apps(
    database: &Database,
    store: &AppStore,
) -> Result<HashSet<Uuid>, IdempotencyError> {
    let tombstones = store.tombstones()?;
    pending(database).await?;
    let mut completed = HashSet::new();
    for row in sqlx::query(
        "SELECT app_id,operation_id,source,completed_at,finalized FROM app_unregistrations",
    )
    .fetch_all(database.pool())
    .await?
    {
        let app_id = canonical_uuid(row.get("app_id"))?;
        let operation_id = canonical_uuid(row.get("operation_id"))?;
        let source: &str = row.get("source");
        if !matches!(source, "deletion" | "operator_repair") {
            return Err(IdempotencyError::RecordInvalid);
        }
        if source == "operator_repair" {
            let audited: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE actor='admin' AND action='app_unregistration_repair' AND target_type='app' AND target_id=? AND request_id=? AND result='succeeded'")
                .bind(app_id.to_string()).bind(row.get::<&str, _>("operation_id"))
                .fetch_one(database.pool()).await?;
            if audited != 1 {
                return Err(IdempotencyError::RecordInvalid);
            }
        }
        crate::db::parse_time(row.get("completed_at"))?;
        match std::fs::symlink_metadata(store.app_directory(app_id)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(IdempotencyError::RecordInvalid),
        }
        if tombstones
            .iter()
            .any(|(app, operation)| *app == app_id && *operation != operation_id)
        {
            return Err(IdempotencyError::RecordInvalid);
        }
        if row.get::<bool, _>("finalized") && !tombstones.iter().any(|(app, _)| *app == app_id) {
            completed.insert(app_id);
        }
    }
    Ok(completed)
}

fn canonical_uuid(value: &str) -> Result<Uuid, IdempotencyError> {
    value
        .parse::<Uuid>()
        .ok()
        .filter(|id| id.to_string() == value)
        .ok_or(IdempotencyError::RecordInvalid)
}
