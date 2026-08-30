pub mod api;
pub mod app_store;
pub mod auth;
pub mod compose;
pub mod config;
pub mod db;
pub mod deploy;
pub mod docker;
pub mod domain;
pub mod error;
pub mod mutation;
pub mod registry;
pub mod security;
pub mod system;
pub mod telemetry;
pub mod webhook;

pub use api::{AppState, router};

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tempfile::tempdir;
    use tower::ServiceExt;

    use crate::{auth::AuthService, db::Database};

    #[tokio::test]
    async fn healthz_returns_json_without_authentication() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::open(&root.path().join("state.sqlite3"))
            .await
            .unwrap();
        let auth = AuthService::new(database, root.path().join("bootstrap.token"));
        let response = crate::router(crate::AppState::control_plane(
            auth,
            "https://example.com".into(),
            root.path().to_owned(),
        ))
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            r#"{"status":"ok"}"#
        );
    }
}
