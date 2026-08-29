use std::{
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
        let rows = sqlx::query(
            "SELECT route,operation_id FROM idempotency_records WHERE status='succeeded' AND route LIKE '/api/v1/apps/%' LIMIT 100",
        )
        .fetch_all(self.database.pool())
        .await?;
        for row in rows {
            let route: String = row.get(0);
            let Some(app_text) = route.strip_prefix("/api/v1/apps/") else {
                continue;
            };
            if app_text.contains('/') {
                continue;
            }
            let Ok(app_id) = app_text.parse::<Uuid>() else {
                continue;
            };
            let Ok(operation_id) = row.get::<String, _>(1).parse::<Uuid>() else {
                continue;
            };
            let path = store.tombstone_path(app_id, operation_id);
            match fs::symlink_metadata(&path) {
                Ok(_) => store.finalize_tombstone(app_id, operation_id)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
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
        let retention_cutoff = format_time(OffsetDateTime::now_utc() - time::Duration::hours(24))?;
        sqlx::query("DELETE FROM idempotency_records WHERE rowid IN (SELECT rowid FROM idempotency_records WHERE status IN ('succeeded','failed') AND updated_at < ? ORDER BY updated_at LIMIT 100)")
            .bind(retention_cutoff)
            .execute(&mut *tx)
            .await?;
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
        deploy::ScheduledResolvedTarget,
        registry::{CredentialStore, Platform, ResolvedImage},
        security::secret::SecretValue,
    };
    use std::os::unix::fs::PermissionsExt;

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
        let service = IdempotencyService::initialize(database, root.path()).unwrap();
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
        let (_, first_tombstone) =
            completed_deletion(&service, &store, first_id, "credential-delete-remove-0001").await;
        store.fail_next_finalize_remove_for_test();
        assert!(
            service
                .finalize_succeeded_credential_tombstones(&store)
                .await
                .is_err()
        );
        assert!(first_tombstone.exists());
        service
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
