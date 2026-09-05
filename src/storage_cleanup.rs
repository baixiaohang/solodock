use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    app_store::{
        AppStore, StoreError,
        cleanup::{
            CleanupArtifact, CleanupTempLocation, exact_internal_temp_name, valid_temp_name,
        },
        config_revision,
        releases::ReleaseV2,
    },
    db::Database,
};

pub const RECENT_ROLLBACK_RELEASES: usize = 3;
pub const MAX_CLEANUP_ITEMS: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupPlan {
    pub candidates: Vec<CleanupCandidate>,
    pub protected: Vec<ProtectedArtifact>,
    pub estimated_logical_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupCandidate {
    #[serde(flatten)]
    pub artifact: CleanupArtifact,
    pub estimated_logical_bytes: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub release_created_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_record: Option<CleanedReleaseRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanedReleaseRecord {
    pub manifest_digest: String,
    pub local_image_id: String,
    pub platform_os: String,
    pub platform_architecture: String,
    pub platform_variant: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtectedArtifact {
    pub app_id: Option<Uuid>,
    pub artifact_kind: String,
    pub artifact_id: String,
    pub reason: ProtectionReason,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionReason {
    Active,
    Pending,
    CurrentDraft,
    RecentRollback,
    DeploymentRecovery,
    CleanupInProgress,
}

#[derive(Debug, Error)]
pub enum CleanupError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("the recovery inventory is incomplete")]
    InventoryIncomplete,
    #[error("cleanup records are invalid")]
    RecordInvalid,
}

impl From<std::io::Error> for CleanupError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(StoreError::Io(error))
    }
}

impl From<crate::security::permissions::PermissionError> for CleanupError {
    fn from(error: crate::security::permissions::PermissionError) -> Self {
        Self::Store(StoreError::Permission(error))
    }
}

#[derive(Clone)]
struct ValidRelease {
    app_id: Uuid,
    release: ReleaseV2,
    bytes: u64,
}

pub async fn build_plan(
    store: &AppStore,
    database: &Database,
) -> Result<CleanupPlan, CleanupError> {
    match build_plan_inner(store, database, None).await {
        Err(CleanupError::Store(_)) => Err(CleanupError::InventoryIncomplete),
        result => result.map(|inventory| inventory.plan),
    }
}

/// Recheck current protections under the catalog and candidate app guards.
/// The caller's exact reservation is validated but does not veto itself.
/// This inventory authorizes only a subset of the original immutable plan.
pub(crate) async fn resume_candidates(
    store: &AppStore,
    database: &Database,
    operation: Uuid,
) -> Result<HashSet<CleanupArtifact>, CleanupError> {
    Ok(build_plan_inner(store, database, Some(operation))
        .await?
        .plan
        .candidates
        .into_iter()
        .map(|candidate| candidate.artifact)
        .collect())
}

pub(crate) struct ArtifactInventory {
    pub plan: CleanupPlan,
    pub app_ids: Vec<Uuid>,
    pub releases: Vec<CleanedReleaseRecord>,
}

pub(crate) async fn image_protection_inventory(
    store: &AppStore,
    database: &Database,
) -> Result<ArtifactInventory, CleanupError> {
    build_plan_inner(store, database, None).await
}

