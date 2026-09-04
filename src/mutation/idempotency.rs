use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    db::{Database, DbError, format_time},
    registry::CredentialStore,
    security::permissions::{check_private, ensure_private_directory},
};

#[derive(Clone)]
pub struct IdempotencyService {
    database: Database,
    key: std::sync::Arc<KeyMaterial>,
    #[cfg(test)]
    fail_next_effect_marker: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    gc_test_gate: std::sync::Arc<std::sync::Mutex<Option<GcTestGate>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct GcTestGate {
    candidates_selected: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct KeyMaterial(Vec<u8>);

pub enum ClaimResult {
    New(Uuid),
    Resume(Uuid),
    Replay {
        operation_id: Uuid,
        status: u16,
        body: String,
    },
}

#[derive(Clone, Debug)]
pub struct EffectMarker {
    pub phase: String,
    pub pre_container_id: Option<String>,
    pub pre_started_at: Option<String>,
    pub post_container_id: Option<String>,
}

impl IdempotencyService {
    /// Reads a completed response without acquiring or changing an operation.
    /// This lets callers replay before consulting mutable application facts.
    pub async fn completed(
        &self,
        route: &str,
        raw_key: &str,
        request_hmac: &[u8],
    ) -> Result<Option<ClaimResult>, IdempotencyError> {
        self.completed_for_actor("admin", route, raw_key, request_hmac)
            .await
    }

    pub async fn completed_for_actor(
        &self,
        actor: &str,
        route: &str,
        raw_key: &str,
        request_hmac: &[u8],
    ) -> Result<Option<ClaimResult>, IdempotencyError> {
        if !matches!(actor, "admin" | "system") {
            return Err(IdempotencyError::KeyInvalid);
        }
        Self::validate_key(raw_key)?;
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let row = sqlx::query("SELECT request_hmac, operation_id, status, response_status, response_body FROM idempotency_records WHERE actor=? AND route=? AND key_hmac=?")
            .bind(actor)
            .bind(route)
            .bind(key_hmac)
            .fetch_optional(self.database.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored: Vec<u8> = row.get(0);
        if stored != request_hmac {
            return Err(IdempotencyError::Reused);
        }
        if !matches!(row.get::<String, _>(2).as_str(), "succeeded" | "failed") {
            return Ok(None);
        }
        Ok(Some(ClaimResult::Replay {
            operation_id: row
                .get::<String, _>(1)
                .parse()
                .map_err(|_| IdempotencyError::RecordInvalid)?,
            status: row.get::<i64, _>(3) as u16,
            body: row.get(4),
        }))
    }

    pub fn initialize(
        database: Database,
        state_directory: &Path,
    ) -> Result<Self, IdempotencyError> {
        let secrets = state_directory.join("secrets");
        ensure_private_directory(&secrets)?;
        let path = secrets.join("idempotency.key");
        let key = match fs::symlink_metadata(&path) {
            Ok(_) => {
                check_private(&path, false)?;
                fs::read(&path)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut key = vec![0u8; 32];
                getrandom::fill(&mut key).map_err(|_| IdempotencyError::Random)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(&key)?;
                file.sync_all()?;
                super_sync(&secrets)?;
                key
            }
            Err(error) => return Err(error.into()),
        };
        if key.len() != 32 {
            return Err(IdempotencyError::KeyInvalid);
        }
        Ok(Self {
            database,
            key: std::sync::Arc::new(KeyMaterial(key)),
            #[cfg(test)]
            fail_next_effect_marker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            gc_test_gate: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    }

    pub fn validate_key(value: &str) -> Result<(), IdempotencyError> {
        if !(16..=128).contains(&value.len())
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(IdempotencyError::KeyInvalid);
        }
        Ok(())
    }

    pub fn fingerprint(&self, canonical: &[u8]) -> Vec<u8> {
        hmac(&self.key.0, canonical)
    }

    pub fn integrity_key(&self) -> Vec<u8> {
        self.fingerprint(b"config")
    }

    pub async fn interrupt_pending(&self) -> Result<(), IdempotencyError> {
        let now = format_time(OffsetDateTime::now_utc())?;
        sqlx::query("UPDATE idempotency_records SET status='interrupted', updated_at=? WHERE status='pending'").bind(now).execute(self.database.pool()).await?;
        Ok(())
    }

    pub async fn finalize_succeeded_tombstones(
        &self,
        store: &crate::app_store::AppStore,
    ) -> Result<(), IdempotencyError> {
        for (app_id, operation_id) in self.succeeded_app_tombstones(store).await? {
            store.finalize_tombstone(app_id, operation_id)?;
        }
        Ok(())
    }

    pub async fn succeeded_app_tombstones(
        &self,
        store: &crate::app_store::AppStore,
    ) -> Result<Vec<(Uuid, Uuid)>, IdempotencyError> {
        let mut succeeded = Vec::new();
        for (app_id, operation_id) in store.tombstones()? {
            let route = format!("/api/v1/apps/{app_id}");
            let row = sqlx::query("SELECT status,response_status,response_body FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                .bind(route)
                .bind(operation_id.to_string())
                .fetch_optional(self.database.pool())
                .await?;
            let Some(row) = row else {
                return Err(IdempotencyError::RecordInvalid);
            };
            let status: String = row.get(0);
            if matches!(status.as_str(), "pending" | "interrupted") {
                continue;
            }
            let response_status: Option<i64> = row.get(1);
            let response_body: Option<String> = row.get(2);
            let valid = status == "succeeded"
                && response_status == Some(200)
                && response_body
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .is_some_and(|value| {
                        value.get("app_id").and_then(|value| value.as_str())
                            == Some(app_id.to_string().as_str())
                            && value.get("unregistered").and_then(|value| value.as_bool())
                                == Some(true)
                    });
            if !valid {
                return Err(IdempotencyError::RecordInvalid);
            }
            succeeded.push((app_id, operation_id));
        }
        Ok(succeeded)
    }

    /// Finalizes credential tombstones only after the exact deletion response
    /// is durable. Interrupted operations remain resumable; malformed or
    /// unowned markers fail closed instead of becoming a broad startup cleanup.
    pub async fn finalize_succeeded_credential_tombstones(
        &self,
        store: &CredentialStore,
    ) -> Result<(), IdempotencyError> {
        for (credential_id, operation_id) in store.tombstones()? {
            let route = format!("/api/v1/registry-credentials/{credential_id}");
            let row = sqlx::query(
                "SELECT status,response_status,response_body FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?",
            )
            .bind(&route)
            .bind(operation_id.to_string())
            .fetch_optional(self.database.pool())
            .await?;
            let Some(row) = row else {
                return Err(IdempotencyError::RecordInvalid);
            };
            match row.get::<String, _>(0).as_str() {
                "pending" | "interrupted" => continue,
                "succeeded" => {
                    let status = row.get::<Option<i64>, _>(1);
                    let body = row.get::<Option<String>, _>(2);
                    let valid = status == Some(200)
                        && body
                            .as_deref()
                            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                            .is_some_and(|value| {
                                value.get("id").and_then(|value| value.as_str())
                                    == Some(credential_id.to_string().as_str())
                                    && value.get("deleted").and_then(|value| value.as_bool())
                                        == Some(true)
                            });
                    if !valid {
                        return Err(IdempotencyError::RecordInvalid);
                    }
                    store.finalize_tombstone(credential_id, operation_id)?;
                }
                _ => return Err(IdempotencyError::RecordInvalid),
            }
        }
        Ok(())
    }

    pub async fn finalize_succeeded_webhook_revisions(
        &self,
        store: &crate::webhook::WebhookStore,
    ) -> Result<(), IdempotencyError> {
        let inventory = store.recovery_inventory()?;
        for app in inventory.apps {
            let stale = app
                .revisions
                .iter()
                .filter(|revision| !revision.current)
                .collect::<Vec<_>>();
            if stale.is_empty() {
                continue;
            }
            let route = format!("/api/v1/apps/{}/webhook", app.app_id);
            if let Some(metadata) = app.metadata {
                let row = sqlx::query("SELECT rowid,status,response_status,response_body FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                    .bind(&route)
                    .bind(metadata.last_operation_id.to_string())
                    .fetch_optional(self.database.pool())
                    .await?;
                let Some(row) = row else {
                    // A v0.1.0 instance may already have collected the proof
                    // for its long-lived current metadata. If a later rotate
                    // published its canonical revision but crashed before
                    // metadata publication, that exact interrupted operation
                    // must remain resumable. Skip all cleanup for the app;
                    // without either such an operation or transition authority
                    // the stale state remains invalid and fail-closed.
                    let mut resumable_operation_owned_revision = false;
                    for revision in &stale {
                        let own = sqlx::query("SELECT status FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                            .bind(&route)
                            .bind(revision.operation_id.to_string())
                            .fetch_optional(self.database.pool())
                            .await?;
                        if own.is_some_and(|own| {
                            matches!(own.get::<String, _>(0).as_str(), "pending" | "interrupted")
                        }) {
                            resumable_operation_owned_revision = true;
                        }
                    }
                    if resumable_operation_owned_revision {
                        continue;
                    }
                    return Err(IdempotencyError::RecordInvalid);
                };
                let transition_rowid = row.get::<i64, _>(0);
                let status = row.get::<String, _>(1);
                if matches!(status.as_str(), "pending" | "interrupted") {
                    continue;
                }
                let response_status = row.get::<Option<i64>, _>(2);
                let response_body = row.get::<Option<String>, _>(3);
                if status != "succeeded"
                    || response_status != Some(200)
                    || !response_body
                        .as_deref()
                        .is_some_and(|body| webhook_transition_response_matches(body, &metadata))
                {
                    return Err(IdempotencyError::RecordInvalid);
                }
                let mut future_operation_owned_revision = false;
                for revision in &stale {
                    let row = sqlx::query("SELECT rowid,status FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                        .bind(&route)
                        .bind(revision.operation_id.to_string())
                        .fetch_optional(self.database.pool())
                        .await?;
                    let Some(row) = row else {
                        // A pre-v0.1.1 creation proof may already have been
                        // collected. The newer signed transition remains the
                        // cleanup authority for that historical revision.
                        continue;
                    };
                    let operation_rowid = row.get::<i64, _>(0);
                    if operation_rowid < transition_rowid {
                        continue;
                    }
                    if operation_rowid == transition_rowid {
                        return Err(IdempotencyError::RecordInvalid);
                    }
                    match row.get::<String, _>(1).as_str() {
                        "pending" | "interrupted" => {
                            future_operation_owned_revision = true;
                        }
                        _ => return Err(IdempotencyError::RecordInvalid),
                    }
                }
                if future_operation_owned_revision {
                    // During rotate, a later canonical directory is visible
                    // before its metadata. The older current transition must
                    // not authorize broad cleanup of that future artifact.
                    continue;
                }
                store.cleanup_unreferenced(app.app_id)?;
            } else {
                // A canonical revision can become visible immediately before
                // configure publishes metadata. It remains owned by its exact
                // operation until that request resumes; no other proof may be
                // used to infer that the revision is disposable.
                for revision in stale {
                    let row = sqlx::query("SELECT status FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                        .bind(&route)
                        .bind(revision.operation_id.to_string())
                        .fetch_optional(self.database.pool())
                        .await?;
                    match row.map(|row| row.get::<String, _>(0)) {
                        Some(status) if matches!(status.as_str(), "pending" | "interrupted") => {}
                        _ => return Err(IdempotencyError::RecordInvalid),
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn cleanup_webhook_operation_temps(
        &self,
        store: &crate::webhook::WebhookStore,
    ) -> Result<(), IdempotencyError> {
        for app in store.recovery_inventory()?.apps {
            let route = format!("/api/v1/apps/{}/webhook", app.app_id);
            for operation_id in app.operation_temps {
                let row = sqlx::query("SELECT status FROM idempotency_records WHERE actor='admin' AND route=? AND operation_id=?")
                    .bind(&route)
                    .bind(operation_id.to_string())
                    .fetch_optional(self.database.pool())
                    .await?;
                match row.map(|row| row.get::<String, _>(0)) {
                    Some(status)
                        if matches!(status.as_str(), "interrupted" | "failed" | "succeeded") =>
                    {
                        store.discard_operation_temp(app.app_id, operation_id)?;
                    }
                    _ => return Err(IdempotencyError::RecordInvalid),
                }
            }
        }
        Ok(())
    }

    pub async fn claim(
        &self,
        route: &str,
        raw_key: &str,
        request_hmac: &[u8],
        request_id: Uuid,
    ) -> Result<ClaimResult, IdempotencyError> {
        Self::validate_key(raw_key)?;
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let now = format_time(OffsetDateTime::now_utc())?;
        let operation_id = Uuid::new_v4();
        let mut tx = self.database.pool().begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,created_at,updated_at) VALUES ('admin',?,?,?,?, 'pending',?,?)")
            .bind(route).bind(&key_hmac).bind(request_hmac).bind(operation_id.to_string()).bind(&now).bind(&now).execute(&mut *tx).await?.rows_affected();
        if inserted == 1 {
            sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('admin',?,?,NULL,NULL,'attempt','{}',?)")
                .bind(request_id.to_string()).bind(route).bind(&now).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(ClaimResult::New(operation_id));
        }
        let row = sqlx::query("SELECT request_hmac, operation_id, status, response_status, response_body FROM idempotency_records WHERE actor='admin' AND route=? AND key_hmac=?").bind(route).bind(&key_hmac).fetch_one(&mut *tx).await?;
        let stored: Vec<u8> = row.get(0);
        if stored != request_hmac {
            return Err(IdempotencyError::Reused);
        }
        let operation_id = row
            .get::<String, _>(1)
            .parse()
            .map_err(|_| IdempotencyError::RecordInvalid)?;
        match row.get::<String, _>(2).as_str() {
            "succeeded" | "failed" => Ok(ClaimResult::Replay {
                operation_id,
                status: row.get::<i64, _>(3) as u16,
                body: row.get(4),
            }),
            "interrupted" => {
                sqlx::query("UPDATE idempotency_records SET status='pending', updated_at=? WHERE actor='admin' AND route=? AND key_hmac=?").bind(now).bind(route).bind(key_hmac).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(ClaimResult::Resume(operation_id))
            }
            "pending" => Err(IdempotencyError::InProgress),
            _ => Err(IdempotencyError::RecordInvalid),
        }
    }

    /// Builds a fresh, complete inventory before any terminal replay proof is collected.
    /// A failure from any store aborts the inventory so callers cannot mistake it for an empty set.
    pub fn protected_operation_ids(
        &self,
        apps: &crate::app_store::AppStore,
        credentials: &CredentialStore,
        webhooks: &crate::webhook::WebhookStore,
    ) -> Result<HashSet<Uuid>, IdempotencyError> {
        let mut protected = HashSet::new();
        protected.extend(
            apps.tombstones()?
                .into_iter()
                .map(|(_, operation_id)| operation_id),
        );
        protected.extend(
            credentials
                .tombstones()?
                .into_iter()
                .map(|(_, operation_id)| operation_id),
        );
        protected.extend(
            webhooks
                .recovery_inventory()?
                .apps
                .into_iter()
                .flat_map(|app| {
                    let has_stale = app.revisions.iter().any(|revision| !revision.current);
                    let authority = has_stale
                        .then(|| app.metadata.map(|metadata| metadata.last_operation_id))
                        .flatten();
                    app.revisions
                        .into_iter()
                        .map(|revision| revision.operation_id)
                        .chain(app.operation_temps)
                        .chain(authority)
                        .collect::<Vec<_>>()
                }),
        );
        Ok(protected)
    }

    /// Deletes at most 100 expired terminal replay records that have no filesystem owner.
    pub async fn gc_terminal_records(
        &self,
        protected: &HashSet<Uuid>,
    ) -> Result<u64, IdempotencyError> {
        let retention_cutoff = format_time(OffsetDateTime::now_utc() - time::Duration::hours(24))?;
        let candidates = sqlx::query(
            "SELECT rowid,operation_id FROM idempotency_records WHERE status IN ('succeeded','failed') AND updated_at < ? ORDER BY updated_at",
        )
        .bind(&retention_cutoff)
        .fetch_all(self.database.pool())
        .await?;
        let rows = candidates
            .into_iter()
            .filter_map(|row| {
                let operation_id = row.get::<String, _>(1).parse::<Uuid>().ok()?;
                (!protected.contains(&operation_id)).then(|| (row.get::<i64, _>(0), operation_id))
            })
            .take(100)
            .collect::<Vec<_>>();

        #[cfg(test)]
        let test_gate = {
            self.gc_test_gate
                .lock()
                .expect("GC test gate is not poisoned")
                .take()
        };
        #[cfg(test)]
        if let Some(gate) = test_gate {
            gate.candidates_selected.notify_one();
            gate.resume.notified().await;
        }

        let mut tx = self.database.pool().begin().await?;
        let mut deleted = 0;
        for (rowid, operation_id) in rows {
            deleted += sqlx::query("DELETE FROM idempotency_records WHERE rowid=? AND operation_id=? AND status IN ('succeeded','failed') AND updated_at < ?")
                .bind(rowid)
                .bind(operation_id.to_string())
                .bind(&retention_cutoff)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(deleted)
    }

    pub async fn gc_with_artifact_inventory(
        &self,
        apps: &crate::app_store::AppStore,
        credentials: &CredentialStore,
        webhooks: &crate::webhook::WebhookStore,
    ) -> Result<u64, IdempotencyError> {
        let protected = self.protected_operation_ids(apps, credentials, webhooks)?;
        self.gc_terminal_records(&protected).await
    }

    #[cfg(test)]
    fn install_gc_test_gate(
        &self,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let selected = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .gc_test_gate
            .lock()
            .expect("GC test gate is not poisoned") = Some(GcTestGate {
            candidates_selected: selected.clone(),
            resume: resume.clone(),
        });
        (selected, resume)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn claim_deployment(
        &self,
        route: &str,
        raw_key: &str,
        request_hmac: &[u8],
        request_id: Uuid,
        app_id: Uuid,
        trigger: &str,
        requested_revision: Uuid,
        from_release_id: Option<Uuid>,
        expected_pending_release_id: Option<Uuid>,
        expected_actual_release_id: Option<Uuid>,
        expected_actual_container_id: Option<&str>,
        rollback_target_release_id: Option<Uuid>,
        rollback_of_deployment_id: Option<Uuid>,
        scheduled: Option<&crate::deploy::ScheduledResolvedTarget>,
    ) -> Result<ClaimResult, IdempotencyError> {
        Self::validate_key(raw_key)?;
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let now = format_time(OffsetDateTime::now_utc())?;
        let operation_id = Uuid::new_v4();
        let actor = if trigger == "poll" { "system" } else { "admin" };
        let mut tx = self.database.pool().begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,created_at,updated_at) VALUES (?,?,?,?,?,'pending',?,?)")
            .bind(actor).bind(route).bind(&key_hmac).bind(request_hmac).bind(operation_id.to_string()).bind(&now).bind(&now).execute(&mut *tx).await?.rows_affected();
        if inserted == 1 {
            let deployment = sqlx::query("INSERT INTO deployments (id,app_id,trigger,requested_revision,from_release_id,expected_pending_release_id,expected_actual_release_id,expected_actual_container_id,rollback_target_release_id,rollback_of_deployment_id,scheduled_source_image_ref,scheduled_source_descriptor_digest,scheduled_manifest_digest,scheduled_index_digest,scheduled_platform_os,scheduled_platform_architecture,scheduled_platform_variant,scheduled_local_image_id,scheduled_repository,scheduled_target_key,poll_generation,status,phase,request_id,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,'queued','queued',?,?,?)")
                .bind(operation_id.to_string()).bind(app_id.to_string()).bind(trigger).bind(requested_revision.to_string())
                .bind(from_release_id.map(|v|v.to_string()))
                .bind(expected_pending_release_id.map(|v|v.to_string()))
                .bind(expected_actual_release_id.map(|v|v.to_string()))
                .bind(expected_actual_container_id)
                .bind(rollback_target_release_id.map(|v|v.to_string())).bind(rollback_of_deployment_id.map(|v|v.to_string()))
                .bind(scheduled.map(|value| value.image.source_image_ref.as_str()))
                .bind(scheduled.map(|value| value.image.source_descriptor_digest.as_str()))
                .bind(scheduled.map(|value| value.image.manifest_digest.as_str()))
                .bind(scheduled.and_then(|value| value.image.index_digest.as_deref()))
                .bind(scheduled.map(|value| value.image.platform.os.as_str()))
                .bind(scheduled.map(|value| value.image.platform.architecture.as_str()))
                .bind(scheduled.and_then(|value| value.image.platform.variant.as_deref()))
                .bind(scheduled.map(|value| value.image.local_image_id.as_str()))
                .bind(scheduled.map(|value| value.image.repository.as_str()))
                .bind(scheduled.map(|value| value.target_key.as_str()))
                .bind(scheduled.map(|value| value.generation.as_str()))
                .bind(request_id.to_string()).bind(&now).bind(&now).execute(&mut *tx).await;
            if let Err(error) = deployment {
                if error
                    .as_database_error()
                    .is_some_and(|value| value.is_unique_violation())
                {
                    return Err(IdempotencyError::InProgress);
                }
                return Err(error.into());
            }
            sqlx::query("INSERT INTO deployment_transitions (deployment_id,seq,phase,result,created_at) VALUES (?,1,'queued','scheduled',?)")
                .bind(operation_id.to_string()).bind(&now).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES (?,?,?, 'deployment',?,'attempt','{}',?)")
                .bind(actor).bind(request_id.to_string()).bind(route).bind(operation_id.to_string()).bind(&now).execute(&mut *tx).await?;
            let response_body = serde_json::json!({
                "deployment_id": operation_id,
                "status": "queued",
                "idempotency_replayed": false,
                "detail_url": format!("/api/v1/deployments/{operation_id}")
            })
            .to_string();
            sqlx::query("UPDATE idempotency_records SET status='succeeded',response_status=202,response_body=?,updated_at=? WHERE actor=? AND route=? AND key_hmac=? AND operation_id=?")
                .bind(response_body).bind(&now).bind(actor).bind(route).bind(&key_hmac).bind(operation_id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(ClaimResult::New(operation_id));
        }
        let row = sqlx::query("SELECT request_hmac, operation_id, status, response_status, response_body FROM idempotency_records WHERE actor=? AND route=? AND key_hmac=?").bind(actor).bind(route).bind(&key_hmac).fetch_one(&mut *tx).await?;
        let stored: Vec<u8> = row.get(0);
        if stored != request_hmac {
            return Err(IdempotencyError::Reused);
        }
        let operation_id = row
            .get::<String, _>(1)
            .parse()
            .map_err(|_| IdempotencyError::RecordInvalid)?;
        match row.get::<String, _>(2).as_str() {
            "succeeded" | "failed" => Ok(ClaimResult::Replay {
                operation_id,
                status: row.get::<i64, _>(3) as u16,
                body: row.get(4),
            }),
            "interrupted" => Err(IdempotencyError::InProgress),
            "pending" => Err(IdempotencyError::InProgress),
            _ => Err(IdempotencyError::RecordInvalid),
        }
    }

    pub async fn finish(
        &self,
        route: &str,
        raw_key: &str,
        status: u16,
        body: &str,
        error_code: Option<&str>,
        request_id: Uuid,
    ) -> Result<(), IdempotencyError> {
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let now = format_time(OffsetDateTime::now_utc())?;
        let result = if status < 400 { "succeeded" } else { "failed" };
        let mut tx = self.database.pool().begin().await?;
        sqlx::query("UPDATE idempotency_records SET status=?,response_status=?,response_body=?,error_code=?,updated_at=? WHERE actor='admin' AND route=? AND key_hmac=?")
            .bind(result).bind(i64::from(status)).bind(body).bind(error_code).bind(&now).bind(route).bind(&key_hmac).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('admin',?,?,NULL,NULL,?,'{}',?)")
            .bind(request_id.to_string()).bind(route).bind(result).bind(&now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn effect_marker(
        &self,
        route: &str,
        raw_key: &str,
    ) -> Result<Option<EffectMarker>, IdempotencyError> {
        #[cfg(test)]
        if self
            .fail_next_effect_marker
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(IdempotencyError::RecordInvalid);
        }
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let row = sqlx::query("SELECT effect_phase,pre_container_id,pre_started_at,post_container_id FROM idempotency_records WHERE actor='admin' AND route=? AND key_hmac=?")
            .bind(route)
            .bind(key_hmac)
            .fetch_optional(self.database.pool())
            .await?;
        Ok(row.and_then(|row| {
            row.get::<Option<String>, _>(0).map(|phase| EffectMarker {
                phase,
                pre_container_id: row.get(1),
                pre_started_at: row.get(2),
                post_container_id: row.get(3),
            })
        }))
    }

    #[cfg(test)]
    pub(crate) fn fail_next_effect_marker_for_test(&self) {
        self.fail_next_effect_marker
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn mark_effect_observed(
        &self,
        route: &str,
        raw_key: &str,
        container_id: &str,
    ) -> Result<(), IdempotencyError> {
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let changed = sqlx::query("UPDATE idempotency_records SET post_container_id=?,updated_at=? WHERE actor='admin' AND route=? AND key_hmac=? AND status='pending' AND effect_phase='started' AND pre_container_id IS NULL AND (post_container_id IS NULL OR post_container_id=?)")
            .bind(container_id)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(route)
            .bind(key_hmac)
            .bind(container_id)
            .execute(self.database.pool())
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(IdempotencyError::RecordInvalid);
        }
        Ok(())
    }

    pub async fn mark_effect_started(
        &self,
        route: &str,
        raw_key: &str,
        container_id: Option<&str>,
        started_at: Option<&str>,
    ) -> Result<(), IdempotencyError> {
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let changed = sqlx::query("UPDATE idempotency_records SET effect_phase='started',pre_container_id=?,pre_started_at=?,updated_at=? WHERE actor='admin' AND route=? AND key_hmac=? AND status='pending' AND effect_phase IS NULL")
            .bind(container_id)
            .bind(started_at)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(route)
            .bind(key_hmac)
            .execute(self.database.pool())
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(IdempotencyError::RecordInvalid);
        }
        Ok(())
    }

    pub async fn mark_effect_completed(
        &self,
        route: &str,
        raw_key: &str,
    ) -> Result<(), IdempotencyError> {
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let changed = sqlx::query("UPDATE idempotency_records SET effect_phase='completed',updated_at=? WHERE actor='admin' AND route=? AND key_hmac=? AND status='pending' AND effect_phase='started'")
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(route)
            .bind(key_hmac)
            .execute(self.database.pool())
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(IdempotencyError::RecordInvalid);
        }
        Ok(())
    }

    pub async fn mark_interrupted(
        &self,
        route: &str,
        raw_key: &str,
        request_id: Uuid,
    ) -> Result<(), IdempotencyError> {
        let key_hmac = hmac(&self.key.0, raw_key.as_bytes());
        let now = format_time(OffsetDateTime::now_utc())?;
        let mut tx = self.database.pool().begin().await?;
        sqlx::query("UPDATE idempotency_records SET status='interrupted',updated_at=? WHERE actor='admin' AND route=? AND key_hmac=? AND status='pending'")
            .bind(&now)
            .bind(route)
            .bind(key_hmac)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('admin',?,?,NULL,NULL,'interrupted','{}',?)")
            .bind(request_id.to_string())
            .bind(route)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

fn webhook_transition_response_matches(
    body: &str,
    metadata: &crate::webhook::WebhookMetadata,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    value.get("configured").and_then(|value| value.as_bool()) == Some(metadata.enabled)
        && value
            .get("metadata_revision")
            .and_then(|value| value.as_str())
            == Some(metadata.metadata_revision.to_string().as_str())
        && match metadata.secret_revision {
            Some(revision) => {
                value
                    .get("secret_revision")
                    .and_then(|value| value.as_str())
                    == Some(revision.to_string().as_str())
            }
            None => value
                .get("secret_revision")
                .is_some_and(serde_json::Value::is_null),
        }
}

fn hmac(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("32-byte HMAC key");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}
fn super_sync(path: &Path) -> Result<(), std::io::Error> {
    fs::File::open(path)?.sync_all()
}

#[derive(Debug, thiserror::Error)]
pub enum IdempotencyError {
    #[error("idempotency key is required")]
    KeyRequired,
    #[error("idempotency key is invalid")]
    KeyInvalid,
    #[error("idempotency key was reused")]
    Reused,
    #[error("idempotency operation is in progress")]
    InProgress,
    #[error("idempotency record is invalid")]
    RecordInvalid,
    #[error("secure random generation failed")]
    Random,
    #[error(transparent)]
    Permission(#[from] crate::security::permissions::PermissionError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("idempotency key I/O failed")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] crate::app_store::StoreError),
}

impl From<sqlx::Error> for IdempotencyError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DbError::from(error))
    }
}

#[cfg(test)]
mod deployment_tests {
    use super::*;
    use crate::{
        app_store::AppStore,
        deploy::ScheduledResolvedTarget,
        domain::{DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy, normalize_draft},
        mutation::AppMutationCoordinator,
        registry::{CredentialStore, Platform, ResolvedImage},
        security::secret::SecretValue,
        webhook::WebhookStore,
    };
    use std::os::unix::fs::PermissionsExt;

    async fn insert_old_record(
        database: &Database,
        operation_id: Uuid,
        status: &str,
        ordinal: usize,
    ) {
        sqlx::query("INSERT INTO idempotency_records (actor,route,key_hmac,request_hmac,operation_id,status,response_status,response_body,created_at,updated_at) VALUES ('admin',?,?,?,?,?,200,'{}','2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')")
            .bind(format!("/fixture/{ordinal}"))
            .bind(vec![ordinal as u8; 32])
            .bind(vec![ordinal as u8; 32])
            .bind(operation_id.to_string())
            .bind(status)
            .execute(database.pool())
            .await
            .unwrap();
    }

    fn configured_app(apps: &AppStore, key: &[u8], slug: &str) -> Uuid {
        let draft = normalize_draft(
            DraftInput {
                display_name: slug.into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![],
                health: HealthPolicy::default(),
            },
            &ExistingSecrets::default(),
            key,
            &[],
        )
        .unwrap();
        let app_id = Uuid::new_v4();
        apps.create_app(
            app_id,
            slug,
            Uuid::new_v4(),
            Some((Uuid::new_v4(), &draft)),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        app_id
    }

    async fn claim_webhook_operation(
        service: &IdempotencyService,
        app_id: Uuid,
        key: &str,
    ) -> (String, Uuid) {
        let route = format!("/api/v1/apps/{app_id}/webhook");
        let operation = match service
            .claim(&route, key, key.as_bytes(), Uuid::new_v4())
            .await
            .unwrap()
        {
            ClaimResult::New(operation) => operation,
            _ => panic!("webhook operation must be new"),
        };
        (route, operation)
    }

    async fn finish_webhook_operation(
        service: &IdempotencyService,
        route: &str,
        key: &str,
        metadata: &crate::webhook::WebhookMetadata,
    ) {
        let body = serde_json::json!({
            "configured": metadata.enabled,
            "metadata_revision": metadata.metadata_revision,
            "secret_revision": metadata.secret_revision,
        })
        .to_string();
        service
            .finish(route, key, 200, &body, None, Uuid::new_v4())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn gc_keeps_protected_and_nonterminal_records_and_deletes_at_most_one_batch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let protected_id = Uuid::new_v4();
        insert_old_record(&database, protected_id, "succeeded", 0).await;
        insert_old_record(&database, Uuid::new_v4(), "pending", 1).await;
        insert_old_record(&database, Uuid::new_v4(), "interrupted", 2).await;
        for ordinal in 3..108 {
            insert_old_record(&database, Uuid::new_v4(), "failed", ordinal).await;
        }

        let deleted = service
            .gc_terminal_records(&HashSet::from([protected_id]))
            .await
            .unwrap();
        assert_eq!(deleted, 100);
        let protected_status: String =
            sqlx::query_scalar("SELECT status FROM idempotency_records WHERE operation_id=?")
                .bind(protected_id.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(protected_status, "succeeded");
        let nonterminal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM idempotency_records WHERE status IN ('pending','interrupted')",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(nonterminal, 2);
    }

    #[tokio::test]
    async fn fresh_inventory_covers_every_finalizer_artifact_kind() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let key = service.integrity_key();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), key.clone(), vec![]).unwrap();
        let credentials =
            CredentialStore::initialize(root.path().join("registry-credentials"), key.clone())
                .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), key.clone());

        let deleted_app = apps
            .create_app(
                Uuid::new_v4(),
                "deleted-app",
                Uuid::new_v4(),
                None,
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let app_operation = Uuid::new_v4();
        apps.tombstone(deleted_app.id, app_operation).unwrap();

        let credential_id = Uuid::new_v4();
        credentials
            .create(
                credential_id,
                Uuid::new_v4(),
                "registry.example",
                "fixture",
                &SecretValue::new("credential-secret".into()),
            )
            .unwrap();
        let credential_operation = Uuid::new_v4();
        credentials
            .tombstone(credential_id, credential_operation)
            .unwrap();

        let webhook_app = configured_app(&apps, &key, "webhook-app");
        let stale_revision_operation = Uuid::new_v4();
        let first = webhooks
            .configure(
                webhook_app,
                None,
                stale_revision_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        let current_operation = Uuid::new_v4();
        let current = webhooks
            .configure(
                webhook_app,
                Some(first.metadata_revision),
                current_operation,
                &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into()),
            )
            .unwrap();
        let temp_operation = Uuid::new_v4();
        let temporary = apps
            .app_directory(webhook_app)
            .join("webhook-secret-revisions")
            .join(format!(".solodock-webhook-tmp-{}", temp_operation.simple()));
        std::fs::create_dir(&temporary).unwrap();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)).unwrap();

        let protected = service
            .protected_operation_ids(&apps, &credentials, &webhooks)
            .unwrap();
        assert!(protected.contains(&app_operation));
        assert!(protected.contains(&credential_operation));
        assert!(protected.contains(&stale_revision_operation));
        assert!(protected.contains(&current_operation));
        assert!(protected.contains(&current.last_operation_id));
        assert!(protected.contains(&temp_operation));
    }

    #[tokio::test]
    async fn current_webhook_revision_proof_is_retained_with_its_artifact() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "current-proof");
        let (route, operation) =
            claim_webhook_operation(&service, app_id, "current-webhook-proof-0001").await;
        let metadata = webhooks
            .configure(
                app_id,
                None,
                operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &route, "current-webhook-proof-0001", &metadata).await;
        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(operation.to_string())
        .execute(database.pool())
        .await
        .unwrap();
        insert_old_record(&database, Uuid::new_v4(), "failed", 240).await;

        assert_eq!(
            service
                .gc_with_artifact_inventory(&apps, &credentials, &webhooks)
                .await
                .unwrap(),
            1
        );
        let retained: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(operation.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(retained, 1);
    }

    #[tokio::test]
    async fn rotate_transition_proof_cleans_stale_revision_without_old_creation_proof() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "rotate-authority");
        let (first_route, first_operation) =
            claim_webhook_operation(&service, app_id, "webhook-first-proof-00001").await;
        let first = webhooks
            .configure(
                app_id,
                None,
                first_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &first_route, "webhook-first-proof-00001", &first).await;
        sqlx::query("DELETE FROM idempotency_records WHERE operation_id=?")
            .bind(first_operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();

        let (route, transition) =
            claim_webhook_operation(&service, app_id, "webhook-rotate-proof-0001").await;
        let rotated = webhooks
            .configure(
                app_id,
                Some(first.metadata_revision),
                transition,
                &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &route, "webhook-rotate-proof-0001", &rotated).await;
        let stale_path = apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(first_operation.to_string());
        assert!(stale_path.exists());

        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(transition.to_string())
        .execute(database.pool())
        .await
        .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        assert_eq!(
            service
                .gc_with_artifact_inventory(&apps, &credentials, &webhooks)
                .await
                .unwrap(),
            0
        );
        let restarted = IdempotencyService::initialize(database, root.path()).unwrap();
        restarted
            .finalize_succeeded_webhook_revisions(&webhooks)
            .await
            .unwrap();
        assert!(!stale_path.exists());
    }

    #[tokio::test]
    async fn revoke_transition_proof_authorizes_cleanup_but_identity_mismatch_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "revoke-authority");
        let (first_route, first_operation) =
            claim_webhook_operation(&service, app_id, "webhook-revoke-first-001").await;
        let first = webhooks
            .configure(
                app_id,
                None,
                first_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &first_route, "webhook-revoke-first-001", &first).await;
        sqlx::query("DELETE FROM idempotency_records WHERE operation_id=?")
            .bind(first_operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        let (route, transition) =
            claim_webhook_operation(&service, app_id, "webhook-revoke-proof-0001").await;
        let revoked = webhooks
            .revoke(app_id, first.metadata_revision, transition)
            .unwrap();
        service
            .finish(
                &route,
                "webhook-revoke-proof-0001",
                200,
                &serde_json::json!({
                    "configured": false,
                    "metadata_revision": Uuid::new_v4(),
                    "secret_revision": null,
                })
                .to_string(),
                None,
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        let stale_path = apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(first_operation.to_string());
        assert!(
            service
                .finalize_succeeded_webhook_revisions(&webhooks)
                .await
                .is_err()
        );
        assert!(stale_path.exists());

        sqlx::query("UPDATE idempotency_records SET response_body=? WHERE operation_id=?")
            .bind(
                serde_json::json!({
                    "configured": false,
                    "metadata_revision": revoked.metadata_revision,
                    "secret_revision": null,
                })
                .to_string(),
            )
            .bind(transition.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        service
            .finalize_succeeded_webhook_revisions(&webhooks)
            .await
            .unwrap();
        assert!(!stale_path.exists());
    }

    #[tokio::test]
    async fn canonical_revision_without_metadata_keeps_its_exact_interrupted_proof() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "canonical-crash");
        let (route, operation) =
            claim_webhook_operation(&service, app_id, "webhook-crash-window-0001").await;
        webhooks
            .configure(
                app_id,
                None,
                operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        std::fs::remove_file(apps.app_directory(app_id).join("webhook.toml")).unwrap();
        service
            .mark_interrupted(&route, "webhook-crash-window-0001", Uuid::new_v4())
            .await
            .unwrap();
        service
            .finalize_succeeded_webhook_revisions(&webhooks)
            .await
            .unwrap();
        let artifact = apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(operation.to_string());
        assert!(artifact.exists());
        assert_eq!(
            service
                .gc_with_artifact_inventory(&apps, &credentials, &webhooks)
                .await
                .unwrap(),
            0
        );
        sqlx::query("DELETE FROM idempotency_records WHERE operation_id=?")
            .bind(operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        assert!(
            service
                .finalize_succeeded_webhook_revisions(&webhooks)
                .await
                .is_err()
        );
        assert!(artifact.exists());
    }

    #[tokio::test]
    async fn upgrade_missing_current_proof_allows_pre_metadata_rotate_to_resume() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "rotate-crash-window");
        let (route, first_operation) =
            claim_webhook_operation(&service, app_id, "rotate-crash-first-key-01").await;
        let first = webhooks
            .configure(
                app_id,
                None,
                first_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &route, "rotate-crash-first-key-01", &first).await;
        let metadata_path = apps.app_directory(app_id).join("webhook.toml");
        let old_metadata = std::fs::read(&metadata_path).unwrap();
        // v0.1.0 could collect the proof for a webhook that remained current
        // for more than 24 hours before this instance was upgraded.
        sqlx::query("DELETE FROM idempotency_records WHERE operation_id=?")
            .bind(first_operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();

        let rotate_key = "rotate-crash-second-key-01";
        let (_, rotate_operation) = claim_webhook_operation(&service, app_id, rotate_key).await;
        let second_secret = SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into());
        webhooks
            .configure(
                app_id,
                Some(first.metadata_revision),
                rotate_operation,
                &second_secret,
            )
            .unwrap();
        crate::app_store::atomic::AtomicWriter::write(&metadata_path, &old_metadata, 0o600)
            .unwrap();
        service
            .mark_interrupted(&route, rotate_key, Uuid::new_v4())
            .await
            .unwrap();
        let rotate_artifact = apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(rotate_operation.to_string());

        service
            .finalize_succeeded_webhook_revisions(&webhooks)
            .await
            .unwrap();
        assert!(rotate_artifact.exists());
        let protected = service
            .protected_operation_ids(&apps, &credentials, &webhooks)
            .unwrap();
        assert!(protected.contains(&rotate_operation));
        assert!(matches!(
            service
                .claim(&route, rotate_key, rotate_key.as_bytes(), Uuid::new_v4())
                .await
                .unwrap(),
            ClaimResult::Resume(operation) if operation == rotate_operation
        ));
        let resumed = webhooks
            .configure(
                app_id,
                Some(first.metadata_revision),
                rotate_operation,
                &second_secret,
            )
            .unwrap();
        finish_webhook_operation(&service, &route, rotate_key, &resumed).await;
        service
            .finalize_succeeded_webhook_revisions(&webhooks)
            .await
            .unwrap();
        assert!(rotate_artifact.exists());
        assert!(webhooks.load_current(app_id).is_ok());
        assert!(
            !apps
                .app_directory(app_id)
                .join("webhook-secret-revisions")
                .join(first_operation.to_string())
                .exists()
        );
    }

    #[tokio::test]
    async fn webhook_cleanup_filesystem_failure_retains_proof_until_restart_recovery() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "cleanup-failure");
        let (route, first_operation) =
            claim_webhook_operation(&service, app_id, "cleanup-failure-first-001").await;
        let first = webhooks
            .configure(
                app_id,
                None,
                first_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &route, "cleanup-failure-first-001", &first).await;
        // Metadata A is durable, but its response finish was uncertain. The
        // later successful transition must supersede this older interrupted
        // creation proof.
        sqlx::query("UPDATE idempotency_records SET status='interrupted',response_status=NULL,response_body=NULL WHERE operation_id=?")
            .bind(first_operation.to_string())
            .execute(database.pool())
            .await
            .unwrap();
        let transition_key = "cleanup-failure-rotate-01";
        let (_, transition) = claim_webhook_operation(&service, app_id, transition_key).await;
        let rotated = webhooks
            .configure(
                app_id,
                Some(first.metadata_revision),
                transition,
                &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into()),
            )
            .unwrap();
        finish_webhook_operation(&service, &route, transition_key, &rotated).await;
        let stale = apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(first_operation.to_string());
        webhooks.fail_next_cleanup_remove_for_test();
        assert!(
            service
                .finalize_succeeded_webhook_revisions(&webhooks)
                .await
                .is_err()
        );
        assert!(stale.exists());

        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(transition.to_string())
        .execute(database.pool())
        .await
        .unwrap();
        insert_old_record(&database, Uuid::new_v4(), "failed", 246).await;
        assert_eq!(
            service
                .gc_with_artifact_inventory(&apps, &credentials, &webhooks)
                .await
                .unwrap(),
            1
        );
        let proof: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE operation_id=?")
                .bind(transition.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(proof, 1);

        let restarted = IdempotencyService::initialize(database, root.path()).unwrap();
        let restarted_webhooks = WebhookStore::new(apps, restarted.integrity_key());
        restarted
            .finalize_succeeded_webhook_revisions(&restarted_webhooks)
            .await
            .unwrap();
        assert!(!stale.exists());
    }

    #[tokio::test]
    async fn catalog_guard_serializes_inventory_delete_and_webhook_transition() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let credentials = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        let app_id = configured_app(&apps, &service.integrity_key(), "inventory-race");
        let initial_operation = Uuid::new_v4();
        let first = webhooks
            .configure(
                app_id,
                None,
                initial_operation,
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        insert_old_record(&database, Uuid::new_v4(), "failed", 245).await;
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let coordinator = AppMutationCoordinator::new(runtime).unwrap();
        let (selected, resume) = service.install_gc_test_gate();

        let gc_service = service.clone();
        let gc_apps = apps.clone();
        let gc_credentials = credentials.clone();
        let gc_webhooks = webhooks.clone();
        let gc_coordinator = coordinator.clone();
        let gc = tokio::spawn(async move {
            let _catalog = gc_coordinator.catalog_lock().await;
            gc_service
                .gc_with_artifact_inventory(&gc_apps, &gc_credentials, &gc_webhooks)
                .await
                .unwrap()
        });
        selected.notified().await;

        let transitioned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transition_seen = transitioned.clone();
        let transition_store = webhooks.clone();
        let transition_coordinator = coordinator.clone();
        let transition = Uuid::new_v4();
        let rotate = tokio::spawn(async move {
            let _catalog = transition_coordinator.catalog_lock().await;
            transition_store
                .configure(
                    app_id,
                    Some(first.metadata_revision),
                    transition,
                    &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into()),
                )
                .unwrap();
            transition_seen.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::task::yield_now().await;
        assert!(!transitioned.load(std::sync::atomic::Ordering::SeqCst));

        resume.notify_one();
        assert_eq!(gc.await.unwrap(), 1);
        rotate.await.unwrap();
        assert!(transitioned.load(std::sync::atomic::Ordering::SeqCst));
        let protected = service
            .protected_operation_ids(&apps, &credentials, &webhooks)
            .unwrap();
        assert!(protected.contains(&initial_operation));
        assert!(protected.contains(&transition));
    }

    #[tokio::test]
    async fn inventory_failure_prevents_every_gc_delete() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let key = service.integrity_key();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), key.clone(), vec![]).unwrap();
        let credentials =
            CredentialStore::initialize(root.path().join("registry-credentials"), key.clone())
                .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), key);
        insert_old_record(&database, Uuid::new_v4(), "failed", 0).await;
        std::fs::create_dir_all(apps.apps_directory().join(".trash").join("invalid-entry"))
            .unwrap();

        assert!(
            service
                .gc_with_artifact_inventory(&apps, &credentials, &webhooks)
                .await
                .is_err()
        );
        let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(records, 1);
    }

    #[tokio::test]
    async fn startup_discards_only_ledger_owned_webhook_operation_temps() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let app = Uuid::new_v4();
        let app_directory = apps.app_directory(app);
        let revisions = app_directory.join("webhook-secret-revisions");
        std::fs::create_dir(&app_directory).unwrap();
        std::fs::set_permissions(&app_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(&revisions).unwrap();
        std::fs::set_permissions(&revisions, std::fs::Permissions::from_mode(0o700)).unwrap();
        let route = format!("/api/v1/apps/{app}/webhook");
        let key = "webhook-temp-key";
        let operation = match service
            .claim(&route, key, b"fingerprint", Uuid::new_v4())
            .await
            .unwrap()
        {
            ClaimResult::New(value) => value,
            _ => unreachable!(),
        };
        service
            .mark_interrupted(&route, key, Uuid::new_v4())
            .await
            .unwrap();
        let temporary = revisions.join(format!(".solodock-webhook-tmp-{}", operation.simple()));
        std::fs::create_dir(&temporary).unwrap();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let partial = temporary.join("secret");
        std::fs::write(&partial, b"partial").unwrap();
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o600)).unwrap();
        let webhooks = WebhookStore::new(apps, service.integrity_key());

