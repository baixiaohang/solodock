use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    BindMountInput, EnvironmentInput, HealthPolicy, ManagedFileInput, NetworkInput, PortInput,
    VolumeInput,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Stopped,
    Running,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DraftInput {
    pub slug: String,
    pub display_name: String,
    pub discovery_image_ref: String,
    #[serde(default)]
    pub credential_ref: Option<Uuid>,
    #[serde(default)]
    pub auto_deploy_enabled: bool,
    /// One-request acknowledgement. It is fingerprinted but never persisted.
    #[serde(default)]
    pub auto_deploy_acknowledged: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u32,
    #[serde(default)]
    pub environment: EnvironmentInput,
    #[serde(default)]
    pub files: Vec<ManagedFileInput>,
    #[serde(default)]
    pub ports: Vec<PortInput>,
    #[serde(default)]
    pub volumes: Vec<VolumeInput>,
    #[serde(default)]
    pub binds: Vec<BindMountInput>,
    #[serde(default = "default_owned_default_network")]
    pub owned_default_network: bool,
    #[serde(default)]
    pub networks: Vec<NetworkInput>,
    #[serde(default)]
    pub health: HealthPolicy,
}

pub const fn default_owned_default_network() -> bool {
    true
}

pub const fn default_poll_interval() -> u32 {
    300
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppMetadata {
    pub schema_version: u32,
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub project_name: String,
    pub discovery_image_ref: String,
    pub credential_ref: Option<Uuid>,
    pub draft_revision: Uuid,
    pub draft_config_sha256: String,
    pub desired_state: DesiredState,
    pub auto_deploy_enabled: bool,
    pub poll_interval_seconds: u32,
    pub last_operation_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

impl AppMetadata {
    pub fn project_name(id: Uuid) -> String {
        format!("solodock-{}", id.simple())
    }
}
