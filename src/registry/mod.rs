pub mod auth;
pub mod client;
pub mod credentials;
pub mod error;
pub mod manifest;
pub mod reference;

pub use client::{Platform, RegistryResolver, ResolvedImage};
pub use credentials::{CredentialMetadata, CredentialStore, LoadedCredential};
pub use error::RegistryError;
pub use reference::ImageReference;