        service
            .cleanup_webhook_operation_temps(&webhooks)
            .await
            .unwrap();
        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn deployment_claim_is_atomic_with_unique_nonterminal_row() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let app = Uuid::new_v4();
        let revision = Uuid::new_v4();
        let first = service
            .claim_deployment(
                "/deploy",
                "abcdefghijklmnop",
                b"first",
                Uuid::new_v4(),
                app,
                "manual",
                revision,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let deployment_id = match first {
            ClaimResult::New(id) => id,
            _ => panic!("first claim creates the durable deployment"),
        };
        let replay = service
            .claim_deployment(
                "/deploy",
                "abcdefghijklmnop",
                b"first",
                Uuid::new_v4(),
                app,
                "manual",
                revision,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        match replay {
            ClaimResult::Replay {
                operation_id,
                status,
                body,
            } => {
                assert_eq!(operation_id, deployment_id);
                assert_eq!(status, 202);
                assert!(body.contains(&deployment_id.to_string()));
            }
            _ => panic!("accepted response must replay across the spawn/crash window"),
        }
        assert!(matches!(
            service
                .claim_deployment(
                    "/deploy",
                    "qrstuvwxyzABCDEF",
                    b"second",
                    Uuid::new_v4(),
                    app,
                    "manual",
                    revision,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None
                )
                .await,
            Err(IdempotencyError::InProgress)
        ));
        let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records")
            .fetch_one(database.pool())
            .await
            .unwrap();
        let deployments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deployments")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!((records, deployments), (1, 1));
    }

    #[tokio::test]
    async fn poll_deployment_records_system_actor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let request_id = Uuid::new_v4();
        let target = ScheduledResolvedTarget {
            image: ResolvedImage {
                source_image_ref: "registry.example/app:stable".to_owned(),
                logical_registry: "registry.example".to_owned(),
                repository: "app".to_owned(),
                source_tag: "stable".to_owned(),
                source_descriptor_digest: format!("sha256:{}", "1".repeat(64)),
                index_digest: None,
                manifest_digest: format!("sha256:{}", "2".repeat(64)),
                runnable_image_ref: format!("registry.example/app@sha256:{}", "2".repeat(64)),
                platform: Platform::canonical("linux", "amd64", None).unwrap(),
                local_image_id: format!("sha256:{}", "3".repeat(64)),
            },
            generation: "generation".to_owned(),
            target_key: "target".to_owned(),
        };
        let claim = service
            .claim_deployment(
                "/internal/poll",
                "poll-abcdefghijkl",
                b"poll-fingerprint",
                request_id,
                Uuid::new_v4(),
                "poll",
                Uuid::new_v4(),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&target),
            )
            .await
            .unwrap();
        assert!(matches!(claim, ClaimResult::New(_)));