async fn build_plan_inner(
    store: &AppStore,
    database: &Database,
    resuming_operation: Option<Uuid>,
) -> Result<ArtifactInventory, CleanupError> {
    let report = store.scan_read_only()?;
    crate::mutation::idempotency::IdempotencyService::validate_app_tombstones(database, store)
        .await
        .map_err(|_| CleanupError::InventoryIncomplete)?;
    if report
        .issues
        .iter()
        .any(|issue| issue.code != "TEMP_ARTIFACT_IGNORED")
    {
        return Err(CleanupError::InventoryIncomplete);
    }

    let mut releases = Vec::new();
    let mut revisions: BTreeMap<(Uuid, Uuid), u64> = BTreeMap::new();
    let mut temps = Vec::new();
    collect_root_temps(store, &mut temps)?;
    for app in &report.valid_apps {
        let app_directory = store.app_directory(app.app_id);
        let releases_directory = app_directory.join("releases");
        for entry in fs::read_dir(&releases_directory)? {
            let entry = entry?;
            if exact_internal_temp_name(&entry.file_name()) {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| CleanupError::InventoryIncomplete)?;
                let bytes = temp_bytes(
                    &entry.path(),
                    Some(app.app_id),
                    CleanupTempLocation::Releases,
                    &name,
                )?;
                temps.push(candidate_temp(
                    Some(app.app_id),
                    CleanupTempLocation::Releases,
                    name,
                    bytes,
                ));
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| CleanupError::InventoryIncomplete)?;
            let release_id = parse_canonical_uuid(&name)?;
            let release = store.load_v2_release(app.app_id, release_id)?;
            releases.push(ValidRelease {
                app_id: app.app_id,
                release,
                bytes: logical_bytes(&entry.path())?,
            });
        }
        let revision_directory = app_directory.join("config-revisions");
        for entry in fs::read_dir(&revision_directory)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| CleanupError::InventoryIncomplete)?;
            if valid_temp_name(CleanupTempLocation::ConfigRevisions, &name) {
                let bytes = temp_bytes(
                    &entry.path(),
                    Some(app.app_id),
                    CleanupTempLocation::ConfigRevisions,
                    &name,
                )?;
                temps.push(candidate_temp(
                    Some(app.app_id),
                    CleanupTempLocation::ConfigRevisions,
                    name,
                    bytes,
                ));
                continue;
            }
            let revision = parse_canonical_uuid(&name)?;
            config_revision::load_verified(&app_directory, revision, store.integrity_key()?)?;
            revisions.insert(
                (app.app_id, revision),
                crate::app_store::cleanup::logical_artifact_bytes(
                    &entry.path(),
                    &CleanupArtifact::ConfigRevision {
                        app_id: app.app_id,
                        revision_id: revision,
                    },
                )?,
            );
        }
        for entry in fs::read_dir(&app_directory)? {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => return Err(CleanupError::InventoryIncomplete),
            };
            if valid_temp_name(CleanupTempLocation::AppRoot, &name) {
                let bytes = temp_bytes(
                    &entry.path(),
                    Some(app.app_id),
                    CleanupTempLocation::AppRoot,
                    &name,
                )?;
                temps.push(candidate_temp(
                    Some(app.app_id),
                    CleanupTempLocation::AppRoot,
                    name,
                    bytes,
                ));
            } else if !matches!(
                name.as_str(),
                "app.toml"
                    | "active"
                    | "pending"
                    | "releases"
                    | "config-revisions"
                    | "webhook.toml"
                    | "webhook-secret-revisions"
            ) {
                return Err(CleanupError::InventoryIncomplete);
            }
        }
    }

    releases.sort_by_key(|item| (item.release.created_at, item.app_id, item.release.id));
    temps.sort_by(|left, right| left.artifact.cmp(&right.artifact));

    let mut release_protection: HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>> = HashMap::new();
    let mut revision_protection: HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>> = HashMap::new();
    let mut temporary_protection = HashSet::new();
    for app in &report.valid_apps {
        protect_release(
            &mut release_protection,
            app.app_id,
            app.active_release_id,
            ProtectionReason::Active,
        );
        protect_release(
            &mut release_protection,
            app.app_id,
            app.pending_release_id,
            ProtectionReason::Pending,
        );
        protect_revision(
            &mut revision_protection,
            app.app_id,
            app.draft_revision,
            ProtectionReason::CurrentDraft,
        );
    }
    protect_from_deployments(
        database,
        &releases,
        &mut release_protection,
        &mut revision_protection,
    )
    .await?;
    protect_from_cleanup_operations(
        store,
        database,
        &mut release_protection,
        &mut revision_protection,
        &mut temporary_protection,
        resuming_operation,
    )
    .await?;

    let valid_release_ids: HashSet<_> = releases
        .iter()
        .map(|item| (item.app_id, item.release.id))
        .collect();
    let valid_revision_ids: HashSet<_> = revisions.keys().copied().collect();
    let valid_temporary_artifacts: HashSet<_> = temps
        .iter()
        .map(|candidate| candidate.artifact.clone())
        .collect();
    if release_protection
        .keys()
        .any(|key| !valid_release_ids.contains(key))
        || revision_protection
            .keys()
            .any(|key| !valid_revision_ids.contains(key))
        || temporary_protection
            .iter()
            .any(|artifact| !valid_temporary_artifacts.contains(artifact))
    {
        return Err(CleanupError::InventoryIncomplete);
    }

    let mut candidates = Vec::new();
    let mut selected_releases = HashSet::new();
    for item in &releases {
        if candidates.len() == MAX_CLEANUP_ITEMS {
            break;
        }
        if release_protection.contains_key(&(item.app_id, item.release.id)) {
            continue;
        }
        selected_releases.insert((item.app_id, item.release.id));
        candidates.push(CleanupCandidate {
            artifact: CleanupArtifact::Release {
                app_id: item.app_id,
                release_id: item.release.id,
                config_revision_id: item.release.config_revision,
            },
            estimated_logical_bytes: item.bytes,
            release_created_at: Some(item.release.created_at),
            release_record: Some(CleanedReleaseRecord {
                manifest_digest: item.release.manifest_digest.clone(),
                local_image_id: item.release.local_image_id.clone(),
                platform_os: item.release.platform_os.clone(),
                platform_architecture: item.release.platform_architecture.clone(),
                platform_variant: item.release.platform_variant.clone(),
            }),
        });
    }

    let retained_revisions: HashSet<_> = releases
        .iter()
        .filter(|item| !selected_releases.contains(&(item.app_id, item.release.id)))
        .map(|item| (item.app_id, item.release.config_revision))
        .chain(revision_protection.keys().copied())
        .collect();
    for ((app_id, revision), bytes) in revisions {
        if candidates.len() == MAX_CLEANUP_ITEMS {
            break;
        }
        if !retained_revisions.contains(&(app_id, revision)) {
            candidates.push(CleanupCandidate {
                artifact: CleanupArtifact::ConfigRevision {
                    app_id,
                    revision_id: revision,
                },
                estimated_logical_bytes: bytes,
                release_created_at: None,
                release_record: None,
            });
        }
    }
    for temp in temps {
        if candidates.len() == MAX_CLEANUP_ITEMS {
            break;
        }
        if !temporary_protection.contains(&temp.artifact) {
            candidates.push(temp);
        }
    }

    let mut protected = Vec::new();
    for ((app_id, release_id), reasons) in release_protection {
        for reason in reasons {
            protected.push(ProtectedArtifact {
                app_id: Some(app_id),
                artifact_kind: "release".to_owned(),
                artifact_id: release_id.to_string(),
                reason,
            });
        }
    }
    for ((app_id, revision_id), reasons) in revision_protection {
        for reason in reasons {
            protected.push(ProtectedArtifact {
                app_id: Some(app_id),
                artifact_kind: "config_revision".to_owned(),
                artifact_id: revision_id.to_string(),
                reason,
            });
        }
    }
    for artifact in temporary_protection {
        protected.push(ProtectedArtifact {
            app_id: artifact.app_id(),
            artifact_kind: artifact.kind_name().to_owned(),
            artifact_id: artifact.public_id(),
            reason: ProtectionReason::CleanupInProgress,
        });
    }
    protected.sort();
    let estimated_logical_bytes = candidates
        .iter()
        .map(|candidate| candidate.estimated_logical_bytes)
        .sum();
    Ok(ArtifactInventory {
        app_ids: report.valid_apps.iter().map(|app| app.app_id).collect(),
        releases: releases
            .iter()
            .map(|item| CleanedReleaseRecord {
                manifest_digest: item.release.manifest_digest.clone(),
                local_image_id: item.release.local_image_id.clone(),
                platform_os: item.release.platform_os.clone(),
                platform_architecture: item.release.platform_architecture.clone(),
                platform_variant: item.release.platform_variant.clone(),
            })
            .collect(),
        plan: CleanupPlan {
            candidates,
            protected,
            estimated_logical_bytes,
        },
    })
}

