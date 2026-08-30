use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

#[derive(Clone)]
pub struct WebhookRateLimiter {
    inner: Arc<Mutex<State>>,
}

struct State {
    window: Instant,
    global: u16,
    apps: HashMap<Uuid, u8>,
}

impl Default for WebhookRateLimiter {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                window: Instant::now(),
                global: 0,
                apps: HashMap::new(),
            })),
        }
    }
}

impl WebhookRateLimiter {
    pub async fn check(&self, app_id: Option<Uuid>, known_app: bool) -> bool {
        let mut state = self.inner.lock().await;
        if state.window.elapsed() >= Duration::from_secs(60) {
            state.window = Instant::now();
            state.global = 0;
            state.apps.clear();
        }
        if state.global >= 120 {
            return false;
        }
        state.global += 1;
        if known_app && let Some(app_id) = app_id {
            let count = state.apps.entry(app_id).or_default();
            if *count >= 10 {
                return false;
            }
            *count += 1;
        }
        true
    }

    pub async fn retain_apps(&self, ids: &[Uuid]) {
        self.inner
            .lock()
            .await
            .apps
            .retain(|id, _| ids.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn limits_known_apps_without_growing_for_unknown_ids() {
        let limiter = WebhookRateLimiter::default();
        let app = Uuid::new_v4();
        for _ in 0..10 {
            assert!(limiter.check(Some(app), true).await);
        }
        assert!(!limiter.check(Some(app), true).await);
        for _ in 0..20 {
            assert!(limiter.check(Some(Uuid::new_v4()), false).await);
        }
        assert_eq!(limiter.inner.lock().await.apps.len(), 1);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(limiter.check(Some(app), true).await);
    }

    #[tokio::test(start_paused = true)]
    async fn global_bucket_is_strictly_bounded_and_recovers_next_window() {
        let limiter = WebhookRateLimiter::default();
        for _ in 0..120 {
            assert!(limiter.check(Some(Uuid::new_v4()), false).await);
        }
        assert!(!limiter.check(None, false).await);
        assert!(limiter.inner.lock().await.apps.is_empty());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(limiter.check(None, false).await);
    }
}
