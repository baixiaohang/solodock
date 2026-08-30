use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortInput {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VolumeInput {
    Owned {
        logical_name: String,
        target_path: String,
    },
    External {
        name: String,
        target_path: String,
    },
}

impl VolumeInput {
    pub fn target_path(&self) -> &str {
        match self {
            Self::Owned { target_path, .. } | Self::External { target_path, .. } => target_path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindMountInput {
    pub source: String,
    pub target_path: String,
    #[serde(default = "default_readonly")]
    pub readonly: bool,
    #[serde(default)]
    pub acknowledge_non_rollbackable: bool,
}

const fn default_readonly() -> bool {
    true
}

pub use super::networks::NetworkInput;
