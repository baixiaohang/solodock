use std::net::SocketAddr;

use axum::{
    Json,
    extract::{ConnectInfo, Extension, FromRequestParts, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use super::AppState;
use crate::{
    auth::{AuthenticatedSession, password::PasswordService},
    error::{ApiError, RequestId},
    security::{origin::matches_public_origin, secret::SecretValue},
};

const SESSION_COOKIE: &str = "__Host-solodock_session";
const CSRF_COOKIE: &str = "__Host-solodock_csrf";

pub struct Authenticated {
    pub token: SecretValue,
    pub session: AuthenticatedSession,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let request_id = parts
            .extensions
            .get::<RequestId>()
            .copied()
            .unwrap_or(RequestId(uuid::Uuid::nil()));
        let token = cookie_value(&parts.headers, SESSION_COOKIE).ok_or_else(|| {
            ApiError::from_auth(crate::auth::AuthError::SessionRequired, request_id)
        })?;
        let session = state
            .auth
            .authenticate(&token)
            .await
            .map_err(|error| ApiError::from_auth(error, request_id))?;
        Ok(Self {
            token: SecretValue::new(token),
            session,
        })
    }
}

pub struct MutationAuthenticated(pub Authenticated);

impl FromRequestParts<AppState> for MutationAuthenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let request_id = parts
            .extensions
            .get::<RequestId>()
            .copied()
            .unwrap_or(RequestId(uuid::Uuid::nil()));
        validate_required_origin(state, &parts.headers, request_id)?;
        validate_csrf(&parts.headers, request_id)?;
        Ok(Self(Authenticated::from_request_parts(parts, state).await?))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRequest {
    bootstrap_token: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct MeResponse {
    username: &'static str,
    session: MeSession,
}

#[derive(Serialize)]
pub struct MeSession {
    created_at: String,
    expires_at: String,
}

pub async fn bootstrap(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<BootstrapRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    if !peer.ip().is_loopback() {
        return Err(ApiError::bootstrap_local_only(request_id));
    }
    validate_optional_origin(&state, &headers, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    PasswordService::validate(&payload.password).map_err(|_| ApiError::validation(request_id))?;
    state
        .auth
        .bootstrap(payload.bootstrap_token, payload.password, request_id.0)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn login(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    payload: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    validate_required_origin(&state, &headers, request_id)?;
    let Json(payload) = payload.map_err(|_| ApiError::validation(request_id))?;
    let session = state
        .auth
        .login(&payload.username, payload.password, request_id.0)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    append_cookie(
        response.headers_mut(),
        session_cookie(session.session_token.expose()),
    );
    append_cookie(
        response.headers_mut(),
        csrf_cookie(session.csrf_token.expose()),
    );
    Ok(response)
}

pub async fn me(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    authenticated: Result<Authenticated, ApiError>,
) -> Result<Json<MeResponse>, ApiError> {
    if !state
        .auth
        .is_initialized()
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?
    {
        return Err(ApiError::from_auth(
            crate::auth::AuthError::SetupRequired,
            request_id,
        ));
    }
    let session = authenticated?.session;
    Ok(Json(MeResponse {
        username: "admin",
        session: MeSession {
            created_at: session.created_at.format(&Rfc3339).expect("valid UTC time"),
            expires_at: session.expires_at.format(&Rfc3339).expect("valid UTC time"),
        },
    }))
}

pub async fn logout(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    validate_required_origin(&state, &headers, request_id)?;
    validate_csrf(&headers, request_id)?;
    let token = cookie_value(&headers, SESSION_COOKIE)
        .ok_or_else(|| ApiError::from_auth(crate::auth::AuthError::SessionRequired, request_id))?;
    state
        .auth
        .authenticate(&token)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    state
        .auth
        .logout(&token, request_id.0)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    Ok(expired_cookie_response())
}

pub async fn revoke_all(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    validate_required_origin(&state, &headers, request_id)?;
    validate_csrf(&headers, request_id)?;
    let token = cookie_value(&headers, SESSION_COOKIE)
        .ok_or_else(|| ApiError::from_auth(crate::auth::AuthError::SessionRequired, request_id))?;
    state
        .auth
        .authenticate(&token)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    state
        .auth
        .revoke_all(request_id.0)
        .await
        .map_err(|error| ApiError::from_auth(error, request_id))?;
    Ok(expired_cookie_response())
}

fn validate_required_origin(
    state: &AppState,
    headers: &HeaderMap,
    request_id: RequestId,
) -> Result<(), ApiError> {
    let supplied = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::origin(request_id))?;
    if !matches_public_origin(&state.public_origin, supplied) {
        return Err(ApiError::origin(request_id));
    }
    Ok(())
}

fn validate_optional_origin(
    state: &AppState,
    headers: &HeaderMap,
    request_id: RequestId,
) -> Result<(), ApiError> {
    if headers.contains_key(header::ORIGIN) {
        validate_required_origin(state, headers, request_id)?;
    }
    Ok(())
}

fn validate_csrf(headers: &HeaderMap, request_id: RequestId) -> Result<(), ApiError> {
    let cookie = cookie_value(headers, CSRF_COOKIE).ok_or_else(|| ApiError::csrf(request_id))?;
    let supplied = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::csrf(request_id))?;
    let cookie = SecretValue::new(cookie);
    if !cookie.constant_time_eq(supplied) {
        return Err(ApiError::csrf(request_id));
    }
    Ok(())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| Cookie::parse(part.trim().to_owned()).ok())
        .find(|cookie| cookie.name() == name)
        .map(|cookie| cookie.value().to_owned())
}

fn session_cookie(value: &str) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, value.to_owned()))
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Strict)
        .build()
}

fn csrf_cookie(value: &str) -> Cookie<'static> {
    Cookie::build((CSRF_COOKIE, value.to_owned()))
        .path("/")
        .secure(true)
        .same_site(SameSite::Strict)
        .build()
}

fn expired_cookie(name: &'static str, http_only: bool) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .secure(true)
        .http_only(http_only)
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::ZERO)
        .build()
}

fn append_cookie(headers: &mut HeaderMap, cookie: Cookie<'static>) {
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("generated cookie is a valid header"),
    );
}

fn expired_cookie_response() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    append_cookie(response.headers_mut(), expired_cookie(SESSION_COOKIE, true));
    append_cookie(response.headers_mut(), expired_cookie(CSRF_COOKIE, false));
    response
}