        let idempotency_actor: String =
            sqlx::query_scalar("SELECT actor FROM idempotency_records WHERE request_hmac=?")
                .bind(b"poll-fingerprint".as_slice())
                .fetch_one(database.pool())
                .await
                .unwrap();
        let audit_actor: String =
            sqlx::query_scalar("SELECT actor FROM audit_events WHERE request_id=?")
                .bind(request_id.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(idempotency_actor, "system");
        assert_eq!(audit_actor, "system");
    }

    #[tokio::test]
    async fn succeeded_credential_tombstones_retry_remove_and_parent_sync_failures() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        let store = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();

        async fn completed_deletion(
            service: &IdempotencyService,
            store: &CredentialStore,
            id: Uuid,
            key: &str,
        ) -> (Uuid, std::path::PathBuf) {
            store
                .create(
                    id,
                    Uuid::new_v4(),
                    "ghcr.io",
                    &format!("robot-{id}"),
                    &SecretValue::new("credential-secret".into()),
                )
                .unwrap();
            let route = format!("/api/v1/registry-credentials/{id}");
            let fingerprint = service.fingerprint(route.as_bytes());
            let operation = match service
                .claim(&route, key, &fingerprint, Uuid::new_v4())
                .await
                .unwrap()
            {
                ClaimResult::New(value) => value,
                _ => panic!("first deletion claim must be new"),
            };
            let tombstone = store.tombstone(id, operation).unwrap();
            service
                .finish(
                    &route,
                    key,
                    200,
                    &serde_json::json!({"id": id, "deleted": true}).to_string(),
                    None,
                    Uuid::new_v4(),
                )
                .await
                .unwrap();
            (operation, tombstone)
        }

