use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::models::{ContainerRecord, DockerError, DockerErrorKind, ImageRecord};

/// Only a complete canonical engine image identity may reach ImageDelete.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ExactImageId(String);

impl ExactImageId {
    pub fn parse(value: &str) -> Result<Self, DockerError> {
        crate::registry::reference::validate_digest(value)
            .map_err(|_| DockerError::new(DockerErrorKind::ObservationFailed))?;
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ExactImageId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|_| serde::de::Error::custom("invalid exact image ID"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupImage {
    pub image: ImageRecord,
    pub reported_size_bytes: u64,
    pub repo_tags: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveImageResult {
    Accepted,
    Retained,
}

/// Separate from DockerReadApi: observers cannot acquire mutation capability.
#[async_trait]
pub trait ImageCleanup: Send + Sync {
    async fn all_containers(&self) -> Result<Vec<ContainerRecord>, DockerError>;
    async fn inspect(&self, id: &ExactImageId) -> Result<Option<CleanupImage>, DockerError>;
    async fn remove(&self, id: &ExactImageId) -> Result<RemoveImageResult, DockerError>;
}

pub struct UnavailableImageCleanup;
#[async_trait]
impl ImageCleanup for UnavailableImageCleanup {
    async fn all_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn inspect(&self, _: &ExactImageId) -> Result<Option<CleanupImage>, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
    async fn remove(&self, _: &ExactImageId) -> Result<RemoveImageResult, DockerError> {
        Err(DockerError::new(DockerErrorKind::Unavailable))
    }
}
