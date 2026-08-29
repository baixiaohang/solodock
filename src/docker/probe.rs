use std::{sync::Arc, time::Duration};

use tokio::{sync::RwLock, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::models::{DockerReadApi, ProbeSnapshot};

#[derive(Clone)]
pub struct DockerSupervisor {
    snapshot: Arc<RwLock<ProbeSnapshot>>,
}

impl DockerSupervisor {
    pub fn new() -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(ProbeSnapshot::starting())),
        }
    }

    pub fn from_snapshot(snapshot: ProbeSnapshot) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(snapshot)),
        }
    }

    pub async fn current(&self) -> ProbeSnapshot {
        self.snapshot.read().await.clone()
    }

    pub fn start(
        &self,
        api: Arc<dyn DockerReadApi>,
        cancellation: CancellationToken,
    ) -> JoinHandle<()> {
        let snapshot = self.snapshot.clone();
        tokio::spawn(async move {
            let delays = [1, 2, 5, 10, 30];
            let mut attempt = 0usize;
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                let next = match api.probe().await {
                    Ok(value) => {
                        attempt = 0;
                        value
                    }
                    Err(error) => {
                        warn!(
                            error_code = error.public_code(),
                            "Docker capability probe failed"
                        );
                        ProbeSnapshot::failed(&error)
                    }
                };
                let ready = matches!(next.status, super::models::ProbeStatus::Ready);
                *snapshot.write().await = next;
                let seconds = if ready {
                    30
                } else {
                    let value = delays[attempt.min(delays.len() - 1)];
                    attempt = attempt.saturating_add(1);
                    value
                };
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(seconds)) => {}
                }
            }
        })
    }
}

impl Default for DockerSupervisor {
    fn default() -> Self {
        Self::new()
    }
}
