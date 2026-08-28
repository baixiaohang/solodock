pub mod models;

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    app_store::recovery::RecoveryReport,
    security::permissions::{PermissionError, check_private, set_private_file_mode},
};

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn open(path: &Path) -> Result<Self, DbError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => check_private(path, false)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)?;
                }
                #[cfg(not(unix))]
                OpenOptions::new().write(true).create_new(true).open(path)?;
            }
            Err(error) => return Err(error.into()),
        }
        for sidecar in sqlite_sidecars(path) {
            match std::fs::symlink_metadata(&sidecar) {
                Ok(_) => check_private(&sidecar, false)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        set_private_file_mode(path)?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        for sidecar in sqlite_sidecars(path) {
            match std::fs::symlink_metadata(&sidecar) {
                Ok(_) => set_private_file_mode(&sidecar)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    pub async fn has_admin(&self) -> Result<bool, DbError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_credentials")
            .fetch_one(&self.pool)
            .await?;
        Ok(count != 0)
    }

    pub async fn refresh_app_index(&self, report: &RecoveryReport) -> Result<(), DbError> {
        let indexed_at = format_time(OffsetDateTime::now_utc())?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM app_index")
            .execute(&mut *transaction)
            .await?;
        for app in &report.valid_apps {
            sqlx::query(
                "INSERT INTO app_index (app_id, slug, display_name, project_name, active_release_id, active_image_ref, source_updated_at, indexed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(app.app_id.to_string())
            .bind(&app.slug)
            .bind(&app.display_name)
            .bind(&app.project_name)
            .bind(app.active_release_id.map(|id| id.to_string()))
            .bind(&app.active_image_ref)
            .bind(format_time(app.source_updated_at)?)
            .bind(&indexed_at)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn audit_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn indexed_active(
        &self,
        app_id: &str,
    ) -> Result<Option<(Option<String>, Option<String>)>, DbError> {
        let row = sqlx::query(
            "SELECT active_release_id, active_image_ref FROM app_index WHERE app_id = ?",
        )
        .bind(app_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| (row.get(0), row.get(1))))
    }
}

fn sqlite_sidecars(path: &Path) -> [PathBuf; 3] {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    let mut journal = path.as_os_str().to_os_string();
    journal.push("-journal");
    [
        PathBuf::from(wal),
        PathBuf::from(shm),
        PathBuf::from(journal),
    ]
}

pub fn format_time(value: OffsetDateTime) -> Result<String, DbError> {
    value
        .format(&Rfc3339)
        .map_err(|_| DbError::InvalidTimestamp)
}

pub fn parse_time(value: &str) -> Result<OffsetDateTime, DbError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| DbError::InvalidTimestamp)
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database is busy")]
    Busy,
    #[error("database operation failed")]
    Sql(sqlx::Error),
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error("database file creation failed")]
    Io(#[from] std::io::Error),
    #[error("database contained an invalid timestamp")]
    InvalidTimestamp,
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        let is_busy = error.as_database_error().is_some_and(|database_error| {
            matches!(database_error.code().as_deref(), Some("5" | "6"))
        });
        if is_busy {
            Self::Busy
        } else {
            Self::Sql(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn migrates_new_database_idempotently() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("state.sqlite3");
        let database = Database::open(&path).await.unwrap();
        assert!(!database.has_admin().await.unwrap());
        drop(database);
        let database = Database::open(&path).await.unwrap();
        assert_eq!(database.audit_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn corrupted_database_fails_without_replacement() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("state.sqlite3");
        std::fs::write(&path, b"not sqlite").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Database::open(&path).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
    }

    #[tokio::test]
    async fn rejects_unsafe_existing_database_permissions() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("state.sqlite3");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            Database::open(&path).await,
            Err(DbError::Permission(PermissionError::Mode(_)))
        ));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[tokio::test]
    async fn classifies_locked_database_as_busy() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let mut lock = database.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock)
            .await
            .unwrap();
        let mut competing = database.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA busy_timeout = 1")
            .execute(&mut *competing)
            .await
            .unwrap();
        let error =
            sqlx::query("UPDATE auth_throttle SET failure_count = 1 WHERE singleton_id = 1")
                .execute(&mut *competing)
                .await
                .unwrap_err();
        assert!(matches!(DbError::from(error), DbError::Busy));
        sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_dangling_sqlite_sidecar_symlink_before_connecting() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("state.sqlite3");
        let outside = root.path().join("outside-wal");
        symlink(&outside, root.path().join("state.sqlite3-wal")).unwrap();
        assert!(matches!(
            Database::open(&path).await,
            Err(DbError::Permission(PermissionError::UnexpectedType(_)))
        ));
        assert!(!outside.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
