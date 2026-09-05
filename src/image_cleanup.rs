//! Read-only image eligibility, sharing artifact recovery and image identity rules.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    app_store::{AppStore, cleanup::CleanupArtifact},
    db::Database,
    docker::{
        image_cleanup::{CleanupImage, ExactImageId, ImageCleanup},
        models::ContainerRecord,
    },
    registry::{ImageIdentity, Platform},
    storage_cleanup::{CleanedReleaseRecord, CleanupError, CleanupPlan, plan_hash},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageCandidate {
    pub image_id: ExactImageId,
    pub identity: CleanedReleaseRecord,
    pub reported_size_bytes: u64,
    pub repo_digests: Vec<String>,
    pub repo_tags: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImagePlan {
    pub candidates: Vec<ImageCandidate>,
    pub protected_count: usize,
    pub facts_hash: Vec<u8>,
}

pub(crate) fn identity(record: &CleanedReleaseRecord) -> Result<ImageIdentity, CleanupError> {
    let platform = Platform::canonical(
        &record.platform_os,
        &record.platform_architecture,
        record.platform_variant.as_deref(),
    )
    .map_err(|_| CleanupError::InventoryIncomplete)?;
    ImageIdentity::new(&record.manifest_digest, &record.local_image_id, &platform)
        .map_err(|_| CleanupError::InventoryIncomplete)
}

fn overlaps(
    left: &CleanedReleaseRecord,
    right: &CleanedReleaseRecord,
) -> Result<bool, CleanupError> {
    let value = identity(left)?;
    identity(right)?;
    Ok(value.matches_engine_image_id(Some(&right.local_image_id))
        || value.matches_engine_image_id(Some(&right.manifest_digest)))
}

fn container_references(containers: &[ContainerRecord]) -> Result<BTreeSet<String>, CleanupError> {
    if containers.len() > 4096 {
        return Err(CleanupError::InventoryIncomplete);
    }
    let mut ids = BTreeSet::new();
    let mut references = BTreeSet::new();
    for container in containers {
        if !crate::docker::ownership::valid_container_id(&container.id)
            || !ids.insert(&container.id)
        {
            return Err(CleanupError::InventoryIncomplete);
        }
        let image = container
            .image_id
            .as_deref()
            .ok_or(CleanupError::InventoryIncomplete)?;
        ExactImageId::parse(image).map_err(|_| CleanupError::InventoryIncomplete)?;
        references.insert(image.to_owned());
        if let Some(descriptor) = &container.manifest_descriptor {
            let digest = descriptor
                .digest
                .as_deref()
                .ok_or(CleanupError::InventoryIncomplete)?;
            ExactImageId::parse(digest).map_err(|_| CleanupError::InventoryIncomplete)?;
            Platform::canonical(
                descriptor
                    .os
                    .as_deref()
                    .ok_or(CleanupError::InventoryIncomplete)?,
                descriptor
                    .architecture
                    .as_deref()
                    .ok_or(CleanupError::InventoryIncomplete)?,
                descriptor.variant.as_deref(),
            )
            .map_err(|_| CleanupError::InventoryIncomplete)?;
            references.insert(digest.to_owned());
        }
    }
    Ok(references)
}

pub(crate) fn matches_inspect(
    record: &CleanedReleaseRecord,
    observed: &CleanupImage,
) -> Result<bool, CleanupError> {
    let image = &observed.image;
    ExactImageId::parse(&image.id).map_err(|_| CleanupError::InventoryIncomplete)?;
    let platform = Platform::canonical(&image.os, &image.architecture, image.variant.as_deref())
        .map_err(|_| CleanupError::InventoryIncomplete)?;
    let expected_platform = Platform::canonical(
        &record.platform_os,
        &record.platform_architecture,
        record.platform_variant.as_deref(),
    )
    .map_err(|_| CleanupError::InventoryIncomplete)?;
    let descriptor_ok =
        identity(record)?.matches_observation(Some(&image.id), image.manifest_descriptor.as_ref());
    // Classic engines have no descriptor: their immutable RepoDigest must prove
    // the manifest as well as the config ID and platform.
    let digest_proven = image.manifest_descriptor.is_some()
        || image.repo_digests.iter().any(|value| {
            value
                .rsplit_once('@')
                .is_some_and(|(_, digest)| digest == record.manifest_digest)
        });
    Ok(platform == expected_platform && descriptor_ok && digest_proven)
}

/// All physical releases protect images, including releases eligible for a
/// future artifact cleanup. A planned artifact is not a cleaned artifact.
pub async fn build_plan(
    store: &AppStore,
    database: &Database,
    docker: &dyn ImageCleanup,
) -> Result<ImagePlan, CleanupError> {
    build_plan_for_operation(store, database, docker, None).await
}

pub(crate) async fn build_plan_for_operation(
    store: &AppStore,
    database: &Database,
    docker: &dyn ImageCleanup,
    resuming: Option<Uuid>,
) -> Result<ImagePlan, CleanupError> {
    validate_operations(database).await?;
    let inventory = crate::storage_cleanup::image_protection_inventory(store, database).await?;
    let tombstones: BTreeSet<_> = store.cleanup_tombstones()?.into_iter().collect();
    let rows = sqlx::query("SELECT c.*,o.plan_json,o.plan_hash,o.status AS operation_status,o.retirement_pending FROM cleaned_releases c JOIN storage_cleanup_operations o ON o.operation_id=c.cleanup_operation_id ORDER BY c.app_id,c.release_id LIMIT 4097")
        .fetch_all(database.pool()).await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases")
        .fetch_one(database.pool())
        .await?;
    if rows.len() > 4096 || count != rows.len() as i64 {
        return Err(CleanupError::InventoryIncomplete);
    }
    let mut retained = inventory.releases;
    for operation in sqlx::query("SELECT operation_id,plan_json FROM image_cleanup_operations WHERE operation_id IN (SELECT operation_id FROM image_cleanup_items WHERE status IN ('planned','started')) ORDER BY operation_id").fetch_all(database.pool()).await? {
        let id: String = operation.get("operation_id");
        if resuming.is_some_and(|value|value.to_string()==id) { continue; }
        let plan:Vec<ImageCandidate>=serde_json::from_str(operation.get("plan_json")).map_err(|_|CleanupError::RecordInvalid)?;
        for item in plan { retained.push(item.identity); }
    }
    for release in store.tombstone_releases()? {
        retained.push(CleanedReleaseRecord {
            manifest_digest: release.manifest_digest,
            local_image_id: release.local_image_id,
            platform_os: release.platform_os,
            platform_architecture: release.platform_architecture,
            platform_variant: release.platform_variant,
        });
    }
    let mut cleaned = Vec::new();
    for row in rows {
        let record = CleanedReleaseRecord {
            manifest_digest: row.get("manifest_digest"),
            local_image_id: row.get("local_image_id"),
            platform_os: row.get("platform_os"),
            platform_architecture: row.get("platform_architecture"),
            platform_variant: row.get("platform_variant"),
        };
        identity(&record)?;
        let app: String = row.get("app_id");
        let release: String = row.get("release_id");
        let operation: String = row.get("cleanup_operation_id");
        let operation_id = Uuid::parse_str(&operation).map_err(|_| CleanupError::RecordInvalid)?;
        let json: String = row.get("plan_json");
        let hash: Vec<u8> = row.get("plan_hash");
        let plan: CleanupPlan =
            serde_json::from_str(&json).map_err(|_| CleanupError::RecordInvalid)?;
        if plan_hash(&json) != hash {
            return Err(CleanupError::RecordInvalid);
        }
        let matching: Vec<_> = plan.candidates.iter().enumerate().filter(|(_, item)| matches!(&item.artifact, CleanupArtifact::Release { app_id, release_id, .. } if app_id.to_string()==app && release_id.to_string()==release) && item.release_record.as_ref()==Some(&record)).collect();
        if matching.len() != 1 {
            return Err(CleanupError::RecordInvalid);
        }
        let item = sqlx::query("SELECT app_id,artifact_id,artifact_kind,status FROM storage_cleanup_items WHERE operation_id=? AND ordinal=?")
            .bind(&operation).bind(matching[0].0 as i64).fetch_one(database.pool()).await?;
        if item.get::<String, _>("app_id") != app
            || item.get::<String, _>("artifact_id") != release
            || item.get::<String, _>("artifact_kind") != "release"
            || item.get::<String, _>("status") != "detached"
        {
            return Err(CleanupError::RecordInvalid);
        }
        let terminal = matches!(
            row.get::<&str, _>("operation_status"),
            "completed" | "completed_with_failures"
        );
        if terminal
            && row.get::<i64, _>("retirement_pending") == 0
            && !tombstones.contains(&operation_id)
        {
            cleaned.push(record);
        } else {
            retained.push(record);
        }
    }
    let containers = docker
        .all_containers()
        .await
        .map_err(|_| CleanupError::InventoryIncomplete)?;
    let mut references = container_references(&containers)?;
    for container in &containers {
        let requested = ExactImageId::parse(
            container
                .image_id
                .as_deref()
                .ok_or(CleanupError::InventoryIncomplete)?,
        )
        .map_err(|_| CleanupError::InventoryIncomplete)?;
        let observed = docker
            .inspect(&requested)
            .await
            .map_err(|_| CleanupError::InventoryIncomplete)?
            .ok_or(CleanupError::InventoryIncomplete)?;
        let image_id = ExactImageId::parse(&observed.image.id)
            .map_err(|_| CleanupError::InventoryIncomplete)?;
        let platform = Platform::canonical(
            &observed.image.os,
            &observed.image.architecture,
            observed.image.variant.as_deref(),
        )
        .map_err(|_| CleanupError::InventoryIncomplete)?;
        references.insert(image_id.as_str().to_owned());
        if let Some(descriptor) = &observed.image.manifest_descriptor {
            let digest = descriptor
                .digest
                .as_deref()
                .ok_or(CleanupError::InventoryIncomplete)?;
            let config = if requested.as_str() == digest {
                image_id.as_str()
            } else {
                requested.as_str()
            };
            let expected = ImageIdentity::new(digest, config, &platform)
                .map_err(|_| CleanupError::InventoryIncomplete)?;
            if !expected.matches_observation(Some(image_id.as_str()), Some(descriptor))
                || !expected.matches_engine_image_id(Some(requested.as_str()))
            {
                return Err(CleanupError::InventoryIncomplete);
            }
            references.insert(digest.to_owned());
        } else if requested != image_id {
            // Without a descriptor, only an explicit RepoDigest can prove that
            // the requested manifest resolves to the returned classic config ID.
            let proven = observed.image.repo_digests.iter().any(|value| {
                value
                    .rsplit_once('@')
                    .is_some_and(|(_, digest)| digest == requested.as_str())
            });
            let expected = ImageIdentity::new(requested.as_str(), image_id.as_str(), &platform)
                .map_err(|_| CleanupError::InventoryIncomplete)?;
            if !proven || !expected.matches_observation(Some(image_id.as_str()), None) {
                return Err(CleanupError::InventoryIncomplete);
            }
        }
        if let Some(descriptor) = &container.manifest_descriptor {
            let manifest = descriptor
                .digest
                .as_deref()
                .ok_or(CleanupError::InventoryIncomplete)?;
            let record = CleanedReleaseRecord {
                manifest_digest: manifest.to_owned(),
                local_image_id: if requested.as_str() == manifest {
                    image_id.as_str()
                } else {
                    requested.as_str()
                }
                .to_owned(),
                platform_os: descriptor
                    .os
                    .clone()
                    .ok_or(CleanupError::InventoryIncomplete)?,
                platform_architecture: descriptor
                    .architecture
                    .clone()
                    .ok_or(CleanupError::InventoryIncomplete)?,
                platform_variant: descriptor.variant.clone(),
            };
            if !matches_inspect(&record, &observed)? {
                return Err(CleanupError::InventoryIncomplete);
            }
        }
    }
    let mut candidates = BTreeMap::new();
    let mut protected = BTreeSet::new();
    let mut observations = BTreeMap::new();
    for record in &cleaned {
        let expected = identity(record)?;
        let requested = ExactImageId::parse(&record.local_image_id)
            .map_err(|_| CleanupError::InventoryIncomplete)?;
        let mut observed = docker
            .inspect(&requested)
            .await
            .map_err(|_| CleanupError::InventoryIncomplete)?;
        if observed.is_none() && record.manifest_digest != record.local_image_id {
            let manifest = ExactImageId::parse(&record.manifest_digest)
                .map_err(|_| CleanupError::InventoryIncomplete)?;
            observed = docker
                .inspect(&manifest)
                .await
                .map_err(|_| CleanupError::InventoryIncomplete)?;
        }
        let Some(observed) = observed else {
            continue;
        };
        if !matches_inspect(record, &observed)? {
            return Err(CleanupError::InventoryIncomplete);
        }
        let id = ExactImageId::parse(&observed.image.id)
            .map_err(|_| CleanupError::InventoryIncomplete)?;
        let candidate = ImageCandidate {
            image_id: id.clone(),
            identity: record.clone(),
            reported_size_bytes: observed.reported_size_bytes,
            repo_digests: observed.image.repo_digests,
            repo_tags: observed.repo_tags,
        };
        observations.insert(id.clone(), candidate.clone());
        let mut is_protected = references
            .iter()
            .any(|value| expected.matches_engine_image_id(Some(value)) || value == id.as_str());
        for other in &retained {
            is_protected |= overlaps(record, other)?;
        }
        if is_protected {
            protected.insert(id);
        } else {
            candidates.entry(id).or_insert(candidate);
        }
    }
    for id in &protected {
        candidates.remove(id);
    }
    // Stable membership and image identity facts, not volatile CPU/status data.
    let mut container_facts: Vec<_> = containers
        .iter()
        .map(|item| {
            (
                &item.id,
                &item.image_id,
                item.manifest_descriptor
                    .as_ref()
                    .and_then(|value| value.digest.as_ref()),
            )
        })
        .collect();
    container_facts.sort();
    let facts = serde_json::to_string(&(
        inventory.plan,
        cleaned,
        retained,
        container_facts,
        observations.values().collect::<Vec<_>>(),
    ))
    .map_err(|_| CleanupError::RecordInvalid)?;
    Ok(ImagePlan {
        candidates: candidates
            .into_values()
            .take(crate::storage_cleanup::MAX_CLEANUP_ITEMS)
            .collect(),
        protected_count: protected.len(),
        facts_hash: plan_hash(&facts),
    })
}

pub async fn current_app_ids(
    store: &AppStore,
    database: &Database,
) -> Result<Vec<Uuid>, CleanupError> {
    let mut ids = crate::storage_cleanup::image_protection_inventory(store, database)
        .await?
        .app_ids;
    ids.sort();
    Ok(ids)
}

/// Read-only restart/restore validation. No daemon call or automatic execution.
/// Pending/interrupted proofs remain under the existing idempotency retention
/// rule; only terminal image operations may outlive their ordinary replay TTL.
pub async fn validate_operations(database: &Database) -> Result<(), CleanupError> {
    let operations = sqlx::query(
        "SELECT operation_id,token_hmac,plan_json,plan_hash FROM image_cleanup_operations",
    )
    .fetch_all(database.pool())
    .await?;
    for operation in operations {
        let id: String = operation.get("operation_id");
        let uuid = Uuid::parse_str(&id).map_err(|_| CleanupError::RecordInvalid)?;
        if uuid.to_string() != id {
            return Err(CleanupError::RecordInvalid);
        }
        let json: String = operation.get("plan_json");
        let hash: Vec<u8> = operation.get("plan_hash");
        let plan: Vec<ImageCandidate> =
            serde_json::from_str(&json).map_err(|_| CleanupError::RecordInvalid)?;
        if plan.is_empty()
            || plan.len() > 100
            || plan_hash(&json) != hash
            || plan
                .windows(2)
                .any(|pair| pair[0].image_id >= pair[1].image_id)
        {
            return Err(CleanupError::RecordInvalid);
        }
        for item in &plan {
            identity(&item.identity)?;
        }
        let preview = sqlx::query(
            "SELECT plan_json,consumed_at FROM image_cleanup_previews WHERE token_hmac=?",
        )
        .bind(operation.get::<Vec<u8>, _>("token_hmac"))
        .fetch_optional(database.pool())
        .await?
        .ok_or(CleanupError::RecordInvalid)?;
        let original: ImagePlan = serde_json::from_str(preview.get("plan_json"))
            .map_err(|_| CleanupError::RecordInvalid)?;
        if preview.get::<Option<String>, _>("consumed_at").is_none()
            || plan.iter().any(|item| !original.candidates.contains(item))
        {
            return Err(CleanupError::RecordInvalid);
        }
        let result = terminal_result(database, uuid, &plan, &hash).await?;
        let proof = sqlx::query("SELECT actor,route,status,response_status,response_body FROM idempotency_records WHERE operation_id=?").bind(&id).fetch_optional(database.pool()).await?;
        match proof {
            None if result.is_some() => {}
            Some(row)
                if row.get::<&str, _>("actor") == "admin"
                    && row.get::<&str, _>("route") == "/api/v1/system/image-cleanup/apply" =>
            {
                match row.get::<&str, _>("status") {
                    "pending" | "interrupted" => {}
                    "succeeded" if row.get::<Option<i64>, _>("response_status") == Some(200) => {
                        let actual: serde_json::Value = serde_json::from_str(
                            row.get::<Option<&str>, _>("response_body")
                                .ok_or(CleanupError::RecordInvalid)?,
                        )
                        .map_err(|_| CleanupError::RecordInvalid)?;
                        if Some(actual) != result {
                            return Err(CleanupError::RecordInvalid);
                        }
                    }
                    _ => return Err(CleanupError::RecordInvalid),
                }
            }
            _ => return Err(CleanupError::RecordInvalid),
        }
    }
    Ok(())
}

pub async fn pending_operation_count(database: &Database) -> Result<usize, CleanupError> {
    validate_operations(database).await?;
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM image_cleanup_operations o JOIN idempotency_records p ON p.operation_id=o.operation_id WHERE p.status IN ('pending','interrupted')").fetch_one(database.pool()).await?;
    usize::try_from(count).map_err(|_| CleanupError::RecordInvalid)
}

pub(crate) async fn terminal_result(
    database: &Database,
    operation: Uuid,
    plan: &[ImageCandidate],
    hash: &[u8],
) -> Result<Option<serde_json::Value>, CleanupError> {
    let rows=sqlx::query("SELECT ordinal,image_id,status FROM image_cleanup_items WHERE operation_id=? ORDER BY ordinal").bind(operation.to_string()).fetch_all(database.pool()).await?;
    if rows.len() != plan.len() {
        return Err(CleanupError::RecordInvalid);
    }
    let mut items = Vec::new();
    let mut pending = false;
    let mut retained = false;
    for (ordinal, (row, candidate)) in rows.iter().zip(plan).enumerate() {
        let status: &str = row.get("status");
        if row.get::<i64, _>("ordinal") != ordinal as i64
            || row.get::<String, _>("image_id") != candidate.image_id.as_str()
            || !matches!(status, "planned" | "started" | "removed" | "retained")
        {
            return Err(CleanupError::RecordInvalid);
        }
        pending |= matches!(status, "planned" | "started");
        retained |= status == "retained";
        items.push(serde_json::json!({"image_id":candidate.image_id,"status":status}));
    }
    Ok((!pending).then(||serde_json::json!({"operation_id":operation,"plan_hash":crate::app_store::cleanup::encode_hex(hash),"status":if retained{"completed_with_failures"}else{"completed"},"items":items,"idempotency_replayed":false})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{docker::models::ImageRecord, registry::ManifestDescriptor};
    #[test]
    fn image_cleanup_classic_and_containerd_use_the_same_identity_and_platform() {
        let manifest = format!("sha256:{}", "a".repeat(64));
        let config = format!("sha256:{}", "b".repeat(64));
        let record = CleanedReleaseRecord {
            manifest_digest: manifest.clone(),
            local_image_id: config.clone(),
            platform_os: "linux".into(),
            platform_architecture: "amd64".into(),
            platform_variant: None,
        };
        let mut observed = CleanupImage {
            image: ImageRecord {
                id: config.clone(),
                manifest_descriptor: None,
                repo_digests: vec![format!("example/app@{manifest}")],
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            reported_size_bytes: 10,
            repo_tags: vec![],
        };
        assert!(matches_inspect(&record, &observed).unwrap());
        observed.image.repo_digests.clear();
        assert!(!matches_inspect(&record, &observed).unwrap());
        observed.image.id = manifest.clone();
        observed.image.manifest_descriptor = Some(ManifestDescriptor {
            digest: Some(manifest),
            os: Some("linux".into()),
            architecture: Some("amd64".into()),
            variant: None,
        });
        assert!(matches_inspect(&record, &observed).unwrap());
        observed
            .image
            .manifest_descriptor
            .as_mut()
            .unwrap()
            .architecture = None;
        assert!(!matches_inspect(&record, &observed).unwrap());
        observed.image.id = "tag:latest".into();
        assert!(matches_inspect(&record, &observed).is_err());
    }
}
