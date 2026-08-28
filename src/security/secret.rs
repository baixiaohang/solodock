use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn random() -> Result<Self, SecretError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| SecretError::Random)?;
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        bytes.zeroize();
        Ok(Self(encoded))
    }

    pub fn sha256(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }

    pub fn constant_time_eq(&self, other: &str) -> bool {
        let expected = Sha256::digest(self.0.as_bytes());
        let supplied = Sha256::digest(other.as_bytes());
        expected.ct_eq(&supplied).into()
    }
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secure random generation failed")]
    Random,
}

pub trait TokenSource: Send + Sync {
    fn generate(&self) -> Result<SecretValue, SecretError>;
}

pub struct SystemTokenSource;

impl TokenSource for SystemTokenSource {
    fn generate(&self) -> Result<SecretValue, SecretError> {
        SecretValue::random()
    }
}
