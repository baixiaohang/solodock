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
#[serde(deny_unknown_fields)]
pub struct AppMetadata {
    pub schema_version: u32,
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
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
    pub fn resource_names(&self) -> AppResourceNames {
        app_resource_names(&self.slug)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppResourceNames {
    pub project_name: String,
    pub owned_default_network_name: String,
    pub bridge_name: String,
}

pub fn app_resource_names(slug: &str) -> AppResourceNames {
    AppResourceNames {
        project_name: format!("solodock-{slug}"),
        owned_default_network_name: format!("solodock-{slug}-default"),
        bridge_name: format!("sd-{slug}"),
    }
}

pub fn owned_volume_name(slug: &str, logical_name: &str) -> String {
    format!("solodock-{slug}.{logical_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_share_the_immutable_slug_namespace() {
        let names = app_resource_names("media-1");
        assert_eq!(names.project_name, "solodock-media-1");
        assert_eq!(names.owned_default_network_name, "solodock-media-1-default");
        assert_eq!(names.bridge_name, "sd-media-1");
        assert!(names.bridge_name.len() <= 15);
        assert_eq!(
            owned_volume_name("media-1", "data"),
            "solodock-media-1.data"
        );
        assert_ne!(owned_volume_name("a-b", "c"), owned_volume_name("a", "b-c"));
    }
}
