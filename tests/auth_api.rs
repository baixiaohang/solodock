use std::{
    fs,
    io::Write,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use solodock::{
    AppState,
    auth::{AuthService, password::PasswordService},
    db::Database,
    router,
};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing::{Instrument, instrument::WithSubscriber};

struct Harness {
    _root: TempDir,
    database: Database,
    app: axum::Router,
    bootstrap_token: String,
}

struct SessionCookies {
    header: String,
    session: String,
    csrf: String,
}

impl Harness {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let bootstrap_path = root.path().join("bootstrap.token");
        let auth = AuthService::new(database.clone(), bootstrap_path.clone());
        assert!(auth.prepare_bootstrap().await.unwrap());
        let bootstrap_token = fs::read_to_string(bootstrap_path).unwrap();
        let app = router(AppState::control_plane(
            auth,
            "https://solodock.example.com".into(),
            root.path().to_owned(),
        ));
        Self {
            _root: root,
            database,
            app,
            bootstrap_token,
        }
    }

    fn request(&self, method: &str, uri: &str, body: Value) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
            ))
            .header(header::CONTENT_LENGTH, body.to_string().len())
    }

    async fn bootstrap(&self, password: &str) -> axum::response::Response {
        let body = json!({"bootstrap_token": self.bootstrap_token, "password": password});
        self.app
            .clone()
            .oneshot(
                self.request("POST", "/api/v1/auth/bootstrap", body.clone())
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn login_session(&self, password: &str) -> SessionCookies {
        let body = json!({"username":"admin","password":password});
        let response = self
            .app
            .clone()
            .oneshot(
                self.request("POST", "/api/v1/auth/login", body.clone())
                    .header(header::ORIGIN, "https://solodock.example.com")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let set_cookies: Vec<_> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        let session = cookie_value(
            set_cookies
                .iter()
                .find(|value| value.starts_with("__Host-solodock_session="))
                .unwrap(),
        );
        let csrf = cookie_value(
            set_cookies
                .iter()
                .find(|value| value.starts_with("__Host-solodock_csrf="))
                .unwrap(),
        );
        SessionCookies {
            header: format!("__Host-solodock_session={session}; __Host-solodock_csrf={csrf}"),
            session,
            csrf,
        }
    }
}

#[tokio::test]
async fn bootstrap_login_me_and_logout_follow_security_contract() {
    let harness = Harness::new().await;
    let password = "correct horse battery";
    let unauthenticated_installation = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/system/installation", json!({}))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        unauthenticated_installation,
        StatusCode::UNAUTHORIZED,
        "SESSION_REQUIRED",
    )
    .await;
    let response = harness.bootstrap(password).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key("x-request-id"));
    assert!(!format!("{:?}", response).contains(&harness.bootstrap_token));

    let second = harness.bootstrap(password).await;
    assert_eq!(second.status(), StatusCode::CONFLICT);

    let login_body = json!({"username":"admin","password":password});
    let captured_logs = CaptureWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(captured_logs.clone())
        .finish();
    let login = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", login_body.clone())
                .header(header::ORIGIN, "https://SOLODOCK.example.com:443")
                .body(Body::from(login_body.to_string()))
                .unwrap(),
        )
        .instrument(tracing::info_span!("secret_canary"))
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::NO_CONTENT);
    let set_cookies: Vec<_> = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(set_cookies.len(), 2);
    let session_cookie = set_cookies
        .iter()
        .find(|value| value.starts_with("__Host-solodock_session="))
        .unwrap();
    let csrf_cookie = set_cookies
        .iter()
        .find(|value| value.starts_with("__Host-solodock_csrf="))
        .unwrap();
    for cookie in &set_cookies {
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
        assert!(!cookie.contains("Domain="));
    }
    assert!(session_cookie.contains("HttpOnly"));
    assert!(!csrf_cookie.contains("HttpOnly"));
    let session_value = cookie_value(session_cookie);
    let csrf_value = cookie_value(csrf_cookie);
    let logs = captured_logs.contents();
    for canary in [
        password,
        &harness.bootstrap_token,
        &session_value,
        &csrf_value,
    ] {
        assert!(!logs.contains(canary));
    }
    let cookie_header =
        format!("__Host-solodock_session={session_value}; __Host-solodock_csrf={csrf_value}");

    let me = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/me", json!({}))
                .header(header::COOKIE, &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
    assert_eq!(me.headers()[header::CACHE_CONTROL], "no-store");
    let body: Value =
        serde_json::from_slice(&me.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["username"], "admin");

    let installation = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/system/installation", json!({}))
                .header(header::COOKIE, &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(installation.status(), StatusCode::OK);
    assert_eq!(installation.headers()[header::CACHE_CONTROL], "no-store");
    let installation_body: Value =
        serde_json::from_slice(&installation.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(matches!(
        installation_body["channel"].as_str(),
        Some("stable" | "main" | "development" | "unknown")
    ));

    let missing_csrf = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/logout", json!({}))
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(missing_csrf, StatusCode::FORBIDDEN, "CSRF_INVALID").await;

    let logout = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/logout", json!({}))
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookie_header)
                .header("x-csrf-token", &csrf_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let expired: Vec<_> = logout
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .collect();
    assert_eq!(expired.len(), 2);
    assert!(
        expired
            .iter()
            .all(|cookie| cookie.to_str().unwrap().contains("Max-Age=0"))
    );

    let login_body = json!({"username":"admin","password":password});
    let login = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", login_body.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(login_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let set_cookies: Vec<_> = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    let session_value = cookie_value(
        set_cookies
            .iter()
            .find(|value| value.starts_with("__Host-solodock_session="))
            .unwrap(),
    );
    let csrf_value = cookie_value(
        set_cookies
            .iter()
            .find(|value| value.starts_with("__Host-solodock_csrf="))
            .unwrap(),
    );
    let cookie_header =
        format!("__Host-solodock_session={session_value}; __Host-solodock_csrf={csrf_value}");
    let revoke = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/me/sessions/revoke-all", json!({}))
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, cookie_header)
                .header("x-csrf-token", &csrf_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);

    let session_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(session_rows, 0);
    let audit: Vec<(String, String)> =
        sqlx::query_as("SELECT action, redacted_metadata FROM audit_events ORDER BY id")
            .fetch_all(harness.database.pool())
            .await
            .unwrap();
    assert_eq!(audit.len(), 5);
    let audit_text = format!("{audit:?}");
    for canary in [
        password,
        &harness.bootstrap_token,
        &session_value,
        &csrf_value,
    ] {
        assert!(!audit_text.contains(canary));
    }
}

