use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Extension, Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response, Sse, sse::Event},
};
use futures_util::StreamExt;
use serde::Deserialize;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{AppState, auth::Authenticated};
use crate::{
    docker::{
        OwnedContainerError,
        events::Replay,
        logs::{LogCursor, LogFramer},
        models::{LogRequest, ProbeStatus},
        stats::StatsUpdate,
    },
    error::{ApiError, RequestId},
};

const HEARTBEAT_SECONDS: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Events,
    Logs,
    Stats,
}

#[derive(Clone, Default)]
pub struct StreamGate {
    counts: Arc<Mutex<GateCounts>>,
}

#[derive(Default)]
struct GateCounts {
    total: usize,
    sessions: HashMap<String, usize>,
    kinds: HashMap<StreamKind, usize>,
    apps: HashMap<(StreamKind, Uuid), usize>,
}

pub struct StreamPermit {
    gate: StreamGate,
    session_id: String,
    kind: StreamKind,
    app_id: Uuid,
}

impl StreamGate {
    pub const GLOBAL_LIMIT: usize = 24;

    pub fn active(&self) -> usize {
        self.counts
            .lock()
            .expect("stream gate mutex poisoned")
            .total
    }

    pub fn acquire(
        &self,
        session_id: String,
        kind: StreamKind,
        app_id: Uuid,
    ) -> Option<StreamPermit> {
        let (kind_limit, app_limit) = match kind {
            StreamKind::Events => (16, 4),
            StreamKind::Logs | StreamKind::Stats => (8, 2),
        };
        let mut counts = self.counts.lock().expect("stream gate mutex poisoned");
        if counts.total >= Self::GLOBAL_LIMIT
            || counts.sessions.get(&session_id).copied().unwrap_or(0) >= 8
            || counts.kinds.get(&kind).copied().unwrap_or(0) >= kind_limit
            || counts.apps.get(&(kind, app_id)).copied().unwrap_or(0) >= app_limit
        {
            return None;
        }
        counts.total += 1;
        *counts.sessions.entry(session_id.clone()).or_insert(0) += 1;
        *counts.kinds.entry(kind).or_insert(0) += 1;
        *counts.apps.entry((kind, app_id)).or_insert(0) += 1;
        drop(counts);
        Some(StreamPermit {
            gate: self.clone(),
            session_id,
            kind,
            app_id,
        })
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        let mut counts = self.gate.counts.lock().expect("stream gate mutex poisoned");
        counts.total = counts.total.saturating_sub(1);
        decrement(&mut counts.sessions, &self.session_id);
        decrement(&mut counts.kinds, &self.kind);
        decrement(&mut counts.apps, &(self.kind, self.app_id));
    }
}

fn decrement<K: std::hash::Hash + Eq + Clone>(counts: &mut HashMap<K, usize>, key: &K) {
    if let Some(value) = counts.get_mut(key) {
        *value = value.saturating_sub(1);
        if *value == 0 {
            counts.remove(key);
        }
    }
}

struct ConnectionGuard {
    _permit: StreamPermit,
    cancellation: CancellationToken,
    producer_abort: Option<tokio::task::AbortHandle>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(abort) = &self.producer_abort {
            abort.abort();
        }
    }
}

