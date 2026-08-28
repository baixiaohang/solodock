pub mod auth;
pub mod middleware;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    middleware as axum_middleware,
    routing::{get, post},
};
use serde::Serialize;

use crate::auth::AuthService;

#[derive(Clone)]
pub struct AppState {
    pub auth: AuthService,
    pub public_origin: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/auth/bootstrap", post(auth::bootstrap))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/me", get(auth::me))
        .route("/api/v1/me/sessions/revoke-all", post(auth::revoke_all))
        .with_state(state)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum_middleware::from_fn(middleware::request_context))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}
