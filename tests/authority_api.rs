mod support;

use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

use axum::{
    body::Body,
    http::{HeaderValue, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use solodock::{
    AppState,
    app_store::AppStore,
    config::CanonicalAuthority,
    db::Database,
    mutation::idempotency::IdempotencyService,
    registry::PollStateStore,
    router,
    webhook::{WebhookRateLimiter, WebhookServices, WebhookStore},
};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

struct Harness {
    _root: TempDir,
    app: axum::Router,
    cookie: String,
}

impl Harness {
    async fn new(management: &str, local: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state_directory = root.path().join("state");
        let runtime_directory = root.path().join("runtime");
        let apps_directory = state_directory.join("apps");
        for path in [&state_directory, &runtime_directory, &apps_directory] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let database = Database::open(&state_directory.join("state.sqlite3"))
            .await
            .unwrap();
        let test_auth = support::seed_authenticated_session(
            &database,
            runtime_directory.join("bootstrap.token"),
        )
        .await;
        let idempotency =
            IdempotencyService::initialize(database.clone(), &state_directory).unwrap();
        let store =
            AppStore::initialize_verified(apps_directory, idempotency.integrity_key()).unwrap();
        let mut state = AppState::control_plane(
            test_auth.service,
            "https://solodock.example.com".into(),
            state_directory,
        );
        state.management_authority = CanonicalAuthority::parse_http(management).unwrap();
        state.webhook_authority =
            Some(CanonicalAuthority::parse_http("hooks.example.com").unwrap());
        state.local_probe_authority = CanonicalAuthority::parse_http(local).unwrap();
        state.webhooks = Some(Arc::new(WebhookServices {
            origin: "https://hooks.example.com".into(),
            store: WebhookStore::new(store, idempotency.integrity_key()),
            poll_states: PollStateStore::new(database.clone()),
            database,
            notify: Arc::new(tokio::sync::Notify::new()),
            limiter: WebhookRateLimiter::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(16)),
        }));
        Self {
            _root: root,
            app: router(state),
            cookie: test_auth.cookie,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        host: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        self.app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn authenticated(
        &self,
        method: Method,
        uri: &str,
        host: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, &self.cookie);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        self.app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
}

async fn assert_isolated(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response.headers().contains_key("x-request-id"));
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
}

#[tokio::test]
async fn management_and_webhook_authorities_do_not_cross_route() {
    let harness = Harness::new("solodock.example.com", "127.0.0.1:8080").await;
    let health = harness
        .request(Method::GET, "/healthz", Some("SOLODOCK.EXAMPLE.COM:443"))
        .await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        health.into_body().collect().await.unwrap().to_bytes(),
        r#"{"status":"ok"}"#
    );
    assert_eq!(
        harness
            .authenticated(Method::GET, "/api/v1/me", Some("solodock.example.com"))
            .await
            .status(),
        StatusCode::OK
    );
    let stream_path = format!("/api/v1/apps/{}/events", Uuid::new_v4());
    let management_stream = harness
        .authenticated(Method::GET, &stream_path, Some("solodock.example.com"))
        .await;
    assert_eq!(management_stream.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &management_stream
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
        )
        .unwrap()["code"],
        "APP_NOT_FOUND"
    );
    for host in ["hooks.example.com", "127.0.0.1:8080"] {
        assert_isolated(
            harness
                .authenticated(Method::GET, &stream_path, Some(host))
                .await,
        )
        .await;
    }
    assert_isolated(
        harness
            .authenticated(
                Method::POST,
                &format!("/hooks/v1/apps/{}/registry", Uuid::new_v4()),
                Some("solodock.example.com"),
            )
            .await,
    )
    .await;
    assert_isolated(
        harness
            .authenticated(Method::GET, "/api/v1/me", Some("hooks.example.com"))
            .await,
    )
    .await;
}

#[tokio::test]
async fn webhook_authority_allows_only_the_exact_canonical_post_path() {
    let harness = Harness::new("solodock.example.com", "127.0.0.1:8080").await;
    let app_id = Uuid::new_v4();
    let path = format!("/hooks/v1/apps/{app_id}/registry");
    let admitted = harness
        .request(Method::POST, &path, Some("HOOKS.EXAMPLE.COM:443"))
        .await;
    assert_eq!(admitted.status(), StatusCode::UNPROCESSABLE_ENTITY);

    for (method, candidate) in [
        (Method::GET, path.clone()),
        (Method::POST, format!("{path}/extra")),
        (Method::POST, format!("/prefix{path}")),
        (
            Method::POST,
            format!(
                "/hooks/v1/apps/{}/registry",
                app_id.to_string().to_uppercase()
            ),
        ),
        (Method::POST, format!("{path}?extra=1")),
        (Method::POST, "/".into()),
        (Method::POST, "/api/v1/me".into()),
    ] {
        assert_isolated(
            harness
                .request(method, &candidate, Some("hooks.example.com"))
                .await,
        )
        .await;
    }
}