pub async fn events(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    authenticated: Authenticated,
) -> Result<Response, ApiError> {
    if state.observer.catalog.get(app_id).is_none() {
        return Err(ApiError::app_not_found(request_id));
    }
    let permit = state
        .stream_gate
        .acquire(authenticated.session.id.clone(), StreamKind::Events, app_id)
        .ok_or_else(|| ApiError::stream_limit(request_id))?;
    ensure_ready(&state, request_id).await?;
    state
        .observer
        .owned_container(app_id)
        .await
        .map_err(|error| owned_error(error, request_id))?;
    // Subscribe before reading the replay ring so an event cannot fall into the
    // gap between replay and the live receiver. Events present in both sources
    // are removed below by their process-unique event ID.
    let mut receiver = state.events.subscribe();
    let snapshot = state
        .observer
        .snapshot()
        .await
        .apps
        .into_iter()
        .find(|app| app.id == app_id)
        .expect("catalog app exists");
    let snapshot = serde_json::to_string(&snapshot).expect("snapshot serializes");
    let replay = state
        .events
        .replay(
            app_id,
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok()),
        )
        .await;
    let auth = state.auth.clone();
    let token = authenticated.token;
    let cancellation = state.shutdown.child_token();
    let guard = ConnectionGuard {
        _permit: permit,
        cancellation: cancellation.clone(),
        producer_abort: None,
    };
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut replayed_ids = HashSet::new();
        match replay {
            Replay::Reset => {
                yield Ok::<Event, Infallible>(Event::default().retry(std::time::Duration::from_secs(3)).event("reset").data("{}"));
                yield Ok(Event::default().event("snapshot").data(snapshot));
            }
            Replay::Events(events) => {
                yield Ok::<Event, Infallible>(Event::default().retry(std::time::Duration::from_secs(3)).event("snapshot").data(snapshot));
                for event in events {
                    replayed_ids.insert(event.id.clone());
                    yield Ok(Event::default().id(event.id.clone()).event("container_event").data(serde_json::to_string(&event).expect("event serializes")));
                }
            }
        }
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECONDS));
        heartbeat.tick().await;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = heartbeat.tick() => {
                    if auth.authenticate(token.expose()).await.is_err() {
                        yield Ok(Event::default().event("stream_error").data("{\"code\":\"SESSION_EXPIRED\"}"));
                        break;
                    }
                    yield Ok(Event::default().comment("heartbeat"));
                },
                event = receiver.recv() => match event {
                    Ok(event) if event.app_id == app_id && !replayed_ids.remove(&event.id) => yield Ok(Event::default().id(event.id.clone()).event("container_event").data(serde_json::to_string(&event).expect("event serializes"))),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(Event::default().event("stream_error").data("{\"code\":\"SLOW_CONSUMER\"}"));
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    };
    Ok(sse_response(Sse::new(stream).into_response()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsQuery {
    tail: Option<usize>,
    since: Option<String>,
}

pub async fn logs(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    headers: HeaderMap,
    query: Result<Query<LogsQuery>, QueryRejection>,
    authenticated: Authenticated,
) -> Result<Response, ApiError> {
    if state.observer.catalog.get(app_id).is_none() {
        return Err(ApiError::app_not_found(request_id));
    }
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    let tail = query.tail.unwrap_or(200);
    if !(1..=500).contains(&tail) {
        return Err(ApiError::invalid_query(request_id));
    }
    let now = OffsetDateTime::now_utc();
    let mut since = query
        .since
        .as_deref()
        .map(|value| OffsetDateTime::parse(value, &Rfc3339))
        .transpose()
        .map_err(|_| ApiError::invalid_query(request_id))?;
    if since.is_some_and(|value| {
        value < now - Duration::hours(24) || value > now + Duration::minutes(5)
    }) {
        return Err(ApiError::invalid_query(request_id));
    }
    let cursor = match headers.get("last-event-id") {
        Some(value) => Some(
            value
                .to_str()
                .ok()
                .and_then(LogCursor::parse)
                .ok_or_else(|| ApiError::invalid_query(request_id))?,
        ),
        None => None,
    };
    if let Some(cursor) = cursor {
        let cursor_time = OffsetDateTime::from_unix_timestamp_nanos(cursor.unix_nanos)
            .map_err(|_| ApiError::invalid_query(request_id))?;
        if cursor_time < now - Duration::hours(24) || cursor_time > now + Duration::minutes(5) {
            return Err(ApiError::invalid_query(request_id));
        }
        since = Some(since.map_or(cursor_time, |value| value.max(cursor_time)));
    }
    let permit = state
        .stream_gate
        .acquire(authenticated.session.id.clone(), StreamKind::Logs, app_id)
        .ok_or_else(|| ApiError::stream_limit(request_id))?;
    ensure_ready(&state, request_id).await?;
    let container = state
        .observer
        .owned_container(app_id)
        .await
        .map_err(|error| owned_error(error, request_id))?;
    let request = LogRequest {
        tail,
        since_seconds: since.map(OffsetDateTime::unix_timestamp),
    };
    let mut docker_stream = state
        .observer
        .api
        .logs(&container.id, request)
        .await
        .map_err(|error| ApiError::docker(request_id, error.public_code()))?;
    let (sender, mut receiver) = mpsc::channel(128);
    let (terminal_sender, mut terminal) = watch::channel(None::<&'static str>);
    let cancellation = state.shutdown.child_token();
    let producer_cancellation = cancellation.clone();
    let redactor = state.redactor.clone();
    let producer = state.stream_tasks.spawn(async move {
        let mut framer = LogFramer::new(redactor);
        loop {
            tokio::select! {
                () = producer_cancellation.cancelled() => break,
                item = docker_stream.next() => match item {
                    Some(Ok(chunk)) => {
                        for event in framer.push(chunk.stream, &chunk.bytes) {
                            if sender.try_send(event).is_err() {
                                terminal_sender.send_replace(Some("SLOW_CONSUMER"));
                                return;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        terminal_sender.send_replace(Some(error.public_code()));
                        break;
                    }
                    None => {
                        for event in framer.finish() {
                            if sender.try_send(event).is_err() {
                                terminal_sender.send_replace(Some("SLOW_CONSUMER"));
                                return;
                            }
                        }
                        terminal_sender.send_replace(Some("CONTAINER_CHANGED"));
                        break;
                    }
                }
            }
        }
    });
    let auth = state.auth.clone();
    let token = authenticated.token;
    let guard = ConnectionGuard {
        _permit: permit,
        cancellation: cancellation.clone(),
        producer_abort: Some(producer.abort_handle()),
    };
    let stream = async_stream::stream! {
        let _guard = guard;
        yield Ok::<Event, Infallible>(Event::default().retry(std::time::Duration::from_secs(3)).comment("connected"));
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECONDS));
        heartbeat.tick().await;
        let mut last_nanos = i128::MIN;
        let mut ordinal = 0u32;
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break,
                _ = heartbeat.tick() => {
                    if auth.authenticate(token.expose()).await.is_err() {
                        yield Ok(Event::default().event("stream_error").data("{\"code\":\"SESSION_EXPIRED\"}"));
                        break;
                    }
                    yield Ok(Event::default().comment("heartbeat"));
                }
                event = receiver.recv() => match event {
                    Some(event) => {
                        let nanos = OffsetDateTime::parse(&event.timestamp, &Rfc3339).map(OffsetDateTime::unix_timestamp_nanos).unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp_nanos());
                        if cursor.is_some_and(|cursor| nanos < cursor.unix_nanos) { continue; }
                        if nanos == last_nanos { ordinal = ordinal.saturating_add(1); } else { last_nanos = nanos; ordinal = 0; }
                        let id = LogCursor { unix_nanos: nanos, ordinal }.encode();
                        yield Ok(Event::default().id(id).event("log").data(serde_json::to_string(&event).expect("log serializes")));
                    }
                    None => {
                        let code = *terminal.borrow();
                        if let Some(code) = code {
                            yield Ok(Event::default().event("stream_error").data(format!("{{\"code\":\"{code}\"}}")));
                        }
                        break;
                    },
                },
                changed = terminal.changed() => {
                    let code = if changed.is_ok() { *terminal.borrow() } else { None };
                    if let Some(code) = code {
                        yield Ok(Event::default().event("stream_error").data(format!("{{\"code\":\"{code}\"}}")));
                    }
                    break;
                },
            }
        }
    };
    Ok(sse_response(Sse::new(stream).into_response()))
}

