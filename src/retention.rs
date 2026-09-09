//! Application retention policy and bounded, restartable cleanup coordination.
use crate::{
    api::{AppState, mutations::M3Services},
    db::{Database, format_time},
    image_cleanup::ImageCandidate,
    storage_cleanup::{CleanupError, CleanupPlan, plan_hash},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{BTreeMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicy {
    pub enabled: bool,
    pub keep_versions: u16,
    pub revision: Uuid,
    pub last_checked_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error_code: Option<String>,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_versions: 3,
            revision: Uuid::nil(),
            last_checked_at: None,
            last_status: None,
            last_error_code: None,
        }
    }
}
pub async fn load(db: &Database, app: Uuid) -> Result<RetentionPolicy, CleanupError> {
    let row = sqlx::query("SELECT * FROM app_retention WHERE app_id=?")
        .bind(app.to_string())
        .fetch_optional(db.pool())
        .await?;
    row.map(|row| {
        let keep: i64 = row.get("keep_versions");
        if !(1..=100).contains(&keep) {
            return Err(CleanupError::RecordInvalid);
        }
        Ok(RetentionPolicy {
            enabled: row.get("enabled"),
            keep_versions: keep as u16,
            revision: Uuid::parse_str(row.get("revision"))
                .map_err(|_| CleanupError::RecordInvalid)?,
            last_checked_at: row.get("last_checked_at"),
            last_status: row.get("last_status"),
            last_error_code: row.get("last_error_code"),
        })
    })
    .unwrap_or(Ok(RetentionPolicy::default()))
}
pub(crate) async fn enabled_policies(
    db: &Database,
) -> Result<BTreeMap<Uuid, RetentionPolicy>, CleanupError> {
    let mut policies = BTreeMap::new();
    for id in sqlx::query_scalar::<_, String>(
        "SELECT app_id FROM app_retention WHERE enabled=1 ORDER BY app_id",
    )
    .fetch_all(db.pool())
    .await?
    {
        let app = Uuid::parse_str(&id).map_err(|_| CleanupError::RecordInvalid)?;
        policies.insert(app, load(db, app).await?);
    }
    Ok(policies)
}
pub(crate) fn retained_releases(
    active: Option<Uuid>,
    keep: usize,
    successes: impl IntoIterator<Item = Uuid>,
) -> HashSet<Uuid> {
    let mut retained: HashSet<_> = active.into_iter().collect();
    for release in successes {
        if retained.len() >= keep {
            break;
        }
        retained.insert(release);
    }
    retained
}

