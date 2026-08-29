use std::{collections::VecDeque, sync::Arc, time::Duration};

use futures_util::StreamExt;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    AppCatalog,
    models::{AppEvent, DockerReadApi, EventKind, RawDockerEvent},
    ownership::{claimed_app_id, valid_container_id, validate_identity},
};

const RING_LIMIT: usize = 512;

#[derive(Clone)]
pub struct DockerEventHub {
    boot_id: Uuid,
    ring: Arc<Mutex<VecDeque<AppEvent>>>,
    sender: broadcast::Sender<AppEvent>,
}

pub enum Replay {
    Events(Vec<AppEvent>),
    Reset,
}

impl DockerEventHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(128);
        Self {
            boot_id: Uuid::new_v4(),
            ring: Arc::new(Mutex::new(VecDeque::with_capacity(RING_LIMIT))),
            sender,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }

    pub async fn replay(&self, app_id: Uuid, last_id: Option<&str>) -> Replay {
        let Some(last_id) = last_id else {
            return Replay::Events(Vec::new());
        };
        let Some((boot, sequence)) = parse_event_id(last_id) else {
            return Replay::Reset;
        };
        if boot != self.boot_id {
            return Replay::Reset;
        }
        let ring = self.ring.lock().await;
        if ring
            .back()
            .and_then(|event| parse_event_id(&event.id))
            .is_some_and(|(_, newest)| sequence > newest)
        {
            return Replay::Reset;
        }
        if ring
            .front()
            .and_then(|event| parse_event_id(&event.id))
            .is_some_and(|(_, oldest)| sequence < oldest.saturating_sub(1))
        {
            return Replay::Reset;
        }
        Replay::Events(
            ring.iter()
                .filter(|event| {
                    event.app_id == app_id
                        && parse_event_id(&event.id).is_some_and(|(_, current)| current > sequence)
                })
                .cloned()
                .collect(),
        )
    }

    pub fn start(
        &self,
        api: Arc<dyn DockerReadApi>,
        catalog: AppCatalog,
        cancellation: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            let delays = [1, 2, 5, 10, 30];
            let mut sequence = 0u64;
            let mut reconnect_attempt = 0usize;
            while !cancellation.is_cancelled() {
                let mut stream = match api.events().await {
                    Ok(stream) => stream,
                    Err(_) => {
                        if !wait_for_reconnect(&cancellation, &delays, &mut reconnect_attempt).await
                        {
                            break;
                        }
                        continue;
                    }
                };
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return,
                        item = stream.next() => match item {
                            Some(Ok(raw)) => {
                                // Receiving an item proves the stream was live;
                                // the next disconnect starts a fresh backoff.
                                reconnect_attempt = 0;
                                if let Some((app_id, mut event)) = map_event(&catalog, raw) {
                                    sequence = sequence.saturating_add(1);
                                    event.id = format!("{}:{sequence}", hub.boot_id);
                                    event.app_id = app_id;
                                    let mut ring = hub.ring.lock().await;
                                    if ring.len() == RING_LIMIT { ring.pop_front(); }
                                    ring.push_back(event.clone());
                                    drop(ring);
                                    let _ = hub.sender.send(event);
                                }
                            }
                            Some(Err(_)) | None => break,
                        }
                    }
                }
                if !wait_for_reconnect(&cancellation, &delays, &mut reconnect_attempt).await {
                    break;
                }
            }
        })
    }
}

async fn wait_for_reconnect(
    cancellation: &CancellationToken,
    delays: &[u64],
    attempt: &mut usize,
) -> bool {
    let seconds = delays[(*attempt).min(delays.len() - 1)];
    *attempt = attempt.saturating_add(1);
    tokio::select! {
        () = cancellation.cancelled() => false,
        () = tokio::time::sleep(Duration::from_secs(seconds)) => true,
    }
}

impl Default for DockerEventHub {
    fn default() -> Self {
        Self::new()
    }
}

fn map_event(catalog: &AppCatalog, raw: RawDockerEvent) -> Option<(Uuid, AppEvent)> {
    if !valid_container_id(&raw.container_id) {
        return None;
    }
    let app_id = claimed_app_id(&raw.labels)?;
    let app = catalog.get(app_id)?;
    validate_identity(&raw.labels, &app)?;
    let kind = match raw.action.as_str() {
        "create" => EventKind::Created,
        "start" => EventKind::Started,
        "stop" | "kill" => EventKind::Stopped,
        "die" => EventKind::Died,
        "destroy" => EventKind::Destroyed,
        "pause" => EventKind::Paused,
        "unpause" => EventKind::Unpaused,
        "restart" => EventKind::Restarted,
        value if value.starts_with("health_status") => EventKind::HealthChanged,
        "rename" => EventKind::Renamed,
        _ => EventKind::Unknown,
    };
    Some((
        app_id,
        AppEvent {
            id: String::new(),
            kind,
            app_id,
            container_id: raw.container_id,
            occurred_at: raw.occurred_at,
            exit_code: raw.exit_code,
        },
    ))
}

