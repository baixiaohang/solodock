use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
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
    #[serde(default = "default_stop_grace_period_seconds")]
    pub stop_grace_period_seconds: u16,
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
    #[serde(default = "default_service_discovery_enabled")]
    pub service_discovery_enabled: bool,
    #[serde(default)]
    pub networks: Vec<NetworkInput>,
    #[serde(default)]
    pub health: HealthPolicy,
}

pub const fn default_owned_default_network() -> bool {
    true
}

pub const fn default_service_discovery_enabled() -> bool {
    true
}

pub const APP_METADATA_SCHEMA_VERSION: u32 = 3;
pub const RESOURCE_NAME_SCHEMA_LEGACY: u32 = 1;
pub const RESOURCE_NAME_SCHEMA_CURRENT: u32 = 2;

const fn legacy_resource_name_schema_version() -> u32 {
    RESOURCE_NAME_SCHEMA_LEGACY
}

pub const fn default_poll_interval() -> u32 {
    300
}

pub const fn default_stop_grace_period_seconds() -> u16 {
    10
}

pub const MAX_STOP_GRACE_PERIOD_SECONDS: u16 = 600;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppMetadata {
    pub schema_version: u32,
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    #[serde(default = "legacy_resource_name_schema_version")]
    pub resource_name_schema_version: u32,
    pub discovery_image_ref: Option<String>,
    pub credential_ref: Option<Uuid>,
    pub draft_revision: Option<Uuid>,
    pub draft_config_sha256: Option<String>,
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
        self.resource_identity().resource_names()
    }

    pub fn resource_identity(&self) -> AppResourceIdentity<'_> {
        AppResourceIdentity {
            app_id: self.id,
            slug: &self.slug,
            schema_version: self.resource_name_schema_version,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.draft_revision.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppResourceNames {
    pub project_name: String,
    pub owned_default_network_name: String,
    pub bridge_name: String,
}

#[derive(Clone, Copy, Debug)]
pub struct AppResourceIdentity<'a> {
    pub app_id: Uuid,
    pub slug: &'a str,
    pub schema_version: u32,
}

impl AppResourceIdentity<'_> {
    pub fn resource_names(self) -> AppResourceNames {
        let bridge_name = match self.schema_version {
            RESOURCE_NAME_SCHEMA_LEGACY => format!("sd-{}", self.slug),
            RESOURCE_NAME_SCHEMA_CURRENT => format!("sd-{}", stable_resource_token(self.app_id)),
            _ => unreachable!("validated resource naming schema"),
        };
        AppResourceNames {
            project_name: format!("solodock-{}", self.slug),
            owned_default_network_name: format!("solodock-{}-default", self.slug),
            bridge_name,
        }
    }

    pub fn owned_volume_name(self, logical_name: &str) -> String {
        format!("solodock-{}.{logical_name}", self.slug)
    }
}

pub fn stable_resource_token(app_id: Uuid) -> String {
    let digest = Sha256::digest(app_id.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_are_versioned_and_stable() {
        let app_id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let legacy = AppResourceIdentity {
            app_id,
            slug: "media-1",
            schema_version: RESOURCE_NAME_SCHEMA_LEGACY,
        };
        let names = legacy.resource_names();
        assert_eq!(names.project_name, "solodock-media-1");
        assert_eq!(names.owned_default_network_name, "solodock-media-1-default");
        assert_eq!(names.bridge_name, "sd-media-1");
        assert!(names.bridge_name.len() <= 15);
        assert_eq!(legacy.owned_volume_name("data"), "solodock-media-1.data");
        let current = AppResourceIdentity {
            schema_version: RESOURCE_NAME_SCHEMA_CURRENT,
            ..legacy
        };
        assert_eq!(current.resource_names().bridge_name.len(), 15);
        assert_eq!(current.resource_names(), current.resource_names());
        assert_ne!(current.resource_names().bridge_name, names.bridge_name);
    }
}
