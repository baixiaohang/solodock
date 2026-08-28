use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::sync::{Mutex, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::models::{DockerReadApi, RawStats, StatsSample};

#[derive(Clone, Debug)]
pub enum StatsUpdate {
    Sample(StatsSample),
    Error(&'static str),
}

#[derive(Clone)]
pub struct StatsHub {
    inner: Arc<StatsHubInner>,
}

struct StatsHubInner {
    api: Arc<dyn DockerReadApi>,
    entries: Mutex<HashMap<String, Arc<StatsEntry>>>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

struct StatsEntry {
    sender: watch::Sender<Option<StatsUpdate>>,
    subscribers: AtomicUsize,
    cancellation: CancellationToken,
}

pub struct StatsSubscription {
    pub receiver: watch::Receiver<Option<StatsUpdate>>,
    entry: Arc<StatsEntry>,
    hub: Weak<StatsHubInner>,
    container_id: String,
}

impl StatsHub {
    pub fn new(
        api: Arc<dyn DockerReadApi>,
        shutdown: CancellationToken,
        tasks: TaskTracker,
    ) -> Self {
        Self {
            inner: Arc::new(StatsHubInner {
                api,
                entries: Mutex::new(HashMap::new()),
                shutdown,
                tasks,
            }),
        }
    }

    pub async fn subscribe(&self, container_id: String) -> StatsSubscription {
        let mut entries = self.inner.entries.lock().await;
        let entry = if let Some(entry) = entries.get(&container_id) {
            entry.clone()
        } else {
            let (sender, _) = watch::channel(None);
            let entry = Arc::new(StatsEntry {
                sender,
                subscribers: AtomicUsize::new(0),
                cancellation: self.inner.shutdown.child_token(),
            });
            entries.insert(container_id.clone(), entry.clone());
            spawn_producer(
                &self.inner.tasks,
                self.inner.api.clone(),
                container_id.clone(),
                entry.clone(),
            );
            entry
        };
        entry.subscribers.fetch_add(1, Ordering::SeqCst);
        StatsSubscription {
            receiver: entry.sender.subscribe(),
            entry,
            hub: Arc::downgrade(&self.inner),
            container_id,
        }
    }
}

fn spawn_producer(
    tasks: &TaskTracker,
    api: Arc<dyn DockerReadApi>,
    container_id: String,
    entry: Arc<StatsEntry>,
) {
    tasks.spawn(async move {
        let mut stream = match api.stats(&container_id).await {
            Ok(stream) => stream,
            Err(error) => {
                entry
                    .sender
                    .send_replace(Some(StatsUpdate::Error(error.public_code())));
                return;
            }
        };
        let mut last_published: Option<Instant> = None;
        loop {
            tokio::select! {
                () = entry.cancellation.cancelled() => break,
                item = stream.next() => match item {
                    Some(Ok(raw)) => {
                        let now = Instant::now();
                        if last_published.is_some_and(|last| now.duration_since(last) < Duration::from_secs(1)) { continue; }
                        last_published = Some(now);
                        entry.sender.send_replace(Some(StatsUpdate::Sample(calculate(raw))));
                    }
                    Some(Err(error)) => {
                        entry.sender.send_replace(Some(StatsUpdate::Error(error.public_code())));
                        break;
                    }
                    None => {
                        entry.sender.send_replace(Some(StatsUpdate::Error("CONTAINER_CHANGED")));
                        break;
                    }
                }
            }
        }
    });
}

impl Drop for StatsSubscription {
    fn drop(&mut self) {
        if self.entry.subscribers.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let entry = self.entry.clone();
        let hub = self.hub.clone();
        let id = self.container_id.clone();
        let Some(hub) = hub.upgrade() else { return };
        let shutdown = hub.shutdown.clone();
        let tasks = hub.tasks.clone();
        tasks.spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            // Removal, zero-subscriber recheck, and cancellation are one
            // critical section with subscribe(). A new subscriber therefore
            // either keeps this live entry or creates a fresh producer.
            let mut entries = hub.entries.lock().await;
            if entry.subscribers.load(Ordering::SeqCst) == 0
                && entries
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                entries.remove(&id);
                entry.cancellation.cancel();
            }
        });
    }
}