pub fn canonical_plan_json(plan: &CleanupPlan) -> Result<String, CleanupError> {
    serde_json::to_string(plan).map_err(|_| CleanupError::RecordInvalid)
}

pub fn plan_hash(plan_json: &str) -> Vec<u8> {
    Sha256::digest(plan_json.as_bytes()).to_vec()
}

/// Removes only payloads whose durable operation plan and terminal
/// idempotency response form one exact recovery proof.
pub async fn finalize_succeeded(store: &AppStore, database: &Database) -> Result<(), CleanupError> {
    let tombstones: HashSet<_> = store.cleanup_tombstones()?.into_iter().collect();
    let mut operations = tombstones.clone();
    for id in sqlx::query_scalar::<_, String>(
        "SELECT operation_id FROM storage_cleanup_operations WHERE retirement_pending=1",
    )
    .fetch_all(database.pool())
    .await?
    {
        operations.insert(parse_db_uuid(id)?);
    }
    for operation_id in operations {
        let marker = tombstones
            .contains(&operation_id)
            .then(|| store.read_cleanup_marker(operation_id))
            .transpose()?;
        let operation = sqlx::query(
            "SELECT cleanup_kind,plan_hash,plan_json,status,completed_at,retirement_pending FROM storage_cleanup_operations WHERE operation_id=?",
        )
        .bind(operation_id.to_string())
        .fetch_optional(database.pool())
        .await?
        .ok_or(CleanupError::RecordInvalid)?;
        let cleanup_kind: String = operation.get(0);
        let stored_hash: Vec<u8> = operation.get(1);
        let plan_json: String = operation.get(2);
        let status: String = operation.get(3);
        let completed_at: Option<String> = operation.get(4);
        let retiring: bool = operation.get(5);
        let plan: CleanupPlan =
            serde_json::from_str(&plan_json).map_err(|_| CleanupError::RecordInvalid)?;
        if cleanup_kind != "artifacts"
            || plan_hash(&plan_json) != stored_hash
            || marker.as_ref().is_some_and(|marker| {
                marker.plan_hash != crate::app_store::cleanup::encode_hex(&stored_hash)
                    || marker.items
                        != plan
                            .candidates
                            .iter()
                            .map(|candidate| candidate.artifact.clone())
                            .collect::<Vec<_>>()
            })
            || (marker.is_none() && !retiring)
        {
            return Err(CleanupError::RecordInvalid);
        }
        if !matches!(status.as_str(), "completed" | "completed_with_failures") {
            if retiring {
                return Err(CleanupError::RecordInvalid);
            }
            continue;
        }
        if completed_at.is_none() {
            return Err(CleanupError::RecordInvalid);
        }
        let expected_items = exact_terminal_items(database, operation_id, &plan).await?;
        let has_failures = expected_items
            .iter()
            .any(|item| item.get("status").and_then(|value| value.as_str()) == Some("retained"));
        if (status == "completed_with_failures") != has_failures {
            return Err(CleanupError::RecordInvalid);
        }
        if proof_is_recoverable(database, operation_id).await? {
            if retiring {
                return Err(CleanupError::RecordInvalid);
            }
            continue;
        }
        verify_terminal_proof(
            database,
            operation_id,
            &crate::app_store::cleanup::encode_hex(&stored_hash),
            &status,
            expected_items,
        )
        .await?;
        // Commit the retirement owner before removing the last signed marker.
        // It survives visible unlink + failed directory sync, and protects GC.
        sqlx::query(
            "UPDATE storage_cleanup_operations SET retirement_pending=1 WHERE operation_id=?",
        )
        .bind(operation_id.to_string())
        .execute(database.pool())
        .await?;
        if marker.is_some() {
            store.finalize_cleanup_tombstone(operation_id)?;
        } else {
            store.sync_cleanup_retirement(operation_id)?;
        }
        sqlx::query(
            "UPDATE storage_cleanup_operations SET retirement_pending=0 WHERE operation_id=?",
        )
        .bind(operation_id.to_string())
        .execute(database.pool())
        .await?;
    }
    Ok(())
}

