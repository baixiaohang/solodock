use serde::Serialize;

use super::{
    HealthPolicy, ManagedFileMetadata, NetworkInput, PortInput, PublicEnvInput, VolumeInput,
};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DraftResponse {
    pub discovery_image_ref: String,
    pub poll_interval_seconds: u32,
    pub public_environment: Vec<PublicEnvInput>,
    pub secret_keys: Vec<String>,
    pub files: Vec<ManagedFileResponse>,
    pub ports: Vec<PortInput>,
    pub volumes: Vec<VolumeInput>,
    pub binds: Vec<super::BindMountInput>,
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