#[tokio::test]
async fn password_change_gates_precede_credential_mutation() {
    let harness = Harness::new().await;
    let old_password = "correct horse battery";
    assert_eq!(
        harness.bootstrap(old_password).await.status(),
        StatusCode::NO_CONTENT
    );
    let cookies = harness.login_session(old_password).await;
    let original_credential: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let original_sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    let original_throttle: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT window_started_at, failure_count, blocked_until FROM auth_throttle WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let original_audits = harness.database.audit_count().await.unwrap();
    let valid = json!({
        "current_password": old_password,
        "new_password": "new correct horse battery",
    });

    let missing_origin = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", valid.clone())
                .header(header::COOKIE, &cookies.header)
                .header("x-csrf-token", &cookies.csrf)
                .body(Body::from(valid.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(missing_origin, StatusCode::FORBIDDEN, "ORIGIN_INVALID").await;

    let missing_csrf = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", valid.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookies.header)
                .body(Body::from(valid.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(missing_csrf, StatusCode::FORBIDDEN, "CSRF_INVALID").await;

    let csrf_cookie = format!("__Host-solodock_csrf={}", cookies.csrf);
    let unauthenticated = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", valid.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, csrf_cookie)
                .header("x-csrf-token", &cookies.csrf)
                .body(Body::from(valid.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        unauthenticated,
        StatusCode::UNAUTHORIZED,
        "SESSION_REQUIRED",
    )
    .await;

    for invalid in [
        json!({"current_password":old_password,"new_password":"new correct horse battery","unexpected":true}),
        json!({"current_password":old_password,"new_password":"too short"}),
    ] {
        let response = harness
            .app
            .clone()
            .oneshot(
                harness
                    .request("PUT", "/api/v1/me/password", invalid.clone())
                    .header(header::ORIGIN, "https://solodock.example.com")
                    .header(header::COOKIE, &cookies.header)
                    .header("x-csrf-token", &cookies.csrf)
                    .body(Body::from(invalid.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error(
            response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_FAILED",
        )
        .await;
    }

    let malformed = "{\"current_password\":";
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", json!({}))
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookies.header)
                .header("x-csrf-token", &cookies.csrf)
                .body(Body::from(malformed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "VALIDATION_FAILED",
    )
    .await;

    let oversized = json!({
        "current_password": "x".repeat(17 * 1024),
        "new_password": "new correct horse battery",
    });
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", oversized.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookies.header)
                .header("x-csrf-token", &cookies.csrf)
                .body(Body::from(oversized.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "VALIDATION_FAILED",
    )
    .await;

    let credential: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    let throttle: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT window_started_at, failure_count, blocked_until FROM auth_throttle WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(credential, original_credential);
    assert_eq!(sessions, original_sessions);
    assert_eq!(throttle, original_throttle);
    assert_eq!(
        harness.database.audit_count().await.unwrap(),
        original_audits
    );
}

#[tokio::test]
async fn invalid_current_password_uses_shared_throttle_without_revoking_session() {
    let harness = Harness::new().await;
    let old_password = "correct horse battery";
    let wrong_password = "wrong-current-password-canary";
    let new_password = "new-password-secret-canary";
    assert_eq!(
        harness.bootstrap(old_password).await.status(),
        StatusCode::NO_CONTENT
    );
    let cookies = harness.login_session(old_password).await;
    let credential_before: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();

    for _ in 0..5 {
        let body = json!({"current_password":wrong_password,"new_password":new_password});
        let response = harness
            .app
            .clone()
            .oneshot(
                harness
                    .request("PUT", "/api/v1/me/password", body.clone())
                    .header(header::ORIGIN, "https://solodock.example.com")
                    .header(header::COOKIE, &cookies.header)
                    .header("x-csrf-token", &cookies.csrf)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let error = assert_error(response, StatusCode::FORBIDDEN, "CURRENT_PASSWORD_INVALID").await;
        let error_text = error.to_string();
        assert!(!error_text.contains(wrong_password));
        assert!(!error_text.contains(new_password));
    }

    let body = json!({"current_password":old_password,"new_password":new_password});
    let cooldown = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", body.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &cookies.header)
                .header("x-csrf-token", &cookies.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(cooldown, StatusCode::TOO_MANY_REQUESTS, "AUTH_COOLDOWN").await;

    let login = json!({"username":"admin","password":old_password});
    let login_cooldown = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", login.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(login.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        login_cooldown,
        StatusCode::TOO_MANY_REQUESTS,
        "AUTH_COOLDOWN",
    )
    .await;

    let me = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/me", json!({}))
                .header(header::COOKIE, &cookies.header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
    let failure_count: i64 =
        sqlx::query_scalar("SELECT failure_count FROM auth_throttle WHERE singleton_id = 1")
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    assert_eq!(failure_count, 5);
    let credential_after: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(credential_after, credential_before);
    let password_audits: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT actor, result, redacted_metadata FROM audit_events WHERE action = 'auth.password_change' ORDER BY id",
    )
    .fetch_all(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(password_audits.len(), 6);
    assert!(
        password_audits
            .iter()
            .all(|(actor, result, _)| actor == "admin" && result == "failure")
    );
    assert!(
        password_audits[..5]
            .iter()
            .all(|(_, _, metadata)| metadata.contains("CURRENT_PASSWORD_INVALID"))
    );
    assert!(password_audits[5].2.contains("AUTH_COOLDOWN"));
    let audit_text = format!("{password_audits:?}");
    assert!(!audit_text.contains(wrong_password));
    assert!(!audit_text.contains(new_password));
}

#[tokio::test]
async fn password_change_atomically_updates_credential_revokes_sessions_and_expires_cookies() {
    let harness = Harness::new().await;
    let old_password = "old-password-secret-canary";
    let new_password = "new-password-secret-canary";
    assert_eq!(
        harness.bootstrap(old_password).await.status(),
        StatusCode::NO_CONTENT
    );
    let first = harness.login_session(old_password).await;
    let second = harness.login_session(old_password).await;
    let before: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE auth_throttle SET window_started_at = '2026-09-04T00:00:00Z', failure_count = 3 WHERE singleton_id = 1")
        .execute(harness.database.pool())
        .await
        .unwrap();
    let body = json!({"current_password":old_password,"new_password":new_password});
    let captured_logs = CaptureWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(captured_logs.clone())
        .finish();
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", body.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &first.header)
                .header("x-csrf-token", &first.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .instrument(tracing::info_span!("password_change_canary"))
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let expired: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(expired.len(), 2);
    assert!(expired.iter().all(|cookie| cookie.contains("Max-Age=0")));
    let expired_session = expired
        .iter()
        .find(|cookie| cookie.starts_with("__Host-solodock_session=;"))
        .expect("the session cookie must be expired");
    let expired_csrf = expired
        .iter()
        .find(|cookie| cookie.starts_with("__Host-solodock_csrf=;"))
        .expect("the CSRF cookie must be expired");
    for cookie in [expired_session, expired_csrf] {
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Secure"));
    }
    assert!(expired_session.contains("HttpOnly"));
    assert!(!expired_csrf.contains("HttpOnly"));
    let response_body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(response_body.is_empty());

    let after: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_ne!(after.0, before.0);
    assert_ne!(after.1, before.1);
    assert!(
        PasswordService::default()
            .verify(new_password.into(), after.0.clone())
            .await
            .unwrap()
    );
    assert!(
        !PasswordService::default()
            .verify(old_password.into(), after.0)
            .await
            .unwrap()
    );
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let throttle: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT window_started_at, failure_count, blocked_until FROM auth_throttle WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(throttle, (None, 0, None));
    let password_audit: (String, String, String) = sqlx::query_as(
        "SELECT actor, result, redacted_metadata FROM audit_events WHERE action = 'auth.password_change'",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    assert_eq!(password_audit.0, "admin");
    assert_eq!(password_audit.1, "success");
    assert_eq!(password_audit.2, "{\"reason_code\":null}");

    let stale_session = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/me", json!({}))
                .header(header::COOKIE, &second.header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(stale_session, StatusCode::UNAUTHORIZED, "SESSION_REQUIRED").await;

    let old_login = json!({"username":"admin","password":old_password});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", old_login.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(old_login.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(response, StatusCode::UNAUTHORIZED, "AUTH_INVALID").await;
    let new_login = harness.login_session(new_password).await;
    assert!(!new_login.session.is_empty());

    let logs = captured_logs.contents();
    let audit_text: String =
        sqlx::query_scalar("SELECT GROUP_CONCAT(redacted_metadata, ',') FROM audit_events")
            .fetch_one(harness.database.pool())
            .await
            .unwrap();
    for canary in [old_password, new_password, &first.session, &first.csrf] {
        assert!(!logs.contains(canary));
        assert!(!audit_text.contains(canary));
    }
}

#[tokio::test]
async fn password_change_audit_failure_rolls_back_hash_sessions_throttle_and_cookies() {
    let harness = Harness::new().await;
    let old_password = "old-password-secret-canary";
    let new_password = "new-password-secret-canary";
    assert_eq!(
        harness.bootstrap(old_password).await.status(),
        StatusCode::NO_CONTENT
    );
    let first = harness.login_session(old_password).await;
    let second = harness.login_session(old_password).await;
    sqlx::query("UPDATE auth_throttle SET window_started_at = '2026-09-04T00:00:00Z', failure_count = 3 WHERE singleton_id = 1")
        .execute(harness.database.pool())
        .await
        .unwrap();
    let credential_before: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let throttle_before: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT window_started_at, failure_count, blocked_until FROM auth_throttle WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let audits_before = harness.database.audit_count().await.unwrap();
    sqlx::query("CREATE TRIGGER reject_password_change_audit BEFORE INSERT ON audit_events WHEN NEW.action = 'auth.password_change' BEGIN SELECT RAISE(ABORT, 'audit disabled'); END")
        .execute(harness.database.pool())
        .await
        .unwrap();

    let body = json!({"current_password":old_password,"new_password":new_password});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("PUT", "/api/v1/me/password", body.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .header(header::COOKIE, &first.header)
                .header("x-csrf-token", &first.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert_error(
        response,
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
    )
    .await;

    let credential_after: (String, String) = sqlx::query_as(
        "SELECT password_hash, updated_at FROM admin_credentials WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let throttle_after: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT window_started_at, failure_count, blocked_until FROM auth_throttle WHERE singleton_id = 1",
    )
    .fetch_one(harness.database.pool())
    .await
    .unwrap();
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(credential_after, credential_before);
    assert_eq!(throttle_after, throttle_before);
    assert_eq!(sessions, 2);
    assert_eq!(harness.database.audit_count().await.unwrap(), audits_before);
    assert!(
        PasswordService::default()
            .verify(old_password.into(), credential_after.0.clone())
            .await
            .unwrap()
    );
    assert!(
        !PasswordService::default()
            .verify(new_password.into(), credential_after.0)
            .await
            .unwrap()
    );
    for cookies in [&first, &second] {
        let me = harness
            .app
            .clone()
            .oneshot(
                harness
                    .request("GET", "/api/v1/me", json!({}))
                    .header(header::COOKIE, &cookies.header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn rejects_peer_origin_json_and_rolls_back_when_audit_fails() {
    let harness = Harness::new().await;
    let password = "correct horse battery";

    let setup_required = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("GET", "/api/v1/me", json!({}))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        setup_required,
        StatusCode::SERVICE_UNAVAILABLE,
        "SETUP_REQUIRED",
    )
    .await;

    let body = json!({"bootstrap_token": harness.bootstrap_token, "password": password});
    let remote = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/bootstrap", body.clone())
                .extension(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(remote, StatusCode::FORBIDDEN, "BOOTSTRAP_LOCAL_ONLY").await;
    assert_eq!(
        harness.bootstrap(password).await.status(),
        StatusCode::NO_CONTENT
    );

    let unknown = json!({"username":"admin","password":password,"unexpected":true});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", unknown.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(unknown.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "VALIDATION_FAILED",
    )
    .await;

    let oversized = json!({"username":"admin","password":"x".repeat(17 * 1024)});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", oversized.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(oversized.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "VALIDATION_FAILED",
    )
    .await;

    let missing_origin = json!({"username":"admin","password":password});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", missing_origin.clone())
                .body(Body::from(missing_origin.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(response, StatusCode::FORBIDDEN, "ORIGIN_INVALID").await;

    sqlx::query("CREATE TRIGGER reject_login_audit BEFORE INSERT ON audit_events WHEN NEW.action = 'auth.login' BEGIN SELECT RAISE(ABORT, 'audit disabled'); END")
        .execute(harness.database.pool())
        .await
        .unwrap();
    let login = json!({"username":"admin","password":password});
    let response = harness
        .app
        .clone()
        .oneshot(
            harness
                .request("POST", "/api/v1/auth/login", login.clone())
                .header(header::ORIGIN, "https://solodock.example.com")
                .body(Body::from(login.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
    )
    .await;
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(harness.database.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0, "session insert must roll back with audit");
}

fn cookie_value(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_owned()
}

async fn assert_error(response: axum::response::Response, status: StatusCode, code: &str) -> Value {
    assert_eq!(response.status(), status);
    assert!(response.headers().contains_key("x-request-id"));
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["code"], code);
    assert!(body["request_id"].as_str().is_some());
    body
}

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}