/// Validates the cleanup recovery ledger and returns the number of operations
/// that still own a resumable plan, preparation directory, or tombstone.
pub async fn pending_operation_count(
    store: &AppStore,
    database: &Database,
) -> Result<usize, CleanupError> {
    let mut release_protection = HashMap::new();
    let mut revision_protection = HashMap::new();
    let mut temporary_protection = HashSet::new();
    protect_from_cleanup_operations(
        store,
        database,
        &mut release_protection,
        &mut revision_protection,
        &mut temporary_protection,
        None,
    )
    .await?;

    let mut pending: HashSet<Uuid> = store.cleanup_tombstones()?.into_iter().collect();
    pending.extend(store.cleanup_preparations()?);
    for row in sqlx::query(
        "SELECT operation_id FROM storage_cleanup_operations WHERE status IN ('planned','running') OR retirement_pending=1",
    )
    .fetch_all(database.pool())
    .await?
    {
        pending.insert(parse_db_uuid(row.get::<String, _>(0))?);
    }
    Ok(pending.len())
}

async fn verify_terminal_proof(
    database: &Database,
    operation_id: Uuid,
    encoded_plan_hash: &str,
    status: &str,
    expected_items: Vec<serde_json::Value>,
) -> Result<(), CleanupError> {
    let proof = sqlx::query("SELECT route,status,response_status,response_body FROM idempotency_records WHERE operation_id=?")
        .bind(operation_id.to_string())
        .fetch_optional(database.pool())
        .await?
        .ok_or(CleanupError::RecordInvalid)?;
    let response_body: Option<String> = proof.get(3);
    let body = response_body
        .as_deref()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .ok_or(CleanupError::RecordInvalid)?;
    let expected_body = serde_json::json!({
        "operation_id": operation_id,
        "plan_hash": encoded_plan_hash,
        "status": status,
        "items": expected_items,
        "idempotency_replayed": false,
    });
    if proof.get::<String, _>(0) != "/api/v1/system/storage-cleanup/apply"
        || proof.get::<String, _>(1) != "succeeded"
        || proof.get::<Option<i64>, _>(2) != Some(200)
        || body != expected_body
    {
        return Err(CleanupError::RecordInvalid);
    }
    Ok(())
}

async fn proof_is_recoverable(database: &Database, operation: Uuid) -> Result<bool, CleanupError> {
    let row = sqlx::query("SELECT route,status FROM idempotency_records WHERE operation_id=?")
        .bind(operation.to_string())
        .fetch_optional(database.pool())
        .await?
        .ok_or(CleanupError::RecordInvalid)?;
    if row.get::<String, _>(0) != "/api/v1/system/storage-cleanup/apply" {
        return Err(CleanupError::RecordInvalid);
    }
    match row.get::<String, _>(1).as_str() {
        "pending" | "interrupted" => Ok(true),
        "succeeded" => Ok(false),
        _ => Err(CleanupError::RecordInvalid),
    }
}

pub(crate) async fn exact_terminal_items(
    database: &Database,
    operation_id: Uuid,
    plan: &CleanupPlan,
) -> Result<Vec<serde_json::Value>, CleanupError> {
    let states = exact_item_states(database, operation_id, plan).await?;
    let mut response = Vec::with_capacity(states.len());
    for (ordinal, (state, candidate)) in states.iter().zip(&plan.candidates).enumerate() {
        let (public_status, public_error) =
            match (state.status.as_str(), state.error_code.as_deref()) {
                ("detached", None) => ("deleted", None),
                ("failed", Some(code)) => ("retained", Some(code)),
                _ => return Err(CleanupError::RecordInvalid),
            };
        let mut item = serde_json::json!({
            "app_id": candidate.artifact.app_id(),
            "artifact_kind": candidate.artifact.kind_name(),
            "artifact_id": match candidate.artifact {
                CleanupArtifact::Temporary { .. } => format!("temporary-{}", ordinal + 1),
                _ => candidate.artifact.public_id(),
            },
            "status": public_status,
        });
        if let Some(code) = public_error {
            item.as_object_mut()
                .expect("cleanup response item is an object")
                .insert(
                    "error_code".to_owned(),
                    serde_json::Value::String(code.to_owned()),
                );
        }
        response.push(item);
    }
    Ok(response)
}

struct CleanupItemState {
    status: String,
    error_code: Option<String>,
}

