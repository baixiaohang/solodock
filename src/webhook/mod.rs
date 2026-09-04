pub mod ingress;
pub mod protocol;
pub mod rate;
pub mod store;

use std::sync::Arc;

use tokio::sync::{Notify, Semaphore};

use crate::{db::Database, registry::PollStateStore};

pub use rate::WebhookRateLimiter;
pub use store::{LoadedWebhook, WebhookMetadata, WebhookStatus, WebhookStore};

#[derive(Clone)]
pub struct WebhookServices {
    pub origin: String,
    pub store: WebhookStore,
    pub poll_states: PollStateStore,
    pub database: Database,
    pub notify: Arc<Notify>,
    pub limiter: rate::WebhookRateLimiter,
    pub permits: Arc<Semaphore>,
}