pub async fn stats(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(app_id): Path<Uuid>,
    authenticated: Authenticated,
) -> Result<Response, ApiError> {
    if state.observer.catalog.get(app_id).is_none() {
        return Err(ApiError::app_not_found(request_id));
    }
    let permit = state
        .stream_gate
        .acquire(authenticated.session.id.clone(), StreamKind::Stats, app_id)
        .ok_or_else(|| ApiError::stream_limit(request_id))?;
    ensure_ready(&state, request_id).await?;
    let container = state
        .observer
        .owned_container(app_id)
        .await
        .map_err(|error| owned_error(error, request_id))?;
    let mut subscription = state.stats.subscribe(container.id).await;
    let auth = state.auth.clone();
    let token = authenticated.token;
    let cancellation = state.shutdown.child_token();
    let guard = ConnectionGuard {
        _permit: permit,
        cancellation: cancellation.clone(),
        producer_abort: None,
    };
    let stream = async_stream::stream! {
        let _guard = guard;
        yield Ok::<Event, Infallible>(Event::default().retry(std::time::Duration::from_secs(3)).comment("connected"));
        let initial = subscription.receiver.borrow().clone();
        if let Some(update) = initial {
            match update {
                StatsUpdate::Sample(sample) => yield Ok(Event::default().event("stats").data(serde_json::to_string(&sample).expect("stats serializes"))),
                StatsUpdate::Error(code) => {
                    yield Ok(Event::default().event("stream_error").data(format!("{{\"code\":\"{code}\"}}")));
                    return;
                }
            }
        }
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECONDS));
        heartbeat.tick().await;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = heartbeat.tick() => {
                    if auth.authenticate(token.expose()).await.is_err() {
                        yield Ok(Event::default().event("stream_error").data("{\"code\":\"SESSION_EXPIRED\"}"));
                        break;
                    }
                    yield Ok(Event::default().comment("heartbeat"));
                }
                changed = subscription.receiver.changed() => {
                    if changed.is_err() { break; }
                    let update = subscription.receiver.borrow().clone();
                    match update {
                        Some(StatsUpdate::Sample(sample)) => yield Ok(Event::default().event("stats").data(serde_json::to_string(&sample).expect("stats serializes"))),
                        Some(StatsUpdate::Error(code)) => {
                            yield Ok(Event::default().event("stream_error").data(format!("{{\"code\":\"{code}\"}}")));
                            break;
                        }
                        None => {}
                    }
                }
            }
        }
    };
    Ok(sse_response(Sse::new(stream).into_response()))
}