async fn exact_item_states(
    database: &Database,
    operation_id: Uuid,
    plan: &CleanupPlan,
) -> Result<Vec<CleanupItemState>, CleanupError> {
    let rows = sqlx::query("SELECT ordinal,app_id,artifact_kind,artifact_id,config_revision_id,status,error_code FROM storage_cleanup_items WHERE operation_id=? ORDER BY ordinal")
        .bind(operation_id.to_string())
        .fetch_all(database.pool())
        .await?;
    if rows.len() != plan.candidates.len() {
        return Err(CleanupError::RecordInvalid);
    }
    let mut states = Vec::with_capacity(rows.len());
    for (ordinal, (row, candidate)) in rows.iter().zip(&plan.candidates).enumerate() {
        let stored_ordinal: i64 = row.get(0);
        let app_id: Option<String> = row.get(1);
        let artifact_kind: String = row.get(2);
        let artifact_id: String = row.get(3);
        let config_revision_id: Option<String> = row.get(4);
        let item_status: String = row.get(5);
        let error_code: Option<String> = row.get(6);
        if !matches!(
            (item_status.as_str(), error_code.as_deref()),
            ("planned" | "detached", None)
                | (
                    "failed",
                    Some("CLEANUP_ITEM_RETAINED" | "RELEASE_RETAINED" | "CLEANUP_ITEM_PROTECTED")
                )
        ) {
            return Err(CleanupError::RecordInvalid);
        }
        let expected_revision = match candidate.artifact {
            CleanupArtifact::Release {
                config_revision_id, ..
            }
            | CleanupArtifact::ConfigRevision {
                revision_id: config_revision_id,
                ..
            } => Some(config_revision_id.to_string()),
            CleanupArtifact::Temporary { .. } => None,
        };
        if stored_ordinal != ordinal as i64
            || app_id != candidate.artifact.app_id().map(|id| id.to_string())
            || artifact_kind != candidate.artifact.kind_name()
            || artifact_id != candidate.artifact.public_id()
            || config_revision_id != expected_revision
        {
            return Err(CleanupError::RecordInvalid);
        }
        states.push(CleanupItemState {
            status: item_status,
            error_code,
        });
    }
    Ok(states)
}

async fn protect_from_deployments(
    database: &Database,
    releases: &[ValidRelease],
    release_protection: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
    revision_protection: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
) -> Result<(), CleanupError> {
    let rows = sqlx::query("SELECT id,app_id,requested_revision,from_release_id,expected_pending_release_id,expected_actual_release_id,predecessor_runtime_release_id,candidate_release_id,rollback_target_release_id,status FROM deployments ORDER BY created_at DESC,id DESC")
        .fetch_all(database.pool())
        .await?;
    let valid: HashSet<_> = releases
        .iter()
        .map(|item| (item.app_id, item.release.id))
        .collect();
    let mut recent: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
    for row in rows {
        let app_id = parse_db_uuid(row.get::<String, _>(1))?;
        let status: String = row.get(9);
        let requested_revision = parse_db_uuid(row.get::<String, _>(2))?;
        let references = [
            optional_db_uuid(row.get::<Option<String>, _>(3))?,
            optional_db_uuid(row.get::<Option<String>, _>(4))?,
            optional_db_uuid(row.get::<Option<String>, _>(5))?,
            optional_db_uuid(row.get::<Option<String>, _>(6))?,
            optional_db_uuid(row.get::<Option<String>, _>(7))?,
            optional_db_uuid(row.get::<Option<String>, _>(8))?,
        ];
        let candidate = references[4];
        if matches!(
            status.as_str(),
            "succeeded" | "no_op" | "failed" | "rolled_back"
        ) && let Some(candidate) = candidate
            && valid.contains(&(app_id, candidate))
            && !release_protection
                .get(&(app_id, candidate))
                .is_some_and(|reasons| {
                    reasons.contains(&ProtectionReason::Active)
                        || reasons.contains(&ProtectionReason::Pending)
                })
            && recent.entry(app_id).or_default().len() < RECENT_ROLLBACK_RELEASES
        {
            let values = recent.entry(app_id).or_default();
            if values.insert(candidate) {
                release_protection
                    .entry((app_id, candidate))
                    .or_default()
                    .insert(ProtectionReason::RecentRollback);
            }
        }
        if matches!(
            status.as_str(),
            "queued" | "running" | "interrupted" | "needs_attention"
        ) {
            protect_revision(
                revision_protection,
                app_id,
                Some(requested_revision),
                ProtectionReason::DeploymentRecovery,
            );
            for release_id in references {
                protect_release(
                    release_protection,
                    app_id,
                    release_id,
                    ProtectionReason::DeploymentRecovery,
                );
            }
        }
    }
    Ok(())
}

