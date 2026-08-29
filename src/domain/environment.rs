use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicEnvInput {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SecretEnvInput {
    pub key: String,
    #[serde(flatten)]
    pub operation: SecretOperation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretOperation {
    Keep,
    Replace { value: String },
    Delete,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentInput {
    pub public: Vec<PublicEnvInput>,
    pub secrets: Vec<SecretEnvInput>,
}

#[derive(Default, Zeroize, ZeroizeOnDrop)]
pub struct SecretMaterial(String);

impl SecretMaterial {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).into_bytes()
    }
}

pub type SecretMap = BTreeMap<String, SecretMaterial>;