async fn ensure_ready(state: &AppState, request_id: RequestId) -> Result<(), ApiError> {
    let probe = state.observer.supervisor.current().await;
    if probe.status == ProbeStatus::Ready {
        return Ok(());
    }
    Err(ApiError::docker(
        request_id,
        probe.error_code.unwrap_or("DOCKER_UNAVAILABLE"),
    ))
}

fn owned_error(error: OwnedContainerError, request_id: RequestId) -> ApiError {
    match error {
        OwnedContainerError::AppNotFound => ApiError::app_not_found(request_id),
        OwnedContainerError::Missing => ApiError::docker(request_id, "APP_CONTAINER_NOT_FOUND"),
        OwnedContainerError::Ambiguous => ApiError::docker(request_id, "APP_CONTAINER_AMBIGUOUS"),
        OwnedContainerError::Invalid | OwnedContainerError::Changed => {
            ApiError::docker(request_id, "APP_CONTAINER_INVALID")
        }
        OwnedContainerError::Docker(kind) => ApiError::docker(
            request_id,
            crate::docker::models::DockerError::new(kind).public_code(),
        ),
    }
}

fn sse_response(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_enforces_per_app_and_releases_on_drop() {
        let gate = StreamGate::default();
        let app = Uuid::new_v4();
        let first = gate
            .acquire("session".into(), StreamKind::Logs, app)
            .unwrap();
        let second = gate
            .acquire("session".into(), StreamKind::Logs, app)
            .unwrap();
        assert!(
            gate.acquire("session".into(), StreamKind::Logs, app)
                .is_none()
        );
        assert_eq!(gate.active(), 2);
        drop(first);
        assert!(
            gate.acquire("session".into(), StreamKind::Logs, app)
                .is_some()
        );
        drop(second);
    }
}
