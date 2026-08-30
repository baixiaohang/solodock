use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::{Database, DbError, format_time, parse_time};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PollOutcome {
    Disabled,
    Scheduled,
    Unchanged,
    ConfigPendingManual,
    BusySkipped,
    BlockedDrift,
    BlockedAttention,
    SuppressedFailedTarget,
    RegistryError,
    CredentialError,
    InvalidSource,
    Cancelled,
}

impl PollOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Scheduled => "scheduled",
            Self::Unchanged => "unchanged",
            Self::ConfigPendingManual => "config_pending_manual",
            Self::BusySkipped => "busy_skipped",
            Self::BlockedDrift => "blocked_drift",
            Self::BlockedAttention => "blocked_attention",
            Self::SuppressedFailedTarget => "suppressed_failed_target",
            Self::RegistryError => "registry_error",
            Self::CredentialError => "credential_error",
            Self::InvalidSource => "invalid_source",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, PollStateError> {
        Ok(match value {
            "disabled" => Self::Disabled,
            "scheduled" => Self::Scheduled,
            "unchanged" => Self::Unchanged,
            "config_pending_manual" => Self::ConfigPendingManual,
            "busy_skipped" => Self::BusySkipped,
            "blocked_drift" => Self::BlockedDrift,
            "blocked_attention" => Self::BlockedAttention,
            "suppressed_failed_target" => Self::SuppressedFailedTarget,
            "registry_error" => Self::RegistryError,
            "credential_error" => Self::CredentialError,
            "invalid_source" => Self::InvalidSource,
            "cancelled" => Self::Cancelled,
            _ => return Err(PollStateError::Corrupt),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PollState {
    pub app_id: Uuid,
    pub generation: String,
    pub enabled: bool,
    pub consecutive_transient_failures: u8,
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_check_not_before: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_checked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_success_at: Option<OffsetDateTime>,
    pub last_source_descriptor_digest: Option<String>,
    pub last_etag: Option<String>,
    pub last_manifest_digest: Option<String>,
    pub last_platform: Option<String>,
    pub last_outcome: PollOutcome,
    pub last_error_class: Option<String>,
    pub last_error_code: Option<String>,
    pub suppressed_target_key: Option<String>,
    pub suppressed_deployment_id: Option<Uuid>,
    pub webhook_sequence: i64,
    pub webhook_processed_sequence: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_webhook_received_at: Option<OffsetDateTime>,
    pub last_wake_source: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct PollStateStore {
    database: Database,
}

#[derive(Clone, Debug)]
pub struct PollObservation<'a> {
    pub generation: &'a str,
    pub enabled: bool,
    pub next_check_not_before: Option<OffsetDateTime>,
    pub checked_at: Option<OffsetDateTime>,
    pub success: bool,
    pub replace_observed_fields: bool,
    pub source_descriptor_digest: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub manifest_digest: Option<&'a str>,
    pub platform: Option<&'a str>,
    pub outcome: PollOutcome,
    pub error_class: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub transient_failures: u8,
}

impl PollStateStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub async fn get(&self, app_id: Uuid) -> Result<Option<PollState>, PollStateError> {
        sqlx::query("SELECT * FROM poll_states WHERE app_id=?")
            .bind(app_id.to_string())
            .fetch_optional(self.database.pool())
            .await?
            .map(parse_state)
            .transpose()
    }

    pub async fn list(&self) -> Result<Vec<PollState>, PollStateError> {
        sqlx::query("SELECT * FROM poll_states ORDER BY app_id")
            .fetch_all(self.database.pool())
            .await?
            .into_iter()
            .map(parse_state)
            .collect()
    }

    pub async fn publish(
        &self,
        app_id: Uuid,
        value: PollObservation<'_>,
    ) -> Result<(), PollStateError> {
        if value.transient_failures > 5 {
            return Err(PollStateError::Invalid);
        }
        let now = OffsetDateTime::now_utc();
        sqlx::query(
            "INSERT INTO poll_states (app_id,generation,enabled,consecutive_transient_failures,next_check_not_before,last_checked_at,last_success_at,last_source_descriptor_digest,last_etag,last_manifest_digest,last_platform,last_outcome,last_error_class,last_error_code,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(app_id) DO UPDATE SET enabled=excluded.enabled,consecutive_transient_failures=excluded.consecutive_transient_failures,next_check_not_before=excluded.next_check_not_before,last_checked_at=COALESCE(excluded.last_checked_at,poll_states.last_checked_at),last_success_at=CASE WHEN ? THEN COALESCE(excluded.last_checked_at,poll_states.last_success_at) ELSE poll_states.last_success_at END,last_source_descriptor_digest=CASE WHEN poll_states.generation<>excluded.generation OR ? THEN excluded.last_source_descriptor_digest ELSE COALESCE(excluded.last_source_descriptor_digest,poll_states.last_source_descriptor_digest) END,last_etag=CASE WHEN poll_states.generation<>excluded.generation OR ? THEN excluded.last_etag ELSE COALESCE(excluded.last_etag,poll_states.last_etag) END,last_manifest_digest=CASE WHEN poll_states.generation<>excluded.generation OR ? THEN excluded.last_manifest_digest ELSE COALESCE(excluded.last_manifest_digest,poll_states.last_manifest_digest) END,last_platform=CASE WHEN poll_states.generation<>excluded.generation OR ? THEN excluded.last_platform ELSE COALESCE(excluded.last_platform,poll_states.last_platform) END,last_outcome=excluded.last_outcome,last_error_class=excluded.last_error_class,last_error_code=excluded.last_error_code,suppressed_target_key=CASE WHEN poll_states.generation<>excluded.generation THEN NULL ELSE poll_states.suppressed_target_key END,suppressed_deployment_id=CASE WHEN poll_states.generation<>excluded.generation THEN NULL ELSE poll_states.suppressed_deployment_id END,updated_at=excluded.updated_at,generation=excluded.generation",
        )
        .bind(app_id.to_string())
        .bind(value.generation)
        .bind(value.enabled)
        .bind(i64::from(value.transient_failures))
        .bind(optional_time(value.next_check_not_before)?)
        .bind(optional_time(value.checked_at)?)
        .bind(if value.success { optional_time(value.checked_at)? } else { None })
        .bind(value.source_descriptor_digest)
        .bind(value.etag)
        .bind(value.manifest_digest)
        .bind(value.platform)
        .bind(value.outcome.as_str())
        .bind(value.error_class)
        .bind(value.error_code)
        .bind(format_time(now)?)
        .bind(value.success)
        .bind(value.replace_observed_fields)
        .bind(value.replace_observed_fields)
        .bind(value.replace_observed_fields)
        .bind(value.replace_observed_fields)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    pub async fn retain_apps(&self, app_ids: &[Uuid]) -> Result<(), PollStateError> {
        let mut transaction = self.database.pool().begin().await?;
        let ids = app_ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let rows = sqlx::query("SELECT app_id FROM poll_states")
            .fetch_all(&mut *transaction)
            .await?;
        for row in rows {
            let id: String = row.get(0);
            if !ids.contains(&id) {
                sqlx::query("DELETE FROM poll_states WHERE app_id=?")
                    .bind(id)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        let replay_apps = sqlx::query("SELECT DISTINCT app_id FROM webhook_replays")
            .fetch_all(&mut *transaction)
            .await?;
        for row in replay_apps {
            let id: String = row.get(0);
            if !ids.contains(&id) {
                sqlx::query("DELETE FROM webhook_replays WHERE app_id=?")
                    .bind(id)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn suppress(
        &self,
        app_id: Uuid,
        target_key: &str,
        deployment_id: Uuid,
    ) -> Result<(), PollStateError> {
        let changed = sqlx::query("UPDATE poll_states SET suppressed_target_key=?,suppressed_deployment_id=?,last_outcome='suppressed_failed_target',updated_at=? WHERE app_id=?")
            .bind(target_key)
            .bind(deployment_id.to_string())
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(app_id.to_string())
            .execute(self.database.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(PollStateError::Corrupt);
        }
        Ok(())
    }

    pub async fn clear_suppression_if_target(
        &self,
        app_id: Uuid,
        target_key: &str,
    ) -> Result<(), PollStateError> {
        sqlx::query("UPDATE poll_states SET suppressed_target_key=NULL,suppressed_deployment_id=NULL,updated_at=? WHERE app_id=? AND suppressed_target_key=?")
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(app_id.to_string())
            .bind(target_key)
            .execute(self.database.pool())
            .await?;
        Ok(())
    }

    pub async fn counts(&self) -> Result<(i64, i64, i64), PollStateError> {
        let row = sqlx::query("SELECT SUM(CASE WHEN enabled=1 THEN 1 ELSE 0 END),SUM(CASE WHEN suppressed_target_key IS NOT NULL THEN 1 ELSE 0 END),SUM(CASE WHEN last_outcome IN ('registry_error','credential_error','invalid_source') THEN 1 ELSE 0 END) FROM poll_states")
            .fetch_one(self.database.pool()).await?;
        Ok((
            row.get::<Option<i64>, _>(0).unwrap_or(0),
            row.get::<Option<i64>, _>(1).unwrap_or(0),
            row.get::<Option<i64>, _>(2).unwrap_or(0),
        ))
    }

    pub async fn accept_webhook(
        &self,
        app_id: Uuid,
        secret_revision: Uuid,
        nonce_sha256: &[u8],
        request_id: Uuid,
        generation: &str,
        enabled: bool,
    ) -> Result<WebhookAccept, PollStateError> {
        let now = OffsetDateTime::now_utc();
        let expires = now + time::Duration::minutes(10);
        let now_text = format_time(now)?;
        let mut tx = self.database.pool().begin().await?;
        sqlx::query("DELETE FROM webhook_replays WHERE rowid IN (SELECT rowid FROM webhook_replays WHERE expires_at < ? ORDER BY expires_at LIMIT 100)")
            .bind(&now_text)
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO webhook_replays (app_id,secret_revision,nonce_sha256,received_at,expires_at,request_id) VALUES (?,?,?,?,?,?)")
            .bind(app_id.to_string())
            .bind(secret_revision.to_string())
            .bind(nonce_sha256)
            .bind(&now_text)
            .bind(format_time(expires)?)
            .bind(request_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if inserted == 0 {
            tx.rollback().await?;
            return Ok(WebhookAccept::Replay);
        }
        let action = if enabled {
            let previous: Option<(i64, i64)> = sqlx::query_as(
                "SELECT webhook_sequence,webhook_processed_sequence FROM poll_states WHERE app_id=?",
            )
            .bind(app_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO poll_states (app_id,generation,enabled,last_outcome,updated_at,webhook_sequence,last_webhook_received_at,last_wake_source) VALUES (?,?,1,'scheduled',?,1,?,'webhook') ON CONFLICT(app_id) DO UPDATE SET consecutive_transient_failures=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.consecutive_transient_failures ELSE 0 END,next_check_not_before=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.next_check_not_before ELSE NULL END,last_checked_at=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_checked_at ELSE NULL END,last_success_at=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_success_at ELSE NULL END,last_source_descriptor_digest=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_source_descriptor_digest ELSE NULL END,last_etag=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_etag ELSE NULL END,last_manifest_digest=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_manifest_digest ELSE NULL END,last_platform=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_platform ELSE NULL END,last_error_class=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_error_class ELSE NULL END,last_error_code=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_error_code ELSE NULL END,suppressed_target_key=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.suppressed_target_key ELSE NULL END,suppressed_deployment_id=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.suppressed_deployment_id ELSE NULL END,generation=excluded.generation,enabled=1,last_outcome=CASE WHEN poll_states.generation=excluded.generation THEN poll_states.last_outcome ELSE 'scheduled' END,webhook_sequence=poll_states.webhook_sequence+1,last_webhook_received_at=excluded.last_webhook_received_at,last_wake_source='webhook',updated_at=excluded.updated_at")
                .bind(app_id.to_string())
                .bind(generation)
                .bind(&now_text)
                .bind(&now_text)
                .execute(&mut *tx)
                .await?;
            if previous.is_some_and(|(sequence, processed)| sequence > processed) {
                WebhookAccept::Coalesced
            } else {
                WebhookAccept::Accepted
            }
        } else {
            WebhookAccept::IgnoredDisabled
        };
        let audit_action = match action {
            WebhookAccept::Accepted => "registry_recheck_requested",
            WebhookAccept::Coalesced => "registry_recheck_coalesced",
            WebhookAccept::IgnoredDisabled => "registry_recheck_ignored_disabled",
            WebhookAccept::Replay => unreachable!(),
        };
        sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('webhook',?,?,'app',?,'accepted','{}',?)")
            .bind(request_id.to_string())
            .bind(audit_action)
            .bind(app_id.to_string())
            .bind(&now_text)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(action)
    }

    pub async fn mark_webhook_processed(
        &self,
        app_id: Uuid,
        captured: i64,
    ) -> Result<(), PollStateError> {
        sqlx::query("UPDATE poll_states SET webhook_processed_sequence=MIN(webhook_sequence,MAX(webhook_processed_sequence,?)),last_wake_source=CASE WHEN webhook_sequence>? THEN 'webhook' ELSE last_wake_source END,updated_at=? WHERE app_id=?")
            .bind(captured)
            .bind(captured)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(app_id.to_string())
            .execute(self.database.pool())
            .await?;
        Ok(())
    }

    pub async fn webhook_replay_count(&self) -> Result<i64, PollStateError> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM webhook_replays")
            .fetch_one(self.database.pool())
            .await?)
    }

    pub async fn remove_app_operational(&self, app_id: Uuid) -> Result<(), PollStateError> {
        let mut transaction = self.database.pool().begin().await?;
        sqlx::query("DELETE FROM webhook_replays WHERE app_id=?")
            .bind(app_id.to_string())
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM poll_states WHERE app_id=?")
            .bind(app_id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookAccept {
    Accepted,
    Coalesced,
    IgnoredDisabled,
    Replay,
}

fn optional_time(value: Option<OffsetDateTime>) -> Result<Option<String>, DbError> {
    value.map(format_time).transpose()
}

fn parse_optional(value: Option<String>) -> Result<Option<OffsetDateTime>, PollStateError> {
    value
        .map(|value| parse_time(&value))
        .transpose()
        .map_err(Into::into)
}

fn parse_state(row: sqlx::sqlite::SqliteRow) -> Result<PollState, PollStateError> {
    Ok(PollState {
        app_id: row
            .get::<String, _>("app_id")
            .parse()
            .map_err(|_| PollStateError::Corrupt)?,
        generation: row.get("generation"),
        enabled: row.get("enabled"),
        consecutive_transient_failures: u8::try_from(
            row.get::<i64, _>("consecutive_transient_failures"),
        )
        .map_err(|_| PollStateError::Corrupt)?,
        next_check_not_before: parse_optional(row.get("next_check_not_before"))?,
        last_checked_at: parse_optional(row.get("last_checked_at"))?,
        last_success_at: parse_optional(row.get("last_success_at"))?,
        last_source_descriptor_digest: row.get("last_source_descriptor_digest"),
        last_etag: row.get("last_etag"),
        last_manifest_digest: row.get("last_manifest_digest"),
        last_platform: row.get("last_platform"),
        last_outcome: PollOutcome::parse(&row.get::<String, _>("last_outcome"))?,
        last_error_class: row.get("last_error_class"),
        last_error_code: row.get("last_error_code"),
        suppressed_target_key: row.get("suppressed_target_key"),
        suppressed_deployment_id: row
            .get::<Option<String>, _>("suppressed_deployment_id")
            .map(|value| value.parse().map_err(|_| PollStateError::Corrupt))
            .transpose()?,
        webhook_sequence: row.get("webhook_sequence"),
        webhook_processed_sequence: row.get("webhook_processed_sequence"),
        last_webhook_received_at: parse_optional(row.get("last_webhook_received_at"))?,
        last_wake_source: row.get("last_wake_source"),
        updated_at: parse_time(&row.get::<String, _>("updated_at"))?,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum PollStateError {
    #[error("poll state is invalid")]
    Invalid,
    #[error("poll state is corrupt")]
    Corrupt,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn generation_change_clears_failed_target_suppression() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = PollStateStore::new(database);
        let app = Uuid::new_v4();
        let observation = |generation| PollObservation {
            generation,
            enabled: true,
            next_check_not_before: None,
            checked_at: Some(OffsetDateTime::now_utc()),
            success: true,
            replace_observed_fields: true,
            source_descriptor_digest: None,
            etag: None,
            manifest_digest: None,
            platform: None,
            outcome: PollOutcome::Unchanged,
            error_class: None,
            error_code: None,
            transient_failures: 0,
        };
        store.publish(app, observation("a")).await.unwrap();
        store.suppress(app, "target", Uuid::new_v4()).await.unwrap();
        store.publish(app, observation("b")).await.unwrap();
        assert!(
            store
                .get(app)
                .await
                .unwrap()
                .unwrap()
                .suppressed_target_key
                .is_none()
        );
    }

    #[tokio::test]
    async fn generation_change_never_reuses_another_source_validator() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = PollStateStore::new(database);
        let app = Uuid::new_v4();
        store
            .publish(
                app,
                PollObservation {
                    generation: "source-a",
                    enabled: true,
                    next_check_not_before: None,
                    checked_at: Some(OffsetDateTime::now_utc()),
                    success: true,
                    replace_observed_fields: true,
                    source_descriptor_digest: Some("sha256:source-a"),
                    etag: Some("\"source-a\""),
                    manifest_digest: Some("sha256:manifest-a"),
                    platform: Some("linux/arm64/v8"),
                    outcome: PollOutcome::Unchanged,
                    error_class: None,
                    error_code: None,
                    transient_failures: 0,
                },
            )
            .await
            .unwrap();
        store
            .publish(
                app,
                PollObservation {
                    generation: "source-b",
                    enabled: true,
                    next_check_not_before: None,
                    checked_at: Some(OffsetDateTime::now_utc()),
                    success: false,
                    replace_observed_fields: false,
                    source_descriptor_digest: None,
                    etag: None,
                    manifest_digest: None,
                    platform: None,
                    outcome: PollOutcome::RegistryError,
                    error_class: Some("transient"),
                    error_code: Some("REGISTRY_UNAVAILABLE"),
                    transient_failures: 1,
                },
            )
            .await
            .unwrap();
        let state = store.get(app).await.unwrap().unwrap();
        assert_eq!(state.generation, "source-b");
        assert!(state.last_etag.is_none());
        assert!(state.last_source_descriptor_digest.is_none());
        assert!(state.last_manifest_digest.is_none());
        assert!(state.last_platform.is_none());
    }

    #[tokio::test]
    async fn webhook_nonce_wake_and_audit_commit_atomically_and_coalesce() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = PollStateStore::new(database.clone());
        let app = Uuid::new_v4();
        let revision = Uuid::new_v4();
        store
            .publish(
                app,
                PollObservation {
                    generation: "old-generation",
                    enabled: true,
                    next_check_not_before: None,
                    checked_at: Some(OffsetDateTime::now_utc()),
                    success: true,
                    replace_observed_fields: true,
                    source_descriptor_digest: Some("sha256:old"),
                    etag: Some("old-etag"),
                    manifest_digest: Some("sha256:old"),
                    platform: Some("linux/amd64"),
                    outcome: PollOutcome::Unchanged,
                    error_class: None,
                    error_code: None,
                    transient_failures: 0,
                },
            )
            .await
            .unwrap();
        store
            .suppress(app, "old-target", Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(
            store
                .accept_webhook(app, revision, &[1; 32], Uuid::new_v4(), "generation", true)
                .await
                .unwrap(),
            WebhookAccept::Accepted
        );
        assert_eq!(
            store
                .accept_webhook(app, revision, &[2; 32], Uuid::new_v4(), "generation", true)
                .await
                .unwrap(),
            WebhookAccept::Coalesced
        );
        assert_eq!(
            store
                .accept_webhook(app, revision, &[1; 32], Uuid::new_v4(), "generation", true)
                .await
                .unwrap(),
            WebhookAccept::Replay
        );
        let state = store.get(app).await.unwrap().unwrap();
        assert_eq!(state.generation, "generation");
        assert!(state.last_etag.is_none());
        assert!(state.last_manifest_digest.is_none());
        assert!(state.suppressed_target_key.is_none());
        assert_eq!(
            (state.webhook_sequence, state.webhook_processed_sequence),
            (2, 0)
        );
        store.mark_webhook_processed(app, 1).await.unwrap();
        let state = store.get(app).await.unwrap().unwrap();
        assert_eq!(
            (state.webhook_sequence, state.webhook_processed_sequence),
            (2, 1)
        );
        assert_eq!(database.audit_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn disabled_webhook_is_audited_without_pending_wake() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = PollStateStore::new(database.clone());
        let app = Uuid::new_v4();
        assert_eq!(
            store
                .accept_webhook(
                    app,
                    Uuid::new_v4(),
                    &[3; 32],
                    Uuid::new_v4(),
                    "generation",
                    false
                )
                .await
                .unwrap(),
            WebhookAccept::IgnoredDisabled
        );
        assert!(store.get(app).await.unwrap().is_none());
        assert_eq!(database.audit_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn webhook_preserves_same_generation_credential_backoff() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = PollStateStore::new(database);
        let app = Uuid::new_v4();
        let deadline = OffsetDateTime::now_utc() + time::Duration::minutes(30);
        store
            .publish(
                app,
                PollObservation {
                    generation: "generation",
                    enabled: true,
                    next_check_not_before: Some(deadline),
                    checked_at: Some(OffsetDateTime::now_utc()),
                    success: false,
                    replace_observed_fields: false,
                    source_descriptor_digest: None,
                    etag: None,
                    manifest_digest: None,
                    platform: None,
                    outcome: PollOutcome::CredentialError,
                    error_class: Some("credential"),
                    error_code: Some("REGISTRY_CREDENTIAL_INVALID"),
                    transient_failures: 0,
                },
            )
            .await
            .unwrap();
        store
            .accept_webhook(
                app,
                Uuid::new_v4(),
                &[4; 32],
                Uuid::new_v4(),
                "generation",
                true,
            )
            .await
            .unwrap();
        let state = store.get(app).await.unwrap().unwrap();
        assert_eq!(state.last_outcome, PollOutcome::CredentialError);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some("REGISTRY_CREDENTIAL_INVALID")
        );
        assert_eq!(state.next_check_not_before, Some(deadline));
        assert_eq!(state.webhook_sequence, 1);
    }
}
