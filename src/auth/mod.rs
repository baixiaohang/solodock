pub mod password;
pub mod session;

use std::{fs, path::PathBuf, sync::Arc};

use sqlx::Row;
use thiserror::Error;
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    app_store::{atomic::AtomicWriter, sync_directory},
    db::{Database, DbError, format_time, models::AuditMetadata, parse_time},
    security::{
        permissions::check_private,
        secret::{SecretError, SecretValue, SystemTokenSource, TokenSource},
    },
};

use self::{
    password::{PasswordError, PasswordService},
    session::{ABSOLUTE_TTL, REFRESH_INTERVAL, idle_expiry},
};

const THROTTLE_WINDOW: Duration = Duration::minutes(15);
const THROTTLE_LIMIT: i64 = 5;
const THROTTLE_COOLDOWN: Duration = Duration::minutes(15);

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Clone)]
pub struct AuthService {
    database: Database,
    password: PasswordService,
    clock: Arc<dyn Clock>,
    tokens: Arc<dyn TokenSource>,
    bootstrap_path: PathBuf,
    login_guard: Arc<Mutex<()>>,
}

pub struct LoginSession {
    pub session_token: SecretValue,
    pub csrf_token: SecretValue,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedSession {
    pub id: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

impl AuthService {
    pub fn new(database: Database, bootstrap_path: PathBuf) -> Self {
        Self {
            database,
            password: PasswordService::default(),
            clock: Arc::new(SystemClock),
            tokens: Arc::new(SystemTokenSource),
            bootstrap_path,
            login_guard: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(test)]
    pub fn with_dependencies(
        database: Database,
        bootstrap_path: PathBuf,
        clock: Arc<dyn Clock>,
        tokens: Arc<dyn TokenSource>,
    ) -> Self {
        Self {
            database,
            password: PasswordService::default(),
            clock,
            tokens,
            bootstrap_path,
            login_guard: Arc::new(Mutex::new(())),
        }
    }

    pub async fn prepare_bootstrap(&self) -> Result<bool, AuthError> {
        if self.database.has_admin().await? {
            self.remove_bootstrap_file()?;
            return Ok(false);
        }
        let token = self.tokens.generate()?;
        AtomicWriter::write(&self.bootstrap_path, token.expose().as_bytes(), 0o600)?;
        Ok(true)
    }

    pub async fn is_initialized(&self) -> Result<bool, AuthError> {
        Ok(self.database.has_admin().await?)
    }

    pub async fn bootstrap(
        &self,
        supplied_token: String,
        password: String,
        request_id: Uuid,
    ) -> Result<(), AuthError> {
        if self.database.has_admin().await? {
            self.remove_bootstrap_file()?;
            return Err(AuthError::AlreadyBootstrapped);
        }
        check_private(&self.bootstrap_path, false)?;
        let expected = SecretValue::new(fs::read_to_string(&self.bootstrap_path)?);
        let supplied_token = SecretValue::new(supplied_token);
        if !expected.constant_time_eq(supplied_token.expose()) {
            return Err(AuthError::BootstrapTokenInvalid);
        }
        let hash = self.password.hash(password).await?;
        let now = format_time(self.clock.now())?;
        let mut transaction = self.database.pool().begin().await?;
        let result = sqlx::query("INSERT INTO admin_credentials (singleton_id, username, password_hash, created_at, updated_at) VALUES (1, 'admin', ?, ?, ?)")
            .bind(hash)
            .bind(&now)
            .bind(&now)
            .execute(&mut *transaction)
            .await;
        if let Err(error) = result {
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation())
            {
                return Err(AuthError::AlreadyBootstrapped);
            }
            return Err(error.into());
        }
        insert_audit(
            &mut transaction,
            "system",
            request_id,
            "auth.bootstrap",
            "success",
            AuditMetadata::empty(),
            &now,
        )
        .await?;
        transaction.commit().await?;
        self.remove_bootstrap_file()?;
        Ok(())
    }

    pub async fn login(
        &self,
        username: &str,
        password: String,
        request_id: Uuid,
    ) -> Result<LoginSession, AuthError> {
        let _login_guard = self.login_guard.lock().await;
        let now = self.clock.now();
        let row = sqlx::query("SELECT password_hash FROM admin_credentials WHERE singleton_id = 1")
            .fetch_optional(self.database.pool())
            .await?
            .ok_or(AuthError::SetupRequired)?;
        self.ensure_not_throttled(now, request_id).await?;
        let password_hash: String = row.get(0);
        let valid = username == "admin" && self.password.verify(password, password_hash).await?;
        if !valid {
            self.record_login_failure(request_id, now).await?;
            return Err(AuthError::InvalidCredentials);
        }

        let session_token = self.tokens.generate()?;
        let csrf_token = self.tokens.generate()?;
        let created_at = now;
        let absolute_expires_at = now + ABSOLUTE_TTL;
        let idle_expires_at = idle_expiry(now, absolute_expires_at);
        let id = Uuid::new_v4().to_string();
        let now_text = format_time(now)?;
        let idle_text = format_time(idle_expires_at)?;
        let absolute_text = format_time(absolute_expires_at)?;
        let mut transaction = self.database.pool().begin().await?;
        sqlx::query("INSERT INTO sessions (id, token_hash, created_at, last_seen_at, idle_expires_at, absolute_expires_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(session_token.sha256().to_vec())
            .bind(&now_text)
            .bind(&now_text)
            .bind(&idle_text)
            .bind(&absolute_text)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("UPDATE auth_throttle SET window_started_at = NULL, failure_count = 0, blocked_until = NULL WHERE singleton_id = 1")
            .execute(&mut *transaction)
            .await?;
        insert_audit(
            &mut transaction,
            "admin",
            request_id,
            "auth.login",
            "success",
            AuditMetadata::empty(),
            &now_text,
        )
        .await?;
        transaction.commit().await?;
        Ok(LoginSession {
            session_token,
            csrf_token,
            created_at,
            expires_at: absolute_expires_at,
        })
    }

    pub async fn authenticate(&self, token: &str) -> Result<AuthenticatedSession, AuthError> {
        if !self.database.has_admin().await? {
            return Err(AuthError::SetupRequired);
        }
        let token = SecretValue::new(token.to_owned());
        let row = sqlx::query("SELECT id, created_at, last_seen_at, idle_expires_at, absolute_expires_at FROM sessions WHERE token_hash = ?")
            .bind(token.sha256().to_vec())
            .fetch_optional(self.database.pool())
            .await?
            .ok_or(AuthError::SessionRequired)?;
        let id: String = row.get(0);
        let created_at = parse_time(row.get::<&str, _>(1))?;
        let last_seen_at = parse_time(row.get::<&str, _>(2))?;
        let idle_expires_at = parse_time(row.get::<&str, _>(3))?;
        let absolute_expires_at = parse_time(row.get::<&str, _>(4))?;
        let now = self.clock.now();
        if now >= idle_expires_at || now >= absolute_expires_at {
            sqlx::query("DELETE FROM sessions WHERE id = ?")
                .bind(&id)
                .execute(self.database.pool())
                .await?;
            return Err(AuthError::SessionExpired);
        }
        if now - last_seen_at >= REFRESH_INTERVAL {
            sqlx::query("UPDATE sessions SET last_seen_at = ?, idle_expires_at = ? WHERE id = ?")
                .bind(format_time(now)?)
                .bind(format_time(idle_expiry(now, absolute_expires_at))?)
                .bind(&id)
                .execute(self.database.pool())
                .await?;
        }
        Ok(AuthenticatedSession {
            id,
            created_at,
            expires_at: absolute_expires_at,
        })
    }

    pub async fn logout(&self, token: &str, request_id: Uuid) -> Result<(), AuthError> {
        let token = SecretValue::new(token.to_owned());
        let now = format_time(self.clock.now())?;
        let mut transaction = self.database.pool().begin().await?;
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token.sha256().to_vec())
            .execute(&mut *transaction)
            .await?;
        insert_audit(
            &mut transaction,
            "admin",
            request_id,
            "auth.logout",
            "success",
            AuditMetadata::empty(),
            &now,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn revoke_all(&self, request_id: Uuid) -> Result<(), AuthError> {
        let now = format_time(self.clock.now())?;
        let mut transaction = self.database.pool().begin().await?;
        sqlx::query("DELETE FROM sessions")
            .execute(&mut *transaction)
            .await?;
        insert_audit(
            &mut transaction,
            "admin",
            request_id,
            "auth.revoke_all",
            "success",
            AuditMetadata::empty(),
            &now,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn ensure_not_throttled(
        &self,
        now: OffsetDateTime,
        request_id: Uuid,
    ) -> Result<(), AuthError> {
        let blocked_until: Option<String> =
            sqlx::query_scalar("SELECT blocked_until FROM auth_throttle WHERE singleton_id = 1")
                .fetch_one(self.database.pool())
                .await?;
        if let Some(blocked_until) = blocked_until {
            let blocked_until = parse_time(&blocked_until)?;
            if blocked_until > now {
                let now_text = format_time(now)?;
                let mut transaction = self.database.pool().begin().await?;
                insert_audit(
                    &mut transaction,
                    "anonymous",
                    request_id,
                    "auth.login",
                    "failure",
                    AuditMetadata::reason("AUTH_COOLDOWN"),
                    &now_text,
                )
                .await?;
                transaction.commit().await?;
                return Err(AuthError::Cooldown(
                    (blocked_until - now).whole_seconds().max(1),
                ));
            }
        }
        Ok(())
    }

    async fn record_login_failure(
        &self,
        request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), AuthError> {
        let mut transaction = self.database.pool().begin().await?;
        let row = sqlx::query(
            "SELECT window_started_at, failure_count FROM auth_throttle WHERE singleton_id = 1",
        )
        .fetch_one(&mut *transaction)
        .await?;
        let window_started: Option<String> = row.get(0);
        let previous_count: i64 = row.get(1);
        let within_window = window_started
            .as_deref()
            .map(parse_time)
            .transpose()?
            .is_some_and(|started| now - started < THROTTLE_WINDOW);
        let failure_count = if within_window { previous_count + 1 } else { 1 };
        let window_started = if within_window {
            window_started.expect("window exists when within window")
        } else {
            format_time(now)?
        };
        let blocked_until = (failure_count >= THROTTLE_LIMIT)
            .then(|| format_time(now + THROTTLE_COOLDOWN))
            .transpose()?;
        sqlx::query("UPDATE auth_throttle SET window_started_at = ?, failure_count = ?, blocked_until = ? WHERE singleton_id = 1")
            .bind(window_started)
            .bind(failure_count)
            .bind(blocked_until)
            .execute(&mut *transaction)
            .await?;
        let now_text = format_time(now)?;
        insert_audit(
            &mut transaction,
            "anonymous",
            request_id,
            "auth.login",
            "failure",
            AuditMetadata::reason("AUTH_INVALID"),
            &now_text,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    fn remove_bootstrap_file(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.bootstrap_path) {
            Ok(()) => {
                if let Some(parent) = self.bootstrap_path.parent() {
                    sync_directory(parent)?;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

async fn insert_audit(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &'static str,
    request_id: Uuid,
    action: &'static str,
    result: &'static str,
    metadata: AuditMetadata,
    created_at: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_events (actor, request_id, action, target_type, target_id, result, redacted_metadata, created_at) VALUES (?, ?, ?, NULL, NULL, ?, ?, ?)")
        .bind(actor)
        .bind(request_id.to_string())
        .bind(action)
        .bind(result)
        .bind(metadata.to_json())
        .bind(created_at)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("setup is required")]
    SetupRequired,
    #[error("administrator already exists")]
    AlreadyBootstrapped,
    #[error("bootstrap token is invalid")]
    BootstrapTokenInvalid,
    #[error("credentials are invalid")]
    InvalidCredentials,
    #[error("authentication cooldown is active")]
    Cooldown(i64),
    #[error("session is required")]
    SessionRequired,
    #[error("session has expired")]
    SessionExpired,
    #[error(transparent)]
    Password(#[from] PasswordError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("managed file operation failed")]
    Store(#[from] crate::app_store::StoreError),
    #[error("managed file operation failed")]
    Io(#[from] std::io::Error),
    #[error("managed path permission check failed")]
    Permission(#[from] crate::security::permissions::PermissionError),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

impl From<sqlx::Error> for AuthError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DbError::from(error))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{
            Arc,
            atomic::{AtomicI64, AtomicUsize, Ordering},
        },
    };

    use argon2::{
        Algorithm, Argon2, Params, Version,
        password_hash::{PasswordHasher, SaltString},
    };
    use tempfile::tempdir;
    use tokio::sync::Barrier;

    use super::*;

    struct TestClock(AtomicI64);

    impl TestClock {
        fn new(now: OffsetDateTime) -> Self {
            Self(AtomicI64::new(now.unix_timestamp()))
        }

        fn advance(&self, duration: Duration) {
            self.0.fetch_add(duration.whole_seconds(), Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> OffsetDateTime {
            OffsetDateTime::from_unix_timestamp(self.0.load(Ordering::SeqCst)).unwrap()
        }
    }

    struct TestTokens(AtomicUsize);

    impl TokenSource for TestTokens {
        fn generate(&self) -> Result<SecretValue, SecretError> {
            Ok(SecretValue::new(format!(
                "test-token-{}",
                self.0.fetch_add(1, Ordering::SeqCst)
            )))
        }
    }

    fn fast_test_password_hash(password: &str) -> String {
        Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8, 1, 1, None).unwrap(),
        )
        .hash_password(
            password.as_bytes(),
            &SaltString::encode_b64(b"solodock-auth-test").unwrap(),
        )
        .unwrap()
        .to_string()
    }

    #[tokio::test]
    async fn bootstrap_replaces_token_and_session_honors_absolute_ttl() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let clock = Arc::new(TestClock::new(
            OffsetDateTime::from_unix_timestamp(1_787_875_200).unwrap(),
        ));
        let tokens = Arc::new(TestTokens(AtomicUsize::new(0)));
        let bootstrap_path = root.path().join("bootstrap.token");
        let auth = AuthService::with_dependencies(
            database.clone(),
            bootstrap_path.clone(),
            clock.clone(),
            tokens,
        );
        auth.prepare_bootstrap().await.unwrap();
        assert_eq!(fs::read_to_string(&bootstrap_path).unwrap(), "test-token-0");
        auth.prepare_bootstrap().await.unwrap();
        assert_eq!(fs::read_to_string(&bootstrap_path).unwrap(), "test-token-1");
        assert!(matches!(
            auth.bootstrap(
                "test-token-0".into(),
                "correct horse battery".into(),
                Uuid::new_v4()
            )
            .await,
            Err(AuthError::BootstrapTokenInvalid)
        ));
        auth.bootstrap(
            "test-token-1".into(),
            "correct horse battery".into(),
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        assert!(!bootstrap_path.exists());

        let login = auth
            .login("admin", "correct horse battery".into(), Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(login.session_token.expose(), "test-token-2");
        let stored_hash: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_ne!(stored_hash, login.session_token.expose().as_bytes());

        for _ in 0..12 {
            clock.advance(Duration::minutes(59));
            auth.authenticate(login.session_token.expose())
                .await
                .unwrap();
        }
        clock.advance(Duration::minutes(13));
        assert!(matches!(
            auth.authenticate(login.session_token.expose()).await,
            Err(AuthError::SessionExpired)
        ));
    }

    #[tokio::test]
    async fn failure_window_enters_and_leaves_global_cooldown() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_787_875_200).unwrap();
        let clock = Arc::new(TestClock::new(now));
        let auth = AuthService::with_dependencies(
            database,
            root.path().join("bootstrap.token"),
            clock.clone(),
            Arc::new(TestTokens(AtomicUsize::new(0))),
        );
        for _ in 0..THROTTLE_LIMIT {
            auth.record_login_failure(Uuid::new_v4(), clock.now())
                .await
                .unwrap();
        }
        assert!(matches!(
            auth.ensure_not_throttled(clock.now(), Uuid::new_v4()).await,
            Err(AuthError::Cooldown(_))
        ));
        clock.advance(Duration::minutes(16));
        auth.ensure_not_throttled(clock.now(), Uuid::new_v4())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn concurrent_failed_logins_are_serialized_without_internal_errors() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let clock = Arc::new(TestClock::new(
            OffsetDateTime::from_unix_timestamp(1_787_875_200).unwrap(),
        ));
        let auth = AuthService::with_dependencies(
            database.clone(),
            root.path().join("bootstrap.token"),
            clock,
            Arc::new(TestTokens(AtomicUsize::new(0))),
        );
        let now = format_time(auth.clock.now()).unwrap();
        sqlx::query("INSERT INTO admin_credentials (singleton_id, username, password_hash, created_at, updated_at) VALUES (1, 'admin', ?, ?, ?)")
            .bind(fast_test_password_hash("correct horse battery"))
            .bind(&now)
            .bind(&now)
            .execute(database.pool())
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(7));
        let mut attempts = Vec::new();
        for _ in 0..6 {
            let auth = auth.clone();
            let barrier = barrier.clone();
            attempts.push(tokio::spawn(async move {
                barrier.wait().await;
                auth.login("admin", "incorrect password".into(), Uuid::new_v4())
                    .await
            }));
        }
        barrier.wait().await;
        let mut invalid = 0;
        let mut cooldown = 0;
        for attempt in attempts {
            match attempt.await.unwrap() {
                Err(AuthError::InvalidCredentials) => invalid += 1,
                Err(AuthError::Cooldown(_)) => cooldown += 1,
                Err(error) => panic!("unexpected login error: {error}"),
                Ok(_) => panic!("invalid login unexpectedly succeeded"),
            }
        }
        assert_eq!(invalid, 5);
        assert_eq!(cooldown, 1);
        let failure_count: i64 =
            sqlx::query_scalar("SELECT failure_count FROM auth_throttle WHERE singleton_id = 1")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(failure_count, 5);
        let login_audits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = 'auth.login'")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(login_audits, 6);
    }

    #[tokio::test]
    async fn auth_sqlite_lock_is_classified_as_busy_on_the_real_caller_path() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let auth = AuthService::with_dependencies(
            database.clone(),
            root.path().join("bootstrap.token"),
            Arc::new(TestClock::new(
                OffsetDateTime::from_unix_timestamp(1_787_875_200).unwrap(),
            )),
            Arc::new(TestTokens(AtomicUsize::new(0))),
        );
        let mut connections = Vec::new();
        for _ in 0..5 {
            let mut connection = database.pool().acquire().await.unwrap();
            sqlx::query("PRAGMA busy_timeout = 1")
                .execute(&mut *connection)
                .await
                .unwrap();
            connections.push(connection);
        }
        let mut lock = connections.pop().unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock)
            .await
            .unwrap();
        drop(connections);

        assert!(matches!(
            auth.record_login_failure(Uuid::new_v4(), auth.clock.now())
                .await,
            Err(AuthError::Database(DbError::Busy))
        ));
        sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    }
}