async fn protect_from_cleanup_operations(
    store: &AppStore,
    database: &Database,
    release_protection: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
    revision_protection: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
    temporary_protection: &mut HashSet<CleanupArtifact>,
    resuming_operation: Option<Uuid>,
) -> Result<(), CleanupError> {
    let tombstones: HashSet<_> = store.cleanup_tombstones()?.into_iter().collect();
    let preparations: HashSet<_> = store.cleanup_preparations()?.into_iter().collect();
    let rows = sqlx::query(
        "SELECT operation_id,plan_hash,plan_json,status,completed_at,retirement_pending FROM storage_cleanup_operations",
    )
    .fetch_all(database.pool())
    .await?;
    let mut matched_tombstones = HashSet::new();
    let mut matched_preparations = HashSet::new();
    for row in rows {
        let operation = parse_db_uuid(row.get::<String, _>(0))?;
        let hash: Vec<u8> = row.get(1);
        let json: String = row.get(2);
        let status: String = row.get(3);
        let completed_at: Option<String> = row.get(4);
        let retiring: bool = row.get(5);
        let has_tombstone = tombstones.contains(&operation);
        let has_preparation = preparations.contains(&operation);
        if has_tombstone && has_preparation {
            return Err(CleanupError::RecordInvalid);
        }
        if !has_tombstone
            && !retiring
            && matches!(status.as_str(), "completed" | "completed_with_failures")
        {
            continue;
        }
        let plan: CleanupPlan =
            serde_json::from_str(&json).map_err(|_| CleanupError::RecordInvalid)?;
        let item_states = exact_item_states(database, operation, &plan).await?;
        if plan_hash(&json) != hash {
            return Err(CleanupError::RecordInvalid);
        }
        match status.as_str() {
            "planned"
                if completed_at.is_none()
                    && item_states
                        .iter()
                        .all(|item| item.status == "planned" && item.error_code.is_none()) => {}
            "running"
                if completed_at.is_none()
                    && item_states.iter().all(|item| {
                        matches!(item.status.as_str(), "planned" | "detached" | "failed")
                            && (item.status == "failed") == item.error_code.is_some()
                    }) => {}
            "completed" | "completed_with_failures"
                if (has_tombstone || retiring) && completed_at.is_some() => {}
            _ => return Err(CleanupError::RecordInvalid),
        }
        if retiring {
            if !matches!(status.as_str(), "completed" | "completed_with_failures") {
                return Err(CleanupError::RecordInvalid);
            }
            let items = exact_terminal_items(database, operation, &plan).await?;
            let failed = items.iter().any(|item| item["status"] == "retained");
            if (status == "completed_with_failures") != failed {
                return Err(CleanupError::RecordInvalid);
            }
            verify_terminal_proof(
                database,
                operation,
                &crate::app_store::cleanup::encode_hex(&hash),
                &status,
                items,
            )
            .await?;
        }
        if has_preparation {
            if status != "planned"
                || item_states
                    .iter()
                    .any(|item| item.status != "planned" || item.error_code.is_some())
            {
                return Err(CleanupError::RecordInvalid);
            }
            matched_preparations.insert(operation);
        }
        if matches!(status.as_str(), "planned" | "running") {
            let proof =
                sqlx::query("SELECT route,status FROM idempotency_records WHERE operation_id=?")
                    .bind(operation.to_string())
                    .fetch_optional(database.pool())
                    .await?
                    .ok_or(CleanupError::RecordInvalid)?;
            if proof.get::<String, _>(0) != "/api/v1/system/storage-cleanup/apply"
                || !matches!(
                    proof.get::<String, _>(1).as_str(),
                    "pending" | "interrupted"
                )
            {
                return Err(CleanupError::RecordInvalid);
            }
        }
        if has_tombstone {
            let marker = store.read_cleanup_marker(operation)?;
            if marker.plan_hash != crate::app_store::cleanup::encode_hex(&hash)
                || marker.items
                    != plan
                        .candidates
                        .iter()
                        .map(|candidate| candidate.artifact.clone())
                        .collect::<Vec<_>>()
            {
                return Err(CleanupError::RecordInvalid);
            }
            if matches!(status.as_str(), "completed" | "completed_with_failures")
                && resuming_operation != Some(operation)
            {
                let expected_items = exact_terminal_items(database, operation, &plan).await?;
                let has_failures = expected_items.iter().any(|item| {
                    item.get("status").and_then(|value| value.as_str()) == Some("retained")
                });
                if (status == "completed_with_failures") != has_failures {
                    return Err(CleanupError::RecordInvalid);
                }
                if !proof_is_recoverable(database, operation).await? {
                    verify_terminal_proof(
                        database,
                        operation,
                        &marker.plan_hash,
                        &status,
                        expected_items,
                    )
                    .await?;
                }
            }
            matched_tombstones.insert(operation);
        } else if status == "running" && item_states.iter().any(|item| item.status != "planned") {
            return Err(CleanupError::RecordInvalid);
        }
        if resuming_operation == Some(operation) {
            let proof =
                sqlx::query("SELECT route,status FROM idempotency_records WHERE operation_id=?")
                    .bind(operation.to_string())
                    .fetch_one(database.pool())
                    .await?;
            if proof.get::<String, _>(0) != "/api/v1/system/storage-cleanup/apply"
                || !matches!(
                    proof.get::<String, _>(1).as_str(),
                    "pending" | "interrupted"
                )
            {
                return Err(CleanupError::RecordInvalid);
            }
            continue;
        }
        for (candidate, item_state) in plan.candidates.into_iter().zip(item_states) {
            if item_state.status == "detached" {
                continue;
            }
            let artifact = candidate.artifact;
            match artifact {
                CleanupArtifact::Release {
                    app_id, release_id, ..
                } => protect_release(
                    release_protection,
                    app_id,
                    Some(release_id),
                    ProtectionReason::CleanupInProgress,
                ),
                CleanupArtifact::ConfigRevision {
                    app_id,
                    revision_id,
                } => protect_revision(
                    revision_protection,
                    app_id,
                    Some(revision_id),
                    ProtectionReason::CleanupInProgress,
                ),
                artifact @ CleanupArtifact::Temporary { .. } => {
                    temporary_protection.insert(artifact);
                }
            }
        }
    }
    if matched_tombstones != tombstones {
        return Err(CleanupError::RecordInvalid);
    }
    if matched_preparations != preparations {
        return Err(CleanupError::RecordInvalid);
    }
    Ok(())
}

