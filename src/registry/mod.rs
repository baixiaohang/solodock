pub mod auth;
pub mod client;
pub mod credentials;
pub mod error;
pub mod manifest;
pub mod poll_state;
pub mod poller;
pub mod reference;

pub use client::{Platform, PollResolve, RegistryResolver, ResolvedImage};
pub use credentials::{CredentialMetadata, CredentialStore, LoadedCredential};
pub use error::RegistryError;
pub use poll_state::{
    PollObservation, PollOutcome, PollState, PollStateError, PollStateStore, WebhookAccept,
};
pub use poller::{PollCoordinator, PollHealth, PollHealthSnapshot};
pub use reference::ImageReference;
