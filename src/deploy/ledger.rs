use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::{Database, DbError, format_time, parse_time};

#[derive(Clone, Debug)]
pub struct ScheduledResolvedTarget {
    pub image: crate::registry::ResolvedImage,
    pub generation: String,
    pub target_key: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTrigger {
    Manual,
    Rollback,
    Poll,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    Queued,
    Running,
    Succeeded,
    NoOp,
    Failed,
    RolledBack,
    NeedsAttention,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentPhase {
    Queued,
    Resolving,
    Preparing,
    Pulling,
    Applying,
    Verifying,
    Committing,
    RollingBack,
    VerifyingRollback,
    Terminal,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeploymentRecord {
    pub id: Uuid,
    pub app_id: Uuid,
    pub trigger: DeploymentTrigger,
    pub requested_revision: Uuid,
    pub from_release_id: Option<Uuid>,
    pub expected_pending_release_id: Option<Uuid>,
    pub expected_actual_release_id: Option<Uuid>,
    pub expected_actual_container_id: Option<String>,
    pub predecessor_runtime_release_id: Option<Uuid>,
    pub candidate_release_id: Option<Uuid>,
    pub rollback_target_release_id: Option<Uuid>,
    pub rollback_of_deployment_id: Option<Uuid>,
    pub status: DeploymentStatus,
    pub phase: DeploymentPhase,
    pub source_image_ref: Option<String>,
    pub source_descriptor_digest: Option<String>,
    pub manifest_digest: Option<String>,
    pub platform: Option<String>,
    pub scheduled_source_image_ref: Option<String>,
    pub scheduled_source_descriptor_digest: Option<String>,
    pub scheduled_manifest_digest: Option<String>,
    pub scheduled_index_digest: Option<String>,
    pub scheduled_platform_os: Option<String>,
    pub scheduled_platform_architecture: Option<String>,
    pub scheduled_platform_variant: Option<String>,
    pub scheduled_local_image_id: Option<String>,
    pub scheduled_repository: Option<String>,
    pub scheduled_target_key: Option<String>,
    pub poll_generation: Option<String>,
    pub error_class: Option<String>,
    pub error_code: Option<String>,
    pub health_policy: Option<String>,
    pub health_result: Option<String>,
    pub request_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeploymentTransition {
    pub seq: i64,
    pub phase: DeploymentPhase,
    pub result: String,
    pub code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct DeploymentLedger {
    database: Database,
}

impl DeploymentLedger {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        id: Uuid,
        app_id: Uuid,
        trigger: DeploymentTrigger,
        requested_revision: Uuid,
        from_release_id: Option<Uuid>,
        rollback_target: Option<Uuid>,
        rollback_of: Option<Uuid>,
        request_id: Uuid,
    ) -> Result<DeploymentRecord, LedgerError> {
        let now = format_time(OffsetDateTime::now_utc())?;
        let mut tx = self.database.pool().begin().await?;
        let result = sqlx::query("INSERT INTO deployments (id,app_id,trigger,requested_revision,from_release_id,rollback_target_release_id,rollback_of_deployment_id,status,phase,request_id,created_at,updated_at) VALUES (?,?,?,?,?,?,?,'queued','queued',?,?,?)")
            .bind(id.to_string()).bind(app_id.to_string()).bind(trigger.as_str()).bind(requested_revision.to_string())
            .bind(from_release_id.map(|v| v.to_string())).bind(rollback_target.map(|v| v.to_string())).bind(rollback_of.map(|v| v.to_string()))
            .bind(request_id.to_string()).bind(&now).bind(&now).execute(&mut *tx).await;
        if let Err(error) = result {
            if error
                .as_database_error()
                .is_some_and(|v| v.is_unique_violation())
            {
                return Err(LedgerError::Busy);
            }
            return Err(DbError::from(error).into());
        }
        sqlx::query("INSERT INTO deployment_transitions (deployment_id,seq,phase,result,created_at) VALUES (?,1,'queued','scheduled',?)")
            .bind(id.to_string()).bind(&now).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(id).await?.ok_or(LedgerError::Corrupt)
    }

    pub async fn transition(
        &self,
        id: Uuid,
        phase: DeploymentPhase,
        status: DeploymentStatus,
        result: &str,
        code: Option<&str>,
    ) -> Result<(), LedgerError> {
        let now = format_time(OffsetDateTime::now_utc())?;
        let terminal = status.is_terminal();
        let mut tx = self.database.pool().begin().await?;
        let changed = sqlx::query("UPDATE deployments SET phase=?,status=?,started_at=COALESCE(started_at,CASE WHEN ?='running' THEN ? ELSE NULL END),completed_at=CASE WHEN ? THEN ? ELSE completed_at END,error_code=?,updated_at=? WHERE id=? AND status IN ('queued','running')")
            .bind(phase.as_str()).bind(status.as_str()).bind(status.as_str()).bind(&now).bind(terminal).bind(&now).bind(code).bind(&now).bind(id.to_string()).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq),0)+1 FROM deployment_transitions WHERE deployment_id=?",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO deployment_transitions (deployment_id,seq,phase,result,code,created_at) VALUES (?,?,?,?,?,?)")
            .bind(id.to_string()).bind(seq).bind(phase.as_str()).bind(result).bind(code).bind(&now).execute(&mut *tx).await?;
        if terminal {
            let row = sqlx::query(
                "SELECT app_id,request_id,trigger,scheduled_target_key,poll_generation FROM deployments WHERE id=?",
            )
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
            let metadata = serde_json::json!({
                "deployment_id": id,
                "trigger": row.get::<String, _>("trigger"),
                "status": status.as_str(),
                "code": code,
            })
            .to_string();
            sqlx::query("INSERT INTO audit_events (actor,request_id,action,target_type,target_id,result,redacted_metadata,created_at) VALUES ('system',?,'deployment_terminal','app',?,?,?,?)")
                .bind(row.get::<String, _>("request_id"))
                .bind(row.get::<String, _>("app_id"))
                .bind(status.as_str())
                .bind(metadata)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            if row.get::<String, _>("trigger") == "poll"
                && matches!(
                    status,
                    DeploymentStatus::Failed
                        | DeploymentStatus::RolledBack
                        | DeploymentStatus::NeedsAttention
                )
                && let Some(target) = row.get::<Option<String>, _>("scheduled_target_key")
            {
                sqlx::query("UPDATE poll_states SET suppressed_target_key=?,suppressed_deployment_id=?,last_outcome='suppressed_failed_target',updated_at=? WHERE app_id=? AND generation=?")
                    .bind(target)
                    .bind(id.to_string())
                    .bind(&now)
                    .bind(row.get::<String,_>("app_id"))
                    .bind(row.get::<Option<String>,_>("poll_generation"))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn resolved(
        &self,
        id: Uuid,
        candidate: Uuid,
        predecessor: Option<Uuid>,
        source: &str,
        descriptor: &str,
        manifest: &str,
        platform: &str,
    ) -> Result<(), LedgerError> {
        let changed = sqlx::query("UPDATE deployments SET candidate_release_id=?,predecessor_runtime_release_id=?,source_image_ref=?,source_descriptor_digest=?,manifest_digest=?,platform=?,updated_at=? WHERE id=? AND status='running'")
            .bind(candidate.to_string()).bind(predecessor.map(|v| v.to_string())).bind(source).bind(descriptor).bind(manifest).bind(platform)
            .bind(format_time(OffsetDateTime::now_utc())?).bind(id.to_string()).execute(self.database.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        Ok(())
    }

    pub async fn set_health(
        &self,
        id: Uuid,
        policy: &str,
        result: &str,
    ) -> Result<(), LedgerError> {
        sqlx::query(
            "UPDATE deployments SET health_policy=?,health_result=?,updated_at=? WHERE id=?",
        )
        .bind(policy)
        .bind(result)
        .bind(format_time(OffsetDateTime::now_utc())?)
        .bind(id.to_string())
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_effect_started(
        &self,
        id: Uuid,
        pre_container_id: Option<&str>,
        pre_started_at: Option<&str>,
    ) -> Result<(), LedgerError> {
        let changed = sqlx::query("UPDATE deployments SET effect_phase='started',pre_container_id=?,pre_started_at=?,updated_at=? WHERE id=? AND status='running' AND phase='applying' AND effect_phase IS NULL")
            .bind(pre_container_id)
            .bind(pre_started_at)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(id.to_string())
            .execute(self.database.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        Ok(())
    }

    pub async fn mark_effect_observed(
        &self,
        id: Uuid,
        post_container_id: &str,
    ) -> Result<(), LedgerError> {
        let changed = sqlx::query("UPDATE deployments SET effect_phase='observed',post_container_id=?,updated_at=? WHERE id=? AND status='running' AND effect_phase='started'")
            .bind(post_container_id)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(id.to_string())
            .execute(self.database.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        Ok(())
    }

    pub async fn mark_rollback_started(
        &self,
        id: Uuid,
        pre_container_id: &str,
    ) -> Result<(), LedgerError> {
        let changed = sqlx::query("UPDATE deployments SET rollback_effect_phase='started',rollback_pre_container_id=?,updated_at=? WHERE id=? AND status='running' AND rollback_effect_phase IS NULL")
            .bind(pre_container_id)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(id.to_string())
            .execute(self.database.pool())
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        Ok(())
    }

    pub async fn mark_rollback_observed(
        &self,
        id: Uuid,
        post_container_id: Option<&str>,
    ) -> Result<(), LedgerError> {
        let changed = sqlx::query("UPDATE deployments SET rollback_effect_phase='observed',rollback_post_container_id=?,updated_at=? WHERE id=? AND status='running' AND rollback_effect_phase='started'")
            .bind(post_container_id)
            .bind(format_time(OffsetDateTime::now_utc())?)
            .bind(id.to_string())
            .execute(self.database.pool())
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(LedgerError::StateChanged);
        }
        Ok(())
    }

    pub async fn interrupt_nonterminal(&self) -> Result<u64, LedgerError> {
        let now = format_time(OffsetDateTime::now_utc())?;
        let changed = sqlx::query("UPDATE deployments SET status='interrupted',phase='terminal',error_class='interrupted',error_code='DEPLOYMENT_INTERRUPTED',completed_at=?,updated_at=? WHERE status IN ('queued','running')")
            .bind(&now).bind(&now).execute(self.database.pool()).await?.rows_affected();
        Ok(changed)
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<DeploymentRecord>, LedgerError> {
        let row = sqlx::query("SELECT * FROM deployments WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(self.database.pool())
            .await?;
        row.map(parse_record).transpose()
    }

    pub async fn list(
        &self,
        app_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DeploymentRecord>, LedgerError> {
        let rows = sqlx::query(
            "SELECT * FROM deployments WHERE app_id=? ORDER BY created_at DESC,id DESC LIMIT ?",
        )
        .bind(app_id.to_string())
        .bind(limit.min(50) as i64)
        .fetch_all(self.database.pool())
        .await?;
        rows.into_iter().map(parse_record).collect()
    }

    pub async fn list_page(
        &self,
        app_id: Uuid,
        limit: usize,
        cursor: Option<Uuid>,
    ) -> Result<(Vec<DeploymentRecord>, Option<Uuid>), LedgerError> {
        let limit = limit.clamp(1, 50);
        let rows = if let Some(cursor) = cursor {
            let row = sqlx::query("SELECT created_at,id FROM deployments WHERE id=? AND app_id=?")
                .bind(cursor.to_string())
                .bind(app_id.to_string())
                .fetch_optional(self.database.pool())
                .await?
                .ok_or(LedgerError::Corrupt)?;
            let created: String = row.get(0);
            let id: String = row.get(1);
            sqlx::query("SELECT * FROM deployments WHERE app_id=? AND (created_at < ? OR (created_at = ? AND id < ?)) ORDER BY created_at DESC,id DESC LIMIT ?")
                .bind(app_id.to_string()).bind(&created).bind(&created).bind(id).bind((limit + 1) as i64).fetch_all(self.database.pool()).await?
        } else {
            sqlx::query(
                "SELECT * FROM deployments WHERE app_id=? ORDER BY created_at DESC,id DESC LIMIT ?",
            )
            .bind(app_id.to_string())
            .bind((limit + 1) as i64)
            .fetch_all(self.database.pool())
            .await?
        };
        let mut values = rows
            .into_iter()
            .map(parse_record)
            .collect::<Result<Vec<_>, _>>()?;
        let next = (values.len() > limit).then(|| values[limit - 1].id);
        values.truncate(limit);
        Ok((values, next))
    }

    pub async fn transitions(&self, id: Uuid) -> Result<Vec<DeploymentTransition>, LedgerError> {
        sqlx::query("SELECT seq,phase,result,code,created_at FROM deployment_transitions WHERE deployment_id=? ORDER BY seq")
            .bind(id.to_string()).fetch_all(self.database.pool()).await?.into_iter().map(|row| Ok(DeploymentTransition {
                seq: row.get(0), phase: DeploymentPhase::parse(row.get::<String,_>(1).as_str())?, result: row.get(2), code: row.get(3), created_at: parse_time(&row.get::<String,_>(4))?
            })).collect()
    }

    pub async fn active_count(&self) -> Result<i64, LedgerError> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployments WHERE status IN ('queued','running')",
        )
        .fetch_one(self.database.pool())
        .await?)
    }

    pub async fn attention_counts(&self) -> Result<(i64, i64), LedgerError> {
        let interrupted =
            sqlx::query_scalar("SELECT COUNT(*) FROM deployments WHERE status='interrupted'")
                .fetch_one(self.database.pool())
                .await?;
        let needs_attention =
            sqlx::query_scalar("SELECT COUNT(*) FROM deployments WHERE status='needs_attention'")
                .fetch_one(self.database.pool())
                .await?;
        Ok((interrupted, needs_attention))
    }
}

fn parse_record(row: sqlx::sqlite::SqliteRow) -> Result<DeploymentRecord, LedgerError> {
    fn uuid(value: String) -> Result<Uuid, LedgerError> {
        value.parse().map_err(|_| LedgerError::Corrupt)
    }
    fn opt(value: Option<String>) -> Result<Option<Uuid>, LedgerError> {
        value.map(uuid).transpose()
    }
    Ok(DeploymentRecord {
        id: uuid(row.get("id"))?,
        app_id: uuid(row.get("app_id"))?,
        trigger: DeploymentTrigger::parse(&row.get::<String, _>("trigger"))?,
        requested_revision: uuid(row.get("requested_revision"))?,
        from_release_id: opt(row.get("from_release_id"))?,
        expected_pending_release_id: opt(row.get("expected_pending_release_id"))?,
        expected_actual_release_id: opt(row.get("expected_actual_release_id"))?,
        expected_actual_container_id: row.get("expected_actual_container_id"),
        predecessor_runtime_release_id: opt(row.get("predecessor_runtime_release_id"))?,
        candidate_release_id: opt(row.get("candidate_release_id"))?,
        rollback_target_release_id: opt(row.get("rollback_target_release_id"))?,
        rollback_of_deployment_id: opt(row.get("rollback_of_deployment_id"))?,
        status: DeploymentStatus::parse(&row.get::<String, _>("status"))?,
        phase: DeploymentPhase::parse(&row.get::<String, _>("phase"))?,
        source_image_ref: row.get("source_image_ref"),
        source_descriptor_digest: row.get("source_descriptor_digest"),
        manifest_digest: row.get("manifest_digest"),
        platform: row.get("platform"),
        scheduled_source_image_ref: row.get("scheduled_source_image_ref"),
        scheduled_source_descriptor_digest: row.get("scheduled_source_descriptor_digest"),
        scheduled_manifest_digest: row.get("scheduled_manifest_digest"),
        scheduled_index_digest: row.get("scheduled_index_digest"),
        scheduled_platform_os: row.get("scheduled_platform_os"),
        scheduled_platform_architecture: row.get("scheduled_platform_architecture"),
        scheduled_platform_variant: row.get("scheduled_platform_variant"),
        scheduled_local_image_id: row.get("scheduled_local_image_id"),
        scheduled_repository: row.get("scheduled_repository"),
        scheduled_target_key: row.get("scheduled_target_key"),
        poll_generation: row.get("poll_generation"),
        error_class: row.get("error_class"),
        error_code: row.get("error_code"),
        health_policy: row.get("health_policy"),
        health_result: row.get("health_result"),
        request_id: uuid(row.get("request_id"))?,
        created_at: parse_time(&row.get::<String, _>("created_at"))?,
        started_at: row
            .get::<Option<String>, _>("started_at")
            .map(|v| parse_time(&v))
            .transpose()?,
        completed_at: row
            .get::<Option<String>, _>("completed_at")
            .map(|v| parse_time(&v))
            .transpose()?,
        updated_at: parse_time(&row.get::<String, _>("updated_at"))?,
    })
}

macro_rules! string_enum {
    ($type:ty, {$($variant:ident => $text:literal),+ $(,)?}) => {
        impl $type {
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $text),+ } }
            fn parse(value: &str) -> Result<Self, LedgerError> { match value { $($text => Ok(Self::$variant)),+, _ => Err(LedgerError::Corrupt) } }
        }
    };
}
string_enum!(DeploymentTrigger, { Manual => "manual", Rollback => "rollback", Poll => "poll" });
string_enum!(DeploymentStatus, { Queued=>"queued", Running=>"running", Succeeded=>"succeeded", NoOp=>"no_op", Failed=>"failed", RolledBack=>"rolled_back", NeedsAttention=>"needs_attention", Interrupted=>"interrupted" });
string_enum!(DeploymentPhase, { Queued=>"queued", Resolving=>"resolving", Preparing=>"preparing", Pulling=>"pulling", Applying=>"applying", Verifying=>"verifying", Committing=>"committing", RollingBack=>"rolling_back", VerifyingRollback=>"verifying_rollback", Terminal=>"terminal" });
impl DeploymentStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("deployment is busy")]
    Busy,
    #[error("deployment state changed")]
    StateChanged,
    #[error("deployment record is corrupt")]
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
    async fn transitions_are_monotonic_and_nonterminal_app_claim_is_unique() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let ledger = DeploymentLedger::new(database);
        let app_id = Uuid::new_v4();
        let revision = Uuid::new_v4();
        let first = ledger
            .create(
                Uuid::new_v4(),
                app_id,
                DeploymentTrigger::Manual,
                revision,
                None,
                None,
                None,
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        assert!(matches!(
            ledger
                .create(
                    Uuid::new_v4(),
                    app_id,
                    DeploymentTrigger::Manual,
                    revision,
                    None,
                    None,
                    None,
                    Uuid::new_v4()
                )
                .await,
            Err(LedgerError::Busy)
        ));
        ledger
            .transition(
                first.id,
                DeploymentPhase::Resolving,
                DeploymentStatus::Running,
                "started",
                None,
            )
            .await
            .unwrap();
        ledger
            .transition(
                first.id,
                DeploymentPhase::Terminal,
                DeploymentStatus::Succeeded,
                "committed",
                None,
            )
            .await
            .unwrap();
        let transitions = ledger.transitions(first.id).await.unwrap();
        assert_eq!(
            transitions
                .iter()
                .map(|value| value.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            ledger.get(first.id).await.unwrap().unwrap().status,
            DeploymentStatus::Succeeded
        );
        ledger
            .create(
                Uuid::new_v4(),
                app_id,
                DeploymentTrigger::Manual,
                revision,
                None,
                None,
                None,
                Uuid::new_v4(),
            )
            .await
            .unwrap();
    }
}
