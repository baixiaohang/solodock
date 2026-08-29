use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum HealthPolicy {
    Healthy {
        #[serde(default)]
        http: Option<HttpHealthcheck>,
    },
    Running {
        #[serde(default = "default_stable_window")]
        stable_window_seconds: u16,
    },
    Completed,
    Disabled {
        #[serde(default)]
        acknowledge_reduced_safety: bool,
    },
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self::Running {
            stable_window_seconds: default_stable_window(),
        }
    }
}

const fn default_stable_window() -> u16 {
    15
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpHealthcheck {
    pub client: HttpClient,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub interval_seconds: u16,
    pub timeout_seconds: u16,
    pub retries: u8,
    pub start_period_seconds: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HttpClient {
    Curl,
    Wget,
}
