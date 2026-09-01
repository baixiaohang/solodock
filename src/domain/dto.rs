use serde::Serialize;
use uuid::Uuid;

use super::{
    HealthPolicy, ManagedFileMetadata, NetworkInput, PortInput, PublicEnvInput, VolumeInput,
};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DraftResponse {
    pub discovery_image_ref: String,
    pub credential_ref: Option<Uuid>,
    pub auto_deploy_enabled: bool,
    pub poll_interval_seconds: u32,
    pub stop_grace_period_seconds: u16,
    pub public_environment: Vec<PublicEnvInput>,
    pub secret_keys: Vec<String>,
    pub files: Vec<ManagedFileResponse>,
    pub ports: Vec<PortInput>,
    pub volumes: Vec<VolumeInput>,
    pub binds: Vec<super::BindMountInput>,
    pub owned_default_network: bool,
    pub service_discovery_enabled: bool,
    pub networks: Vec<NetworkInput>,
    pub health: HealthPolicy,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ManagedFileResponse {
    #[serde(flatten)]
    pub metadata: ManagedFileMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}