        let first_id = Uuid::new_v4();
        let (first_operation, first_tombstone) =
            completed_deletion(&service, &store, first_id, "credential-delete-remove-0001").await;
        store.fail_next_finalize_remove_for_test();
        assert!(
            service
                .finalize_succeeded_credential_tombstones(&store)
                .await
                .is_err()
        );
        assert!(first_tombstone.exists());

        sqlx::query(
            "UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE operation_id=?",
        )
        .bind(first_operation.to_string())
        .execute(database.pool())
        .await
        .unwrap();
        insert_old_record(&database, Uuid::new_v4(), "failed", 91).await;
        let apps =
            AppStore::initialize_managed(root.path().join("apps"), service.integrity_key(), vec![])
                .unwrap();
        let webhooks = WebhookStore::new(apps.clone(), service.integrity_key());
        assert_eq!(
            service
                .gc_with_artifact_inventory(&apps, &store, &webhooks)
                .await
                .unwrap(),
            1
        );
        let proof_status: String =
            sqlx::query_scalar("SELECT status FROM idempotency_records WHERE operation_id=?")
                .bind(first_operation.to_string())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(proof_status, "succeeded");

        let restarted = IdempotencyService::initialize(database.clone(), root.path()).unwrap();
        restarted
            .finalize_succeeded_credential_tombstones(&store)
            .await
            .unwrap();
        assert!(!first_tombstone.exists());

