use std::path::PathBuf;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, SaltString},
};
use sha2::{Digest, Sha256};
use solodock::{
    auth::AuthService,
    db::{Database, format_time, models::AuditMetadata},
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub struct TestAuth {
    pub service: AuthService,
    pub cookie: String,
    #[allow(dead_code)]
    pub csrf: String,
}

pub async fn seed_authenticated_session(database: &Database, bootstrap_path: PathBuf) -> TestAuth {
    let session_token = format!("test-session-{}", Uuid::new_v4());
    let csrf = format!("test-csrf-{}", Uuid::new_v4());
    let now = OffsetDateTime::now_utc();
    let created_at = format_time(now).unwrap();
    let idle_expires_at = format_time(now + Duration::hours(1)).unwrap();
    let absolute_expires_at = format_time(now + Duration::hours(12)).unwrap();
    let password_hash = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(8, 1, 1, None).unwrap(),
    )
    .hash_password(
        b"test fixture password",
        &SaltString::encode_b64(b"solodock-fixture").unwrap(),
    )
    .unwrap()
    .to_string();

    let mut transaction = database.pool().begin().await.unwrap();
    sqlx::query("INSERT INTO admin_credentials (singleton_id, username, password_hash, created_at, updated_at) VALUES (1, 'admin', ?, ?, ?)")
        .bind(password_hash)
        .bind(&created_at)
        .bind(&created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (id, token_hash, created_at, last_seen_at, idle_expires_at, absolute_expires_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(Uuid::new_v4().to_string())
        .bind(Sha256::digest(session_token.as_bytes()).to_vec())
        .bind(&created_at)
        .bind(&created_at)
        .bind(idle_expires_at)
        .bind(absolute_expires_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
    for (actor, action) in [("system", "auth.bootstrap"), ("admin", "auth.login")] {
        sqlx::query("INSERT INTO audit_events (actor, request_id, action, target_type, target_id, result, redacted_metadata, created_at) VALUES (?, ?, ?, NULL, NULL, 'success', ?, ?)")
            .bind(actor)
            .bind(Uuid::new_v4().to_string())
            .bind(action)
            .bind(AuditMetadata::empty().to_json())
            .bind(&created_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();

    TestAuth {
        service: AuthService::new(database.clone(), bootstrap_path),
        cookie: format!("__Host-solodock_session={session_token}; __Host-solodock_csrf={csrf}"),
        csrf,
    }
}
