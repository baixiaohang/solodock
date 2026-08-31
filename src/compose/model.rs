use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ComposeDocument {
    pub services: BTreeMap<String, Service>,
    pub volumes: BTreeMap<String, VolumeDefinition>,
    pub networks: BTreeMap<String, NetworkDefinition>,
}

#[derive(Debug, Serialize)]
pub struct Service {
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub env_file: Vec<EnvFile>,
    pub volumes: Vec<ServiceMount>,
    pub ports: Vec<ServicePort>,
    pub networks: ServiceNetworks,
    pub restart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ServiceNetworks {
    Short(Vec<String>),
    Long(BTreeMap<String, ServiceNetworkAttachment>),
}

impl ServiceNetworks {
    pub fn len(&self) -> usize {
        match self {
            Self::Short(value) => value.len(),
            Self::Long(value) => value.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ServiceNetworkAttachment {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EnvFile {
    pub path: String,
    pub required: bool,
}

#[derive(Debug, Serialize)]
pub struct ServiceMount {
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Serialize)]
pub struct ServicePort {
    pub name: String,
    pub target: u16,
    pub published: String,
    pub host_ip: String,
    pub protocol: String,
}

#[derive(Debug, Serialize)]
pub struct Healthcheck {
    pub test: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disable: bool,
}

#[derive(Debug, Serialize)]
pub struct VolumeDefinition {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    pub name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct NetworkDefinition {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub driver_opts: BTreeMap<String, String>,
    pub name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}
