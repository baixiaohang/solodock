use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{ComposeAction, ComposeError, ComposeRunner, RunContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeStatus {
    Starting,
    Ready,
    Unavailable,
    PermissionDenied,
    Incompatible,
}

#[derive(Clone)]
pub struct ComposeCapability {
    sender: Arc<watch::Sender<ComposeStatus>>,
}

impl Default for ComposeCapability {
    fn default() -> Self {
        let (sender, _) = watch::channel(ComposeStatus::Starting);
        Self {
            sender: Arc::new(sender),
        }
    }
}

impl ComposeCapability {
    pub fn current(&self) -> ComposeStatus {
        *self.sender.borrow()
    }
    pub async fn probe(&self, runner: &dyn ComposeRunner) {
        let status = match runner
            .run(ComposeAction::Version, RunContext::capability())
            .await
        {
            Ok(result) => parse_version(&result.stdout)
                .map_or(ComposeStatus::Incompatible, |_| ComposeStatus::Ready),
            Err(ComposeError::Incompatible) => ComposeStatus::Incompatible,
            Err(ComposeError::PermissionDenied) => ComposeStatus::PermissionDenied,
            Err(_) => ComposeStatus::Unavailable,
        };
        self.sender.send_replace(status);
    }

    pub fn start(
        &self,
        runner: Arc<dyn ComposeRunner>,
        cancellation: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let capability = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                }
                capability.probe(runner.as_ref()).await;
            }
        })
    }
}

fn parse_version(output: &[u8]) -> Option<(u64, u64, u64)> {
    let value = std::str::from_utf8(output)
        .ok()?
        .trim()
        .trim_start_matches('v');
    let mut parts = value.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?,
    );
    (version >= (2, 24, 0)).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn requires_supported_v2() {
        assert!(parse_version(b"v2.24.0\n").is_some());
        assert!(parse_version(b"2.23.9").is_none());
        assert!(parse_version(b"1.99.0").is_none());
    }

    struct RecoveringRunner(AtomicUsize);

    #[async_trait]
    impl ComposeRunner for RecoveringRunner {
        async fn run(
            &self,
            _: ComposeAction,
            _: RunContext,
        ) -> Result<crate::compose::ComposeOutput, ComposeError> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ComposeError::Unavailable)
            } else {
                Ok(crate::compose::ComposeOutput {
                    stdout: b"2.24.0\n".to_vec(),
                    stderr: vec![],
                })
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_capability_is_reprobed_after_recovery() {
        let capability = ComposeCapability::default();
        let runner: Arc<dyn ComposeRunner> = Arc::new(RecoveringRunner(AtomicUsize::new(0)));
        capability.probe(runner.as_ref()).await;
        assert_eq!(capability.current(), ComposeStatus::Unavailable);
        let cancellation = CancellationToken::new();
        let task = capability.start(runner, cancellation.clone());
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(capability.current(), ComposeStatus::Ready);
        cancellation.cancel();
        task.await.unwrap();
    }
}