pub fn calculate(raw: RawStats) -> StatsSample {
    let cpu_percent = match (
        raw.cpu_total,
        raw.previous_cpu_total,
        raw.system_cpu_total,
        raw.previous_system_cpu_total,
        raw.online_cpus,
    ) {
        (Some(cpu), Some(previous_cpu), Some(system), Some(previous_system), Some(cpus))
            if cpu >= previous_cpu && system > previous_system && cpus > 0 =>
        {
            let value = (cpu - previous_cpu) as f64 / (system - previous_system) as f64
                * cpus as f64
                * 100.0;
            value.is_finite().then_some(value)
        }
        _ => None,
    };
    let memory_percent = match (raw.memory_usage, raw.memory_limit) {
        (Some(usage), Some(limit)) if limit > 0 => {
            let value = usage as f64 / limit as f64 * 100.0;
            value.is_finite().then_some(value)
        }
        _ => None,
    };
    let (network_rx_bytes, network_tx_bytes) = if raw.networks.is_empty() {
        (None, None)
    } else {
        let mut rx = Some(0u64);
        let mut tx = Some(0u64);
        for (sample_rx, sample_tx) in raw.networks {
            rx = rx
                .zip(sample_rx)
                .map(|(sum, value)| sum.saturating_add(value));
            tx = tx
                .zip(sample_tx)
                .map(|(sum, value)| sum.saturating_add(value));
        }
        (rx, tx)
    };
    StatsSample {
        observed_at: raw.observed_at,
        cpu_percent,
        memory_usage_bytes: raw.memory_usage,
        memory_limit_bytes: raw.memory_limit,
        memory_percent,
        network_rx_bytes,
        network_tx_bytes,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use async_trait::async_trait;
    use futures_util::stream;
    use time::OffsetDateTime;

    use crate::docker::models::{
        ContainerRecord, DockerError, DockerErrorKind, DockerStream, LogChunk, LogRequest,
        ProbeSnapshot, RawDockerEvent,
    };

    struct FakeStatsApi(AtomicUsize);

    #[async_trait]
    impl DockerReadApi for FakeStatsApi {
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
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }
        async fn logs(
            &self,
            _id: &str,
            _request: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            Err(DockerError::new(DockerErrorKind::Unavailable))
        }
        async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::pending()))
        }
    }

    #[test]
    fn stats_math_rejects_invalid_deltas_and_saturates_networks() {
        let base = RawStats {
            observed_at: OffsetDateTime::UNIX_EPOCH,
            ..Default::default()
        };
        assert!(calculate(base.clone()).cpu_percent.is_none());
        let zero = RawStats {
            cpu_total: Some(2),
            previous_cpu_total: Some(1),
            system_cpu_total: Some(2),
            previous_system_cpu_total: Some(2),
            online_cpus: Some(2),
            memory_usage: Some(2),
            memory_limit: Some(0),
            ..base.clone()
        };
        let sample = calculate(zero);
        assert!(sample.cpu_percent.is_none());
        assert!(sample.memory_percent.is_none());
        let valid = RawStats {
            cpu_total: Some(20),
            previous_cpu_total: Some(10),
            system_cpu_total: Some(110),
            previous_system_cpu_total: Some(100),
            online_cpus: Some(2),
            memory_usage: Some(25),
            memory_limit: Some(100),
            networks: vec![(Some(u64::MAX), Some(3)), (Some(5), Some(4))],
            ..base
        };
        let sample = calculate(valid);
        assert_eq!(sample.cpu_percent, Some(200.0));
        assert_eq!(sample.memory_percent, Some(25.0));
        assert_eq!(sample.network_rx_bytes, Some(u64::MAX));
        assert_eq!(sample.network_tx_bytes, Some(7));
    }

    #[tokio::test]
    async fn subscribers_share_one_producer_and_last_drop_removes_it() {
        tokio::time::pause();
        let api = Arc::new(FakeStatsApi(AtomicUsize::new(0)));
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let hub = StatsHub::new(api.clone(), shutdown, tasks);
        let first = hub.subscribe("container".into()).await;
        let second = hub.subscribe("container".into()).await;
        tokio::task::yield_now().await;
        assert_eq!(api.0.load(Ordering::SeqCst), 1);
        drop(first);
        drop(second);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        let _third = hub.subscribe("container".into()).await;
        tokio::task::yield_now().await;
        assert_eq!(api.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_and_resubscribe_never_return_a_cancelled_entry() {
        let api = Arc::new(FakeStatsApi(AtomicUsize::new(0)));
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let hub = StatsHub::new(api, shutdown.clone(), tasks.clone());
        let first = hub.subscribe("container".into()).await;
        drop(first);

        tokio::time::advance(Duration::from_secs(5)).await;
        let second = hub.subscribe("container".into()).await;
        tokio::task::yield_now().await;
        assert!(!second.entry.cancellation.is_cancelled());
        assert!(
            hub.inner
                .entries
                .lock()
                .await
                .get("container")
                .is_some_and(|current| Arc::ptr_eq(current, &second.entry))
        );

        drop(second);
        shutdown.cancel();
        tasks.close();
        tasks.wait().await;
    }

    #[tokio::test]
    async fn global_shutdown_collects_pending_producer_and_cleanup() {
        let api = Arc::new(FakeStatsApi(AtomicUsize::new(0)));
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let hub = StatsHub::new(api, shutdown.clone(), tasks.clone());
        let subscription = hub.subscribe("container".into()).await;
        tokio::task::yield_now().await;
        drop(subscription);
        assert!(!tasks.is_empty());

        shutdown.cancel();
        tasks.close();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .expect("tracked stats tasks stop after global cancellation");
        assert!(tasks.is_empty());
    }
}
