use std::str::FromStr;

use chrono_tz::Tz;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::{Database, DbError, format_time};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalSettings {
    pub revision: Uuid,
    pub display_timezone: String,
}

#[derive(Clone)]
pub struct SettingsStore {
    database: Database,
}

impl SettingsStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub async fn load(&self) -> Result<GlobalSettings, SettingsError> {
        let row = sqlx::query(
            "SELECT revision, display_timezone FROM global_settings WHERE singleton_id = 1",
        )
        .fetch_one(self.database.pool())
        .await?;
        let revision = row
            .get::<String, _>(0)
            .parse()
            .map_err(|_| SettingsError::Corrupt)?;
        let display_timezone = row.get::<String, _>(1);
        validate_timezone(&display_timezone)?;
        Ok(GlobalSettings {
            revision,
            display_timezone,
        })
    }

    pub async fn update(
        &self,
        expected_revision: Uuid,
        display_timezone: &str,
    ) -> Result<GlobalSettings, SettingsError> {
        validate_timezone(display_timezone)?;
        let revision = Uuid::new_v4();
        let updated_at = format_time(OffsetDateTime::now_utc())?;
        let changed = sqlx::query(
            "UPDATE global_settings SET revision = ?, display_timezone = ?, updated_at = ? WHERE singleton_id = 1 AND revision = ?",
        )
        .bind(revision.to_string())
        .bind(display_timezone)
        .bind(updated_at)
        .bind(expected_revision.to_string())
        .execute(self.database.pool())
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(SettingsError::RevisionStale);
        }
        Ok(GlobalSettings {
            revision,
            display_timezone: display_timezone.to_owned(),
        })
    }
}

pub fn validate_timezone(value: &str) -> Result<Tz, SettingsError> {
    Tz::from_str(value).map_err(|_| SettingsError::TimezoneInvalid)
}

pub fn supported_timezones() -> Vec<&'static str> {
    let mut values = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|timezone| timezone.name())
        .collect::<Vec<_>>();
    values.sort_unstable_by(|left, right| match (*left == "UTC", *right == "UTC") {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => left.cmp(right),
    });
    values
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings revision is stale")]
    RevisionStale,
    #[error("display timezone is invalid")]
    TimezoneInvalid,
    #[error("settings record is corrupt")]
    Corrupt,
    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<sqlx::Error> for SettingsError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(DbError::from(value))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn defaults_to_utc_and_updates_with_revision_control() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let store = SettingsStore::new(database);
        let initial = store.load().await.unwrap();
        assert_eq!(initial.display_timezone, "UTC");
        let updated = store
            .update(initial.revision, "Asia/Shanghai")
            .await
            .unwrap();
        assert_eq!(updated.display_timezone, "Asia/Shanghai");
        assert!(matches!(
            store.update(initial.revision, "UTC").await,
            Err(SettingsError::RevisionStale)
        ));
    }

    #[test]
    fn rejects_arbitrary_timezones_and_lists_utc_first() {
        assert!(validate_timezone("Asia/Shanghai").is_ok());
        assert!(validate_timezone("Mars/Olympus").is_err());
        let supported = supported_timezones();
        assert_eq!(supported.first(), Some(&"UTC"));
        assert!(supported.contains(&"America/New_York"));
    }
}
