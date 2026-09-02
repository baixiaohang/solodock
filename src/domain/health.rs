use serde::{Deserialize, Serialize};

pub const RUNNING_STABLE_WINDOW_MIN_SECONDS: u16 = 5;
pub const RUNNING_STABLE_WINDOW_MAX_SECONDS: u16 = 300;
pub const HTTP_HEALTH_INTERVAL_MIN_SECONDS: u16 = 1;
pub const HTTP_HEALTH_INTERVAL_MAX_SECONDS: u16 = 300;
pub const HTTP_HEALTH_TIMEOUT_MIN_SECONDS: u16 = 1;
pub const HTTP_HEALTH_TIMEOUT_MAX_SECONDS: u16 = 60;
pub const HTTP_HEALTH_RETRIES_MIN: u8 = 1;
pub const HTTP_HEALTH_RETRIES_MAX: u8 = 10;
pub const HTTP_HEALTH_START_PERIOD_MIN_SECONDS: u16 = 0;
pub const HTTP_HEALTH_START_PERIOD_MAX_SECONDS: u16 = 300;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct NumericLimit<T> {
    pub min: T,
    pub max: T,
    pub default: T,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct HealthConfigurationLimits {
    pub running_stable_window_seconds: NumericLimit<u16>,
    pub http_interval_seconds: NumericLimit<u16>,
    pub http_timeout_seconds: NumericLimit<u16>,
    pub http_retries: NumericLimit<u8>,
    pub http_start_period_seconds: NumericLimit<u16>,
    pub stop_grace_period_seconds: NumericLimit<u16>,
}

pub const fn health_configuration_limits() -> HealthConfigurationLimits {
    HealthConfigurationLimits {
        running_stable_window_seconds: NumericLimit {
            min: RUNNING_STABLE_WINDOW_MIN_SECONDS,
            max: RUNNING_STABLE_WINDOW_MAX_SECONDS,
            default: 15,
        },
        http_interval_seconds: NumericLimit {
            min: HTTP_HEALTH_INTERVAL_MIN_SECONDS,
            max: HTTP_HEALTH_INTERVAL_MAX_SECONDS,
            default: 10,
        },
        http_timeout_seconds: NumericLimit {
            min: HTTP_HEALTH_TIMEOUT_MIN_SECONDS,
            max: HTTP_HEALTH_TIMEOUT_MAX_SECONDS,
            default: 5,
        },
        http_retries: NumericLimit {
            min: HTTP_HEALTH_RETRIES_MIN,
            max: HTTP_HEALTH_RETRIES_MAX,
            default: 6,
        },
        http_start_period_seconds: NumericLimit {
            min: HTTP_HEALTH_START_PERIOD_MIN_SECONDS,
            max: HTTP_HEALTH_START_PERIOD_MAX_SECONDS,
            default: 30,
        },
        stop_grace_period_seconds: NumericLimit {
            min: 1,
            max: super::MAX_STOP_GRACE_PERIOD_SECONDS,
            default: super::default_stop_grace_period_seconds(),
        },
    }
}

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
