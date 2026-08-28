pub mod atomic;
pub mod recovery;

use std::{fs, path::PathBuf};

use thiserror::Error;

use crate::security::permissions::{PermissionError, ensure_private_directory};

pub struct AppStore {
    apps_directory: PathBuf,
}

impl AppStore {
    pub fn initialize(apps_directory: PathBuf) -> Result<Self, StoreError> {
        ensure_private_directory(&apps_directory)?;
        Ok(Self { apps_directory })
    }

    pub fn scan(&self) -> Result<recovery::RecoveryReport, StoreError> {
        recovery::scan(&self.apps_directory)
    }

    pub fn apps_directory(&self) -> &std::path::Path {
        &self.apps_directory
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error("filesystem store I/O failed")]
    Io(#[from] std::io::Error),
    #[error("managed path violates the symlink boundary")]
    SymlinkBoundary,
    #[error("release already exists")]
    ReleaseConflict,
}

pub(crate) fn sync_directory(path: &std::path::Path) -> Result<(), StoreError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