fn protect_release(
    map: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
    app_id: Uuid,
    release_id: Option<Uuid>,
    reason: ProtectionReason,
) {
    if let Some(release_id) = release_id {
        map.entry((app_id, release_id)).or_default().insert(reason);
    }
}

fn protect_revision(
    map: &mut HashMap<(Uuid, Uuid), BTreeSet<ProtectionReason>>,
    app_id: Uuid,
    revision_id: Option<Uuid>,
    reason: ProtectionReason,
) {
    if let Some(revision_id) = revision_id {
        map.entry((app_id, revision_id)).or_default().insert(reason);
    }
}

fn collect_root_temps(
    store: &AppStore,
    candidates: &mut Vec<CleanupCandidate>,
) -> Result<(), CleanupError> {
    for entry in fs::read_dir(store.apps_directory())? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| CleanupError::InventoryIncomplete)?;
        if valid_temp_name(CleanupTempLocation::AppsRoot, &name) {
            let bytes = temp_bytes(&entry.path(), None, CleanupTempLocation::AppsRoot, &name)?;
            candidates.push(candidate_temp(
                None,
                CleanupTempLocation::AppsRoot,
                name,
                bytes,
            ));
        }
    }
    Ok(())
}

fn candidate_temp(
    app_id: Option<Uuid>,
    location: CleanupTempLocation,
    name: String,
    bytes: u64,
) -> CleanupCandidate {
    CleanupCandidate {
        artifact: CleanupArtifact::Temporary {
            app_id,
            location,
            name,
        },
        estimated_logical_bytes: bytes,
        release_created_at: None,
        release_record: None,
    }
}

fn temp_bytes(
    path: &Path,
    app_id: Option<Uuid>,
    location: CleanupTempLocation,
    name: &str,
) -> Result<u64, CleanupError> {
    if !valid_temp_name(location, name) {
        return Err(CleanupError::InventoryIncomplete);
    }
    Ok(crate::app_store::cleanup::logical_artifact_bytes(
        path,
        &CleanupArtifact::Temporary {
            app_id,
            location,
            name: name.to_owned(),
        },
    )?)
}

fn logical_bytes(path: &Path) -> Result<u64, CleanupError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(StoreError::SymlinkBoundary.into());
    }
    if metadata.is_file() {
        crate::security::permissions::check_private(path, false)?;
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Err(StoreError::SymlinkBoundary.into());
    }
    crate::security::permissions::check_private(path, true)?;
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total = total
            .checked_add(logical_bytes(&entry?.path())?)
            .ok_or(CleanupError::InventoryIncomplete)?;
    }
    Ok(total)
}

fn parse_canonical_uuid(value: &str) -> Result<Uuid, CleanupError> {
    value
        .parse::<Uuid>()
        .ok()
        .filter(|id| id.to_string() == value)
        .ok_or(CleanupError::InventoryIncomplete)
}

fn parse_db_uuid(value: String) -> Result<Uuid, CleanupError> {
    value.parse().map_err(|_| CleanupError::RecordInvalid)
}

