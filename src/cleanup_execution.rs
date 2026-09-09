//! Shared exact artifact execution for manual confirmation and retention policy authorization.
use crate::{
    api::mutations::M3Services,
    app_store::cleanup::{CleanupArtifact, DetachResult},
    db::format_time,
    storage_cleanup::{CleanupCandidate, CleanupError, CleanupPlan},
};
use sqlx::Row;
use std::collections::HashSet;
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) async fn execute_artifacts(
    m3: &M3Services,
    operation_id: Uuid,
    plan: &CleanupPlan,
    hash: &[u8],
    eligible: Option<&HashSet<CleanupArtifact>>,
) -> Result<crate::api::storage_cleanup::ApplyResponse, CleanupError> {
    let artifacts: Vec<_> = plan
        .candidates
        .iter()
        .map(|candidate| candidate.artifact.clone())
        .collect();
    if m3
        .store
        .prepare_cleanup_tombstone(operation_id, hash, &artifacts)
        .is_err()
    {
        return Err(CleanupError::RecordInvalid);
    }
    if sqlx::query("UPDATE storage_cleanup_operations SET status='running' WHERE operation_id=? AND status='planned'")
        .bind(operation_id.to_string())
        .execute(m3.database.pool())
        .await
        .is_err()
    {
        return Err(CleanupError::RecordInvalid);
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
            Err(_) => return Err(CleanupError::RecordInvalid),
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
        let newly_protected = if let Some(eligible) = eligible {
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
                        return Err(CleanupError::RecordInvalid);
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
                return Err(CleanupError::RecordInvalid);
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
                return Err(CleanupError::RecordInvalid);
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
                    return Err(CleanupError::RecordInvalid);
                }
            }
            Ok(DetachResult::ConfirmedMissing) => {
                return Err(CleanupError::RecordInvalid);
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
                    return Err(CleanupError::RecordInvalid);
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
                    return Err(CleanupError::RecordInvalid);
                }
            }
            Err(_) => return Err(CleanupError::RecordInvalid),
        }
    }
    let items = match crate::storage_cleanup::exact_terminal_items(&m3.database, operation_id, plan)
        .await
    {
        Ok(items) => items,
        Err(_) => return Err(CleanupError::RecordInvalid),
    };
    let has_failures = items.iter().any(|item| item["status"] == "retained");
    let status = if has_failures {
        "completed_with_failures"
    } else {
        "completed"
    };
    let now = format_time(OffsetDateTime::now_utc()).map_err(|_| CleanupError::RecordInvalid)?;
    if sqlx::query("UPDATE storage_cleanup_operations SET status=?,completed_at=? WHERE operation_id=? AND status IN ('planned','running')")
        .bind(status)
        .bind(&now)
        .bind(operation_id.to_string())
        .execute(m3.database.pool())
        .await
        .is_err()
    {
        return Err(CleanupError::RecordInvalid);
    }
    let response_body = crate::api::storage_cleanup::ApplyResponse {
        operation_id,
        plan_hash: crate::app_store::cleanup::encode_hex(hash),
        status,
        items,
        idempotency_replayed: false,
    };
    Ok(response_body)
}
async fn record_detached(
    m3: &M3Services,
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
    m3: &M3Services,
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

pub(crate) async fn execute_images(
    m3: &M3Services,
    docker: &dyn crate::docker::image_cleanup::ImageCleanup,
    operation: Uuid,
    selected: &[crate::image_cleanup::ImageCandidate],
    current: &crate::image_cleanup::ImagePlan,
    hash: &[u8],
) -> Result<serde_json::Value, CleanupError> {
    use crate::docker::image_cleanup::RemoveImageResult;
    // Verify the whole ledger before effects; a mismatched/extra item must never
    // be interpreted as a new authorization to remove an image.
    let rows=sqlx::query("SELECT ordinal,image_id,status FROM image_cleanup_items WHERE operation_id=? ORDER BY ordinal").bind(operation.to_string()).fetch_all(m3.database.pool()).await?;
    if rows.len() != selected.len() {
        return Err(crate::storage_cleanup::CleanupError::RecordInvalid);
    }
    for (ordinal, (row, candidate)) in rows.iter().zip(selected).enumerate() {
        if row.get::<i64, _>("ordinal") != ordinal as i64
            || row.get::<String, _>("image_id") != candidate.image_id.as_str()
            || !matches!(
                row.get::<&str, _>("status"),
                "planned" | "started" | "removed" | "retained"
            )
        {
            return Err(crate::storage_cleanup::CleanupError::RecordInvalid);
        }
    }
    for (ordinal, (row, candidate)) in rows.iter().zip(selected).enumerate() {
        let previous: &str = row.get("status");
        if !matches!(previous, "removed" | "retained") {
            let eligible = current.candidates.iter().any(|value| value == candidate);
            let observed = docker
                .inspect(&candidate.image_id)
                .await
                .map_err(|_| crate::storage_cleanup::CleanupError::RecordInvalid)?;
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
                        .map_err(|_| crate::storage_cleanup::CleanupError::RecordInvalid)? =>
                {
                    "retained"
                }
                Some(_) => {
                    sqlx::query("UPDATE image_cleanup_items SET status='started' WHERE operation_id=? AND ordinal=? AND status IN ('planned','started')").bind(operation.to_string()).bind(ordinal as i64).execute(m3.database.pool()).await?;
                    match docker
                        .remove(&candidate.image_id)
                        .await
                        .map_err(|_| crate::storage_cleanup::CleanupError::RecordInvalid)?
                    {
                        RemoveImageResult::Retained => "retained",
                        RemoveImageResult::Accepted => {
                            if docker
                                .inspect(&candidate.image_id)
                                .await
                                .map_err(|_| crate::storage_cleanup::CleanupError::RecordInvalid)?
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
    crate::image_cleanup::terminal_result(&m3.database, operation, selected, hash)
        .await
        .map_err(|_| crate::storage_cleanup::CleanupError::RecordInvalid)?
        .ok_or(crate::storage_cleanup::CleanupError::RecordInvalid)
}
