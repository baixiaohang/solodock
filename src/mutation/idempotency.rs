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