fn optional_db_uuid(value: Option<String>) -> Result<Option<Uuid>, CleanupError> {
    value.map(parse_db_uuid).transpose()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    use super::*;

    fn write_private_temp(apps: &Path, name: &str) {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(apps.join(name))
            .unwrap();
    }

    #[tokio::test]
    async fn preview_is_bounded_and_an_unfinished_exact_plan_protects_its_temp() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let apps = root.path().join("apps");
        let store =
            AppStore::initialize_verified(apps.clone(), b"cleanup-plan-test-key".to_vec()).unwrap();
        for _ in 0..101 {
            write_private_temp(&apps, &format!(".solodock-tmp-{}", Uuid::new_v4().simple()));
        }
        let first = build_plan(&store, &database).await.unwrap();
        assert_eq!(first.candidates.len(), MAX_CLEANUP_ITEMS);

        let protected_candidate = first.candidates[0].clone();
        let protected_plan = CleanupPlan {
            candidates: vec![protected_candidate.clone()],
            protected: Vec::new(),
            estimated_logical_bytes: protected_candidate.estimated_logical_bytes,
        };
        let json = canonical_plan_json(&protected_plan).unwrap();
        let hash = plan_hash(&json);
        let operation = Uuid::new_v4();
        let now = crate::db::format_time(OffsetDateTime::now_utc()).unwrap();
        sqlx::query("INSERT INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,created_at,updated_at) VALUES ('admin','/api/v1/system/storage-cleanup/apply',x'21',x'22',?,'interrupted',?,?)")
            .bind(operation.to_string())
            .bind(&now)
            .bind(&now)
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO storage_cleanup_operations (operation_id,cleanup_kind,plan_hash,plan_json,status,created_at) VALUES (?,'artifacts',?,?,'planned',?)")
            .bind(operation.to_string())
            .bind(&hash)
            .bind(&json)
            .bind(&now)
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO storage_cleanup_items (operation_id,ordinal,artifact_kind,artifact_id,status) VALUES (?,0,'temporary',?,'planned')")
            .bind(operation.to_string())
            .bind(protected_candidate.artifact.public_id())
            .execute(database.pool())
            .await
            .unwrap();
        let trash = apps.join(crate::app_store::cleanup::CLEANUP_TRASH_DIRECTORY);
        fs::DirBuilder::new().mode(0o700).create(&trash).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(trash.join(format!(".solodock-tmp-{}", operation.simple())))
            .unwrap();
        assert_eq!(pending_operation_count(&store, &database).await.unwrap(), 1);

        let resumed = build_plan(&store, &database).await.unwrap();
        assert_eq!(resumed.candidates.len(), MAX_CLEANUP_ITEMS);
        assert!(
            !resumed
                .candidates
                .iter()
                .any(|candidate| candidate.artifact == protected_candidate.artifact)
        );
        assert!(resumed.protected.iter().any(|item| {
            item.artifact_id == protected_candidate.artifact.public_id()
                && item.reason == ProtectionReason::CleanupInProgress
        }));
    }

    #[tokio::test]
    async fn cleanup_tombstone_requires_exact_terminal_proof_before_finalization() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let apps = root.path().join("apps");
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store =
            AppStore::initialize_verified(apps.clone(), b"cleanup-test-key".to_vec()).unwrap();
        let temp_name = format!(".solodock-tmp-{}", Uuid::new_v4().simple());
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(apps.join(&temp_name))
            .unwrap();
        let plan = CleanupPlan {
            candidates: vec![CleanupCandidate {
                artifact: CleanupArtifact::Temporary {
                    app_id: None,
                    location: CleanupTempLocation::AppsRoot,
                    name: temp_name.clone(),
                },
                estimated_logical_bytes: 0,
                release_created_at: None,
                release_record: None,
            }],
            protected: Vec::new(),
            estimated_logical_bytes: 0,
        };
        let json = canonical_plan_json(&plan).unwrap();
        let hash = plan_hash(&json);
        let operation = Uuid::new_v4();
        let now = crate::db::format_time(OffsetDateTime::now_utc()).unwrap();
        sqlx::query("INSERT INTO storage_cleanup_operations (operation_id,cleanup_kind,plan_hash,plan_json,status,created_at,completed_at) VALUES (?,'artifacts',?,?,'completed',?,?)")
            .bind(operation.to_string())
            .bind(&hash)
            .bind(&json)
            .bind(&now)
            .bind(&now)
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO storage_cleanup_items (operation_id,ordinal,artifact_kind,artifact_id,status) VALUES (?,0,'temporary',?,'detached')")
            .bind(operation.to_string())
            .bind(&temp_name)
            .execute(database.pool())
            .await
            .unwrap();
        store
            .prepare_cleanup_tombstone(operation, &hash, &[plan.candidates[0].artifact.clone()])
            .unwrap();
        assert_eq!(
            store
                .detach_cleanup_artifact(operation, 0, &plan.candidates[0].artifact)
                .unwrap(),
            crate::app_store::cleanup::DetachResult::Detached
        );
        assert!(store.scan_read_only().unwrap().valid_apps.is_empty());

        let wrong = serde_json::json!({
            "operation_id": operation,
            "plan_hash": "wrong",
            "status": "completed",
            "items": [],
            "idempotency_replayed": false,
        })
        .to_string();
        sqlx::query("INSERT INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,response_status,response_body,created_at,updated_at) VALUES ('admin','/api/v1/system/storage-cleanup/apply',x'01',x'02',?,'succeeded',200,?,?,?)")
            .bind(operation.to_string())
            .bind(wrong)
            .bind(&now)
            .bind(&now)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(finalize_succeeded(&store, &database).await.is_err());
        assert_eq!(store.cleanup_tombstones().unwrap(), vec![operation]);

        let correct = serde_json::json!({
            "operation_id": operation,
            "plan_hash": crate::app_store::cleanup::encode_hex(&hash),
            "status": "completed",
            "items": [{
                "app_id": null,
                "artifact_kind": "temporary",
                "artifact_id": "temporary-1",
                "status": "deleted",
            }],
            "idempotency_replayed": false,
        })
        .to_string();
        sqlx::query("UPDATE idempotency_records SET response_body=? WHERE operation_id=?")
            .bind(correct)
            .bind(operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(operation.to_string())
        .execute(database.pool())
        .await
        .unwrap();
        let disposable = Uuid::new_v4();
        sqlx::query("INSERT INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,response_status,response_body,error_code,created_at,updated_at) VALUES ('admin','/old',x'11',x'12',?,'failed',409,'{}','OLD','2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')")
            .bind(disposable.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        let idempotency =
            crate::mutation::IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let credentials = crate::registry::CredentialStore::initialize(
            root.path().join("registry-credentials"),
            idempotency.integrity_key(),
        )
        .unwrap();
        let webhooks =
            crate::webhook::WebhookStore::new(store.clone(), idempotency.integrity_key());
        assert_eq!(
            idempotency
                .gc_with_artifact_inventory(&store, &credentials, &webhooks)
                .await
                .unwrap(),
            1
        );
        let proof_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(operation.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(proof_count, 1);
        store.fail_next_cleanup_finalize();
        assert!(finalize_succeeded(&store, &database).await.is_err());
        assert_eq!(store.cleanup_tombstones().unwrap(), vec![operation]);
        assert_eq!(pending_operation_count(&store, &database).await.unwrap(), 1);
        finalize_succeeded(&store, &database).await.unwrap();
        assert!(store.cleanup_tombstones().unwrap().is_empty());
        assert_eq!(pending_operation_count(&store, &database).await.unwrap(), 0);
    }
}
