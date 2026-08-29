use serde::Deserialize;

use super::{error::RegistryError, reference::validate_digest};

pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub media_type: Option<String>,
    pub config: Descriptor,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub platform: Option<DescriptorPlatform>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DescriptorPlatform {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
}

pub fn parse_manifest(body: &[u8], media_type: &str) -> Result<Manifest, RegistryError> {
    if !matches!(media_type, OCI_MANIFEST | DOCKER_MANIFEST) {
        return Err(RegistryError::ManifestUnsupported);
    }
    let manifest: Manifest = serde_json::from_slice(body).map_err(|_| RegistryError::Protocol)?;
    if manifest.schema_version != 2
        || manifest
            .media_type
            .as_deref()
            .is_some_and(|value| value != media_type)
    {
        return Err(RegistryError::ManifestUnsupported);
    }
    validate_digest(&manifest.config.digest)?;
    if !matches!(
        manifest.config.media_type.as_str(),
        OCI_CONFIG | DOCKER_CONFIG
    ) {
        return Err(RegistryError::ManifestUnsupported);
    }
    Ok(manifest)
}

pub fn parse_index(body: &[u8], media_type: &str) -> Result<Index, RegistryError> {
    if !matches!(media_type, OCI_INDEX | DOCKER_LIST) {
        return Err(RegistryError::ManifestUnsupported);
    }
    let index: Index = serde_json::from_slice(body).map_err(|_| RegistryError::Protocol)?;
    if index.schema_version != 2
        || index.manifests.len() > 256
        || index
            .media_type
            .as_deref()
            .is_some_and(|value| value != media_type)
    {
        return Err(RegistryError::ManifestUnsupported);
    }
    for descriptor in &index.manifests {
        validate_digest(&descriptor.digest)?;
    }
    Ok(index)
}