        let second_id = Uuid::new_v4();
        let (_, second_tombstone) =
            completed_deletion(&service, &store, second_id, "credential-delete-sync-00002").await;
        store.fail_next_finalize_sync_for_test();
        assert!(
            service
                .finalize_succeeded_credential_tombstones(&store)
                .await
                .is_err()
        );
        assert!(
            !second_tombstone.exists(),
            "the visible removal must be repaired by the next parent fsync"
        );
        service
            .finalize_succeeded_credential_tombstones(&store)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn credential_tombstone_without_an_exact_ledger_record_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let service = IdempotencyService::initialize(database, root.path()).unwrap();
        let store = CredentialStore::initialize(
            root.path().join("registry-credentials"),
            service.integrity_key(),
        )
        .unwrap();
        let id = Uuid::new_v4();
        store
            .create(
                id,
                Uuid::new_v4(),
                "ghcr.io",
                "robot",
                &SecretValue::new("credential-secret".into()),
            )
            .unwrap();
        let tombstone = store.tombstone(id, Uuid::new_v4()).unwrap();
        assert!(matches!(
            service
                .finalize_succeeded_credential_tombstones(&store)
                .await,
            Err(IdempotencyError::RecordInvalid)
        ));
        assert!(
            tombstone.exists(),
            "an unowned marker must never be removed"
        );
    }
}
