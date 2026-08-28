use std::sync::Arc;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

const MEMORY_KIB: u32 = 64 * 1024;
const ITERATIONS: u32 = 3;
const PARALLELISM: u32 = 1;

#[derive(Clone)]
pub struct PasswordService {
    semaphore: Arc<Semaphore>,
}

impl Default for PasswordService {
    fn default() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(2)),
        }
    }
}

impl PasswordService {
    pub fn validate(password: &str) -> Result<&str, PasswordError> {
        let trimmed = password.trim();
        let scalar_count = trimmed.chars().count();
        if !(14..=128).contains(&scalar_count) || trimmed.len() > 512 {
            return Err(PasswordError::Policy);
        }
        Ok(trimmed)
    }

    pub async fn hash(&self, password: String) -> Result<String, PasswordError> {
        Self::validate(&password)?;
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| PasswordError::Worker)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let password = Zeroizing::new(password);
            let password = Zeroizing::new(password.trim().to_owned());
            let mut salt_bytes = [0u8; 16];
            getrandom::fill(&mut salt_bytes).map_err(|_| PasswordError::Random)?;
            let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| PasswordError::Hash)?;
            let argon2 = configured_argon2()?;
            argon2
                .hash_password(password.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|_| PasswordError::Hash)
        })
        .await
        .map_err(|_| PasswordError::Worker)?
    }

    pub async fn verify(
        &self,
        password: String,
        encoded_hash: String,
    ) -> Result<bool, PasswordError> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| PasswordError::Worker)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let password = Zeroizing::new(password);
            let password = Zeroizing::new(password.trim().to_owned());
            let parsed = PasswordHash::new(&encoded_hash).map_err(|_| PasswordError::Hash)?;
            Ok(configured_argon2()?
                .verify_password(password.as_bytes(), &parsed)
                .is_ok())
        })
        .await
        .map_err(|_| PasswordError::Worker)?
    }
}

fn configured_argon2() -> Result<Argon2<'static>, PasswordError> {
    let params =
        Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None).map_err(|_| PasswordError::Hash)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

#[derive(Debug, Error)]
pub enum PasswordError {
    #[error("password must contain 14 to 128 Unicode characters and at most 512 UTF-8 bytes")]
    Policy,
    #[error("password hashing failed")]
    Hash,
    #[error("password worker failed")]
    Worker,
    #[error("secure random generation failed")]
    Random,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_password_boundaries_after_trimming() {
        assert!(PasswordService::validate(" 12345678901234 ").is_ok());
        assert!(PasswordService::validate("1234567890123").is_err());
        assert!(PasswordService::validate(&"界".repeat(129)).is_err());
    }

    #[tokio::test]
    async fn hashes_and_verifies_password() {
        let service = PasswordService::default();
        let hash = service.hash("correct horse battery".into()).await.unwrap();
        assert!(
            service
                .verify("correct horse battery".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(
            !service
                .verify("wrong horse battery".into(), hash)
                .await
                .unwrap()
        );
    }
}