#[tokio::test]
async fn local_probe_authority_exposes_only_exact_get_probes() {
    let harness = Harness::new("solodock.example.com", "127.0.0.1:8080").await;
    let health = harness
        .request(Method::GET, "/healthz", Some("127.0.0.1:8080"))
        .await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        health.into_body().collect().await.unwrap().to_bytes(),
        r#"{"status":"ok"}"#
    );
    for (method, path) in [
        (Method::GET, "/"),
        (Method::GET, "/api/v1/me"),
        (Method::POST, "/healthz"),
        (Method::GET, "/healthz?extra=1"),
        (Method::HEAD, "/favicon.svg"),
    ] {
        assert_isolated(harness.request(method, path, Some("127.0.0.1:8080")).await).await;
    }
}

#[cfg(feature = "embed-ui")]
#[tokio::test]
async fn management_ui_and_local_favicon_follow_the_authority_matrix() {
    let harness = Harness::new("solodock.example.com", "127.0.0.1:8080").await;
    let management_ui = harness
        .request(Method::GET, "/", Some("solodock.example.com"))
        .await;
    assert_eq!(management_ui.status(), StatusCode::OK);
    assert!(
        management_ui.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let favicon = harness
        .request(Method::GET, "/favicon.svg", Some("127.0.0.1:8080"))
        .await;
    assert_eq!(favicon.status(), StatusCode::OK);
    assert_eq!(favicon.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert_isolated(
        harness
            .request(Method::GET, "/", Some("127.0.0.1:8080"))
            .await,
    )
    .await;
    assert_isolated(
        harness
            .request(Method::GET, "/favicon.svg", Some("hooks.example.com"))
            .await,
    )
    .await;
}

#[tokio::test]
async fn missing_duplicate_conflicting_invalid_and_forwarded_authorities_are_isolated() {
    let harness = Harness::new("solodock.example.com", "127.0.0.1:8080").await;
    assert_isolated(harness.request(Method::GET, "/healthz", None).await).await;
    assert_isolated(
        harness
            .request(Method::GET, "/healthz", Some("unknown.example.com"))
            .await,
    )
    .await;
    for invalid in [
        "user@solodock.example.com",
        "solodock.example.com/path",
        "solodock.example.com:+443",
        "[::1]:+8080",
    ] {
        assert_isolated(
            harness
                .request(Method::GET, "/healthz", Some(invalid))
                .await,
        )
        .await;
    }

    let mut duplicate = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    duplicate.headers_mut().append(
        header::HOST,
        HeaderValue::from_static("solodock.example.com"),
    );
    duplicate.headers_mut().append(
        header::HOST,
        HeaderValue::from_static("solodock.example.com"),
    );
    assert_isolated(harness.app.clone().oneshot(duplicate).await.unwrap()).await;

    assert_isolated(
        harness
            .request(
                Method::GET,
                "http://solodock.example.com/healthz",
                Some("hooks.example.com"),
            )
            .await,
    )
    .await;

    let forwarded_only = Request::builder()
        .uri("/healthz")
        .header("forwarded", "host=solodock.example.com")
        .header("x-forwarded-host", "solodock.example.com")
        .header("x-original-host", "solodock.example.com")
        .body(Body::empty())
        .unwrap();
    assert_isolated(harness.app.clone().oneshot(forwarded_only).await.unwrap()).await;

    let mut non_ascii = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    non_ascii
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_bytes(&[0xff]).unwrap());
    assert_isolated(harness.app.clone().oneshot(non_ascii).await.unwrap()).await;
}

#[tokio::test]
async fn uri_authority_and_canonical_ports_and_ip_forms_are_compared_exactly() {
    let harness = Harness::new("solodock.example.com:8443", "127.0.0.1:8080").await;
    assert_eq!(
        harness
            .authenticated(Method::GET, "/api/v1/me", Some("solodock.example.com:8443"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_isolated(
        harness
            .authenticated(Method::GET, "/api/v1/me", Some("solodock.example.com"))
            .await,
    )
    .await;
    assert_eq!(
        harness
            .authenticated(
                Method::GET,
                "http://SOLODOCK.EXAMPLE.COM:8443/api/v1/me",
                None,
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        harness
            .authenticated(
                Method::GET,
                "http://solodock.example.com:8443/api/v1/me",
                Some("SOLODOCK.EXAMPLE.COM:8443"),
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        harness
            .request(Method::GET, "/healthz", Some("127.0.0.1:8080"))
            .await
            .status(),
        StatusCode::OK
    );

    let ipv6 = Harness::new("solodock.example.com", "[::1]:8080").await;
    assert_eq!(
        ipv6.request(Method::GET, "/healthz", Some("[0:0:0:0:0:0:0:1]:8080"),)
            .await
            .status(),
        StatusCode::OK
    );

    let loopback_management = Harness::new("127.0.0.1:8080", "127.0.0.1:8080").await;
    assert_eq!(
        loopback_management
            .authenticated(Method::GET, "/api/v1/me", Some("127.0.0.1:8080"))
            .await
            .status(),
        StatusCode::OK
    );
}