fn parse_event_id(value: &str) -> Option<(Uuid, u64)> {
    let (boot, sequence) = value.split_once(':')?;
    Some((boot.parse().ok()?, sequence.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use futures_util::stream;
    use time::OffsetDateTime;

    use super::*;
    use crate::{
        app_store::recovery::{RecoveredApp, RecoveryReport},
        docker::{models::*, ownership::*},
    };

    struct DisconnectingApi {
        calls: AtomicUsize,
        item_error: bool,
    }

    #[async_trait]
    impl DockerReadApi for DisconnectingApi {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }

        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }

        async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }

        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.item_error {
                Ok(Box::pin(stream::iter([Err(DockerError::new(
                    DockerErrorKind::Unavailable,
                ))])))
            } else {
                Ok(Box::pin(stream::empty()))
            }
        }

        async fn logs(
            &self,
            _id: &str,
            _request: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }

        async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }
    }

    fn catalog() -> (AppCatalog, Uuid, Uuid, HashMap<String, String>) {
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let app = RecoveredApp {
            app_id,
            slug: "example".into(),
            display_name: "Example".into(),
            project_name: "solodock-example".into(),
            active_release_id: Some(release_id),
            active_image_ref: Some(format!("example@sha256:{}", "a".repeat(64))),
            active_config_revision: None,
            active_config_sha256: None,
            discovery_image_ref: None,
            draft_revision: None,
            draft_config_sha256: None,
            desired_state: crate::domain::DesiredState::Stopped,
            auto_deploy_enabled: false,
            poll_interval_seconds: 300,
            last_operation_id: None,
            draft: None,
            source_updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let labels = HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (SCHEMA_LABEL.into(), "1".into()),
            (APP_ID_LABEL.into(), app_id.to_string()),
            (RELEASE_ID_LABEL.into(), release_id.to_string()),
            (PROJECT_LABEL.into(), app.project_name.clone()),
            (SERVICE_LABEL.into(), "app".into()),
            (ONEOFF_LABEL.into(), "False".into()),
        ]);
        (
            AppCatalog::from_recovery(&RecoveryReport {
                valid_apps: vec![app],
                issues: vec![],
            }),
            app_id,
            release_id,
            labels,
        )
    }

    #[tokio::test]
    async fn replay_resets_invalid_boot_old_and_future_cursors() {
        let hub = DockerEventHub::new();
        let (_, app_id, _, _) = catalog();
        let event = AppEvent {
            id: format!("{}:2", hub.boot_id),
            kind: EventKind::Started,
            app_id,
            container_id: "id".into(),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            exit_code: None,
        };
        hub.ring.lock().await.push_back(event.clone());
        assert!(
            matches!(hub.replay(app_id, Some(&format!("{}:1", hub.boot_id))).await, Replay::Events(events) if events == vec![event])
        );
        assert!(matches!(
            hub.replay(app_id, Some(&format!("{}:3", hub.boot_id)))
                .await,
            Replay::Reset
        ));
        assert!(matches!(
            hub.replay(app_id, Some(&format!("{}:1", Uuid::new_v4())))
                .await,
            Replay::Reset
        ));
        assert!(matches!(
            hub.replay(app_id, Some("invalid")).await,
            Replay::Reset
        ));
    }

    #[test]
    fn event_mapping_drops_invalid_ownership_and_never_passes_raw_action() {
        let (catalog, app_id, _, labels) = catalog();
        let raw = RawDockerEvent {
            container_id: "d".repeat(64),
            action: "future-action secret".into(),
            labels: labels.clone(),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            exit_code: None,
        };
        assert!(matches!(
            map_event(&catalog, raw).unwrap().1.kind,
            EventKind::Unknown
        ));
        let mut invalid = labels;
        invalid.insert(PROJECT_LABEL.into(), "foreign".into());
        assert!(
            map_event(
                &catalog,
                RawDockerEvent {
                    container_id: "d".repeat(64),
                    action: "start".into(),
                    labels: invalid,
                    occurred_at: OffsetDateTime::UNIX_EPOCH,
                    exit_code: None
                }
            )
            .is_none()
        );
        assert_eq!(app_id, catalog.snapshot().apps[0].id);
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_eof_and_item_error_use_bounded_cancellable_backoff() {
        for item_error in [false, true] {
            let api = Arc::new(DisconnectingApi {
                calls: AtomicUsize::new(0),
                item_error,
            });
            let cancellation = CancellationToken::new();
            let handle = DockerEventHub::new().start(
                api.clone(),
                AppCatalog::default(),
                cancellation.clone(),
            );
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            assert_eq!(api.calls.load(Ordering::SeqCst), 1);

            tokio::time::advance(Duration::from_millis(999)).await;
            tokio::task::yield_now().await;
            assert_eq!(api.calls.load(Ordering::SeqCst), 1);
            tokio::time::advance(Duration::from_millis(1)).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            assert_eq!(api.calls.load(Ordering::SeqCst), 2);
            tokio::time::advance(Duration::from_secs(2)).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            assert_eq!(api.calls.load(Ordering::SeqCst), 3);

            cancellation.cancel();
            handle.await.unwrap();
            let stopped_at = api.calls.load(Ordering::SeqCst);
            tokio::time::advance(Duration::from_secs(60)).await;
            assert_eq!(api.calls.load(Ordering::SeqCst), stopped_at);
        }
    }
}
