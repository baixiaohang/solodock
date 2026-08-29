use serde::{Deserialize, Serialize};

use super::SecretOperation;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedFileInput {
    pub logical_name: String,
    pub target_path: String,
    pub sensitive: bool,
    #[serde(default = "readonly")]
    pub readonly: bool,
    #[serde(flatten)]
    pub content: ManagedFileContent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ManagedFileContent {
    Public(PublicFileContent),
    Secret(SecretOperation),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicFileContent {
    pub content: String,
}

const fn readonly() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManagedFileMetadata {
    pub logical_name: String,
    pub target_path: String,
    pub sensitive: bool,
    pub readonly: bool,
}