pub(crate) struct Authorization {
    pub app: Uuid,
    pub result: Option<serde_json::Value>,
}
/// A policy authorization is durable and binds the exact plan, app and opt-in
/// policy snapshot. It never impersonates an administrator confirmation token.
pub(crate) async fn authorization(
    db: &Database,
    operation: Uuid,
    kind: &str,
) -> Result<Option<Authorization>, CleanupError> {
    let Some(row) =
        sqlx::query("SELECT * FROM automatic_cleanup_authorizations WHERE operation_id=?")
            .bind(operation.to_string())
            .fetch_optional(db.pool())
            .await?
    else {
        return Ok(None);
    };
    let app = Uuid::parse_str(row.get("app_id")).map_err(|_| CleanupError::RecordInvalid)?;
    let policy: RetentionPolicy =
        serde_json::from_str(row.get("policy_json")).map_err(|_| CleanupError::RecordInvalid)?;
    if row.get::<&str, _>("cleanup_kind") != kind
        || !policy.enabled
        || !(1..=100).contains(&policy.keep_versions)
        || policy.revision.is_nil()
    {
        return Err(CleanupError::RecordInvalid);
    }
    let query = match kind {
        "artifacts" => {
            "SELECT plan_json,plan_hash FROM storage_cleanup_operations WHERE operation_id=?"
        }
        "images" => "SELECT plan_json,plan_hash FROM image_cleanup_operations WHERE operation_id=?",
        _ => return Err(CleanupError::RecordInvalid),
    };
    let plan = sqlx::query(query)
        .bind(operation.to_string())
        .fetch_one(db.pool())
        .await?;
    let json: &str = plan.get("plan_json");
    let hash = plan_hash(json);
    if hash != row.get::<Vec<u8>, _>("plan_hash") || hash != plan.get::<Vec<u8>, _>("plan_hash") {
        return Err(CleanupError::RecordInvalid);
    }
    if kind == "artifacts" {
        let plan: CleanupPlan =
            serde_json::from_str(json).map_err(|_| CleanupError::RecordInvalid)?;
        if plan.candidates.is_empty()
            || plan.candidates.len() > 100
            || plan.candidates.iter().any(|item| {
                item.artifact.app_id() != Some(app)
                    || matches!(
                        item.artifact,
                        crate::app_store::cleanup::CleanupArtifact::Temporary { .. }
                    )
            })
        {
            return Err(CleanupError::RecordInvalid);
        }
    } else {
        let plan: Vec<ImageCandidate> =
            serde_json::from_str(json).map_err(|_| CleanupError::RecordInvalid)?;
        for item in plan {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cleaned_releases WHERE app_id=? AND manifest_digest=? AND local_image_id=? AND platform_os=? AND platform_architecture=? AND platform_variant IS ?)").bind(app.to_string()).bind(&item.identity.manifest_digest).bind(&item.identity.local_image_id).bind(&item.identity.platform_os).bind(&item.identity.platform_architecture).bind(&item.identity.platform_variant).fetch_one(db.pool()).await?;
            if !exists {
                return Err(CleanupError::RecordInvalid);
            }
        }
    }
    let result = row
        .get::<Option<&str>, _>("result_json")
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| CleanupError::RecordInvalid)?;
    Ok(Some(Authorization { app, result }))
}
fn now() -> Result<String, CleanupError> {
    format_time(OffsetDateTime::now_utc()).map_err(|_| CleanupError::RecordInvalid)
}
async fn publish(
    m3: &M3Services,
    app: Uuid,
    policy: &RetentionPolicy,
    kind: &str,
    json: &str,
) -> Result<Uuid, CleanupError> {
    let op = Uuid::new_v4();
    let hash = plan_hash(json);
    let at = now()?;
    let mut tx = m3.database.pool().begin().await?;
    sqlx::query("INSERT INTO automatic_cleanup_authorizations(operation_id,app_id,policy_json,cleanup_kind,plan_hash,created_at) VALUES(?,?,?,?,?,?)").bind(op.to_string()).bind(app.to_string()).bind(serde_json::to_string(policy).map_err(|_|CleanupError::RecordInvalid)?).bind(kind).bind(&hash).bind(&at).execute(&mut *tx).await?;
    if kind == "artifacts" {
        let plan: CleanupPlan =
            serde_json::from_str(json).map_err(|_| CleanupError::RecordInvalid)?;
        sqlx::query("INSERT INTO storage_cleanup_operations(operation_id,cleanup_kind,plan_json,plan_hash,status,created_at) VALUES(?,'artifacts',?,?,'planned',?)").bind(op.to_string()).bind(json).bind(&hash).bind(&at).execute(&mut *tx).await?;
        for (ordinal, item) in plan.candidates.iter().enumerate() {
            use crate::app_store::cleanup::CleanupArtifact;
            let revision = match item.artifact {
                CleanupArtifact::Release {
                    config_revision_id, ..
                } => config_revision_id,
                CleanupArtifact::ConfigRevision { revision_id, .. } => revision_id,
                _ => return Err(CleanupError::RecordInvalid),
            };
            sqlx::query("INSERT INTO storage_cleanup_items(operation_id,ordinal,app_id,artifact_kind,artifact_id,config_revision_id,status) VALUES(?,?,?,?,?,?,'planned')").bind(op.to_string()).bind(ordinal as i64).bind(app.to_string()).bind(item.artifact.kind_name()).bind(item.artifact.public_id()).bind(revision.to_string()).execute(&mut *tx).await?;
        }
    } else {
        let plan: Vec<ImageCandidate> =
            serde_json::from_str(json).map_err(|_| CleanupError::RecordInvalid)?;
        sqlx::query("INSERT INTO image_cleanup_operations(operation_id,token_hmac,plan_json,plan_hash,created_at) VALUES(?,NULL,?,?,?)").bind(op.to_string()).bind(json).bind(&hash).bind(&at).execute(&mut *tx).await?;
        for (ordinal, item) in plan.iter().enumerate() {
            sqlx::query("INSERT INTO image_cleanup_items(operation_id,ordinal,image_id,status) VALUES(?,?,?,'planned')").bind(op.to_string()).bind(ordinal as i64).bind(item.image_id.as_str()).execute(&mut *tx).await?;
        }
    }
    sqlx::query("INSERT INTO audit_events(actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES('system',?,'automatic_cleanup','app',?,'planned',?,?)").bind(op.to_string()).bind(app.to_string()).bind(serde_json::json!({"operation_id":op,"kind":kind,"policy_revision":policy.revision,"keep_versions":policy.keep_versions}).to_string()).bind(at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(op)
}
async fn complete(db: &Database, op: Uuid, body: &serde_json::Value) -> Result<(), CleanupError> {
    sqlx::query("UPDATE automatic_cleanup_authorizations SET result_json=? WHERE operation_id=? AND result_json IS NULL").bind(body.to_string()).bind(op.to_string()).execute(db.pool()).await?;
    Ok(())
}
async fn pending(db: &Database, app: Uuid, kind: &str) -> Result<Option<Uuid>, CleanupError> {
    sqlx::query_scalar::<_,String>("SELECT operation_id FROM automatic_cleanup_authorizations WHERE app_id=? AND cleanup_kind=? AND result_json IS NULL ORDER BY created_at,operation_id LIMIT 1").bind(app.to_string()).bind(kind).fetch_optional(db.pool()).await?.map(|id|Uuid::parse_str(&id).map_err(|_|CleanupError::RecordInvalid)).transpose()
}
async fn artifacts(state: &AppState, app: Uuid) -> Result<bool, CleanupError> {
    let m3 = state.m3.as_ref().ok_or(CleanupError::RecordInvalid)?;
    let _catalog = m3.coordinator.catalog_lock().await;
    let _app = m3
        .coordinator
        .try_app(app)
        .map_err(|_| CleanupError::Busy)?;
    crate::storage_cleanup::finalize_succeeded(&m3.store, &m3.database).await?;
    let policy = load(&m3.database, app).await?;
    let resumed = pending(&m3.database, app, "artifacts").await?;
    if resumed.is_none() && (!policy.enabled || m3.store.read_metadata(app).is_err()) {
        return Ok(false);
    }
    let current =
        crate::storage_cleanup::automatic_plan(&m3.store, &m3.database, app, resumed).await?;
    let (op, plan) = if let Some(op) = resumed {
        let auth = authorization(&m3.database, op, "artifacts")
            .await?
            .ok_or(CleanupError::RecordInvalid)?;
        if auth.app != app {
            return Err(CleanupError::RecordInvalid);
        }
        let json: String = sqlx::query_scalar(
            "SELECT plan_json FROM storage_cleanup_operations WHERE operation_id=?",
        )
        .bind(op.to_string())
        .fetch_one(m3.database.pool())
        .await?;
        (
            op,
            serde_json::from_str::<CleanupPlan>(&json).map_err(|_| CleanupError::RecordInvalid)?,
        )
    } else {
        if current.candidates.is_empty() {
            return Ok(false);
        }
        let json = crate::storage_cleanup::canonical_plan_json(&current)?;
        (
            publish(m3, app, &policy, "artifacts", &json).await?,
            current.clone(),
        )
    };
    let hash = plan_hash(&crate::storage_cleanup::canonical_plan_json(&plan)?);
    let eligible = current
        .candidates
        .into_iter()
        .map(|item| item.artifact)
        .collect();
    let body =
        crate::cleanup_execution::execute_artifacts(m3, op, &plan, &hash, Some(&eligible)).await?;
    complete(
        &m3.database,
        op,
        &serde_json::to_value(&body).map_err(|_| CleanupError::RecordInvalid)?,
    )
    .await?;
    crate::storage_cleanup::finalize_succeeded(&m3.store, &m3.database).await?;
    Ok(body.status == "completed_with_failures")
}
async fn images(state: &AppState, app: Uuid) -> Result<bool, CleanupError> {
    let m3 = state.m3.as_ref().ok_or(CleanupError::RecordInvalid)?;
    let _catalog = m3.coordinator.catalog_lock().await;
    let mut guards = Vec::new();
    for id in crate::image_cleanup::current_app_ids(&m3.store, &m3.database).await? {
        guards.push(m3.coordinator.try_app(id).map_err(|_| CleanupError::Busy)?);
    }
    let _compose = m3
        .coordinator
        .try_compose()
        .map_err(|_| CleanupError::Busy)?;
    let policy = load(&m3.database, app).await?;
    let resumed = pending(&m3.database, app, "images").await?;
    if resumed.is_none() && (!policy.enabled || m3.store.read_metadata(app).is_err()) {
        return Ok(false);
    }
    if resumed.is_none() {
        let has_source: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cleaned_releases WHERE app_id=?)")
                .bind(app.to_string())
                .fetch_one(m3.database.pool())
                .await?;
        if !has_source {
            return Ok(false);
        }
    }
    let mut current = crate::image_cleanup::build_scoped_plan(
        &m3.store,
        &m3.database,
        state.image_cleanup.as_ref(),
        resumed,
        Some(app),
    )
    .await?;
    if !policy.enabled {
        current.candidates.clear();
    }
    let protected = current.protected_count > 0;
    let (op, plan) = if let Some(op) = resumed {
        let auth = authorization(&m3.database, op, "images")
            .await?
            .ok_or(CleanupError::RecordInvalid)?;
        if auth.app != app {
            return Err(CleanupError::RecordInvalid);
        }
        let json: String = sqlx::query_scalar(
            "SELECT plan_json FROM image_cleanup_operations WHERE operation_id=?",
        )
        .bind(op.to_string())
        .fetch_one(m3.database.pool())
        .await?;
        (
            op,
            serde_json::from_str::<Vec<ImageCandidate>>(&json)
                .map_err(|_| CleanupError::RecordInvalid)?,
        )
    } else {
        if current.candidates.is_empty() {
            return Ok(protected);
        }
        let json =
            serde_json::to_string(&current.candidates).map_err(|_| CleanupError::RecordInvalid)?;
        (
            publish(m3, app, &policy, "images", &json).await?,
            current.candidates.clone(),
        )
    };
    let hash = plan_hash(&serde_json::to_string(&plan).map_err(|_| CleanupError::RecordInvalid)?);
    let body = crate::cleanup_execution::execute_images(
        m3,
        state.image_cleanup.as_ref(),
        op,
        &plan,
        &current,
        &hash,
    )
    .await?;
    complete(&m3.database, op, &body).await?;
    Ok(protected || body["status"] == "completed_with_failures")
}
/// One bounded batch per application and phase. The periodic pass advances the
/// backlog from fresh facts, including after restart, without a retry hot loop.
pub async fn run_pass(state: &AppState) -> Result<(), CleanupError> {
    let m3 = state.m3.as_ref().ok_or(CleanupError::RecordInvalid)?;
    let apps: Vec<String> = sqlx::query_scalar("SELECT app_id FROM app_retention WHERE enabled=1 UNION SELECT app_id FROM automatic_cleanup_authorizations WHERE result_json IS NULL ORDER BY app_id").fetch_all(m3.database.pool()).await?;
    for id in apps {
        if state.shutdown.is_cancelled() {
            break;
        }
        let app = Uuid::parse_str(&id).map_err(|_| CleanupError::RecordInvalid)?;
        let (status, error) = match artifacts(state, app).await {
            Err(CleanupError::Busy) => ("pending", Some("APP_BUSY")),
            Err(_) => ("blocked", Some("ARTIFACT_CLEANUP_BLOCKED")),
            Ok(retained) => match images(state, app).await {
                Err(CleanupError::Busy) => ("pending", Some("APP_BUSY")),
                Err(_) => ("blocked", Some("IMAGE_CLEANUP_INVENTORY_INCOMPLETE")),
                Ok(images_retained) => {
                    if retained || images_retained {
                        ("partially_retained", None)
                    } else {
                        ("completed", None)
                    }
                }
            },
        };
        sqlx::query("UPDATE app_retention SET last_checked_at=?,last_status=?,last_error_code=? WHERE app_id=?").bind(now()?).bind(status).bind(error).bind(id).execute(m3.database.pool()).await?;
    }
    Ok(())
}
pub fn start(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { () = state.shutdown.cancelled() => break, _ = interval.tick() => {}, () = state.retention_notify.notified() => {} }
            if run_pass(&state).await.is_err() {
                tracing::warn!("Automatic retention pass blocked; retrying on the next interval");
            }
            // Coalesce bursts of deployment/settings notifications and bound retries.
            tokio::select! { () = state.shutdown.cancelled() => break, () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {} }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_without_active_counts_distinct_successful_releases() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        assert_eq!(
            retained_releases(None, 2, [a, a, b, c]),
            HashSet::from([a, b])
        );
        assert_eq!(retained_releases(Some(c), 1, [a, b, c]), HashSet::from([c]));
    }
    #[tokio::test]
    async fn retention_migration_preserves_manual_image_ledger_and_foreign_keys() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/202609060001_image_cleanup.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO image_cleanup_previews VALUES(x'01','session','{}','time','time')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO image_cleanup_operations VALUES('manual',x'01','[]',x'02','time')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO image_cleanup_items VALUES('manual',0,'image','started')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/202609090001_retention.sql"))
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM image_cleanup_items WHERE operation_id='manual'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            "started"
        );
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            sqlx::query("INSERT INTO image_cleanup_items VALUES('missing',0,'other','planned')")
                .execute(&db)
                .await
                .is_err()
        );
        sqlx::query(
            "INSERT INTO image_cleanup_operations VALUES('automatic',NULL,'[]',x'03','time')",
        )
        .execute(&db)
        .await
        .unwrap();
    }
}
