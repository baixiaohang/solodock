use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore, SemaphorePermit};
use uuid::Uuid;

use crate::security::permissions::{check_private, ensure_private_directory};

#[derive(Clone)]
pub struct AppMutationCoordinator {
    locks_directory: PathBuf,
    apps: Arc<StdMutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
    catalog: Arc<Mutex<()>>,
    compose: Arc<Semaphore>,
}

pub struct AppMutationGuard {
    _process: OwnedMutexGuard<()>,
    file: File,
}

impl Drop for AppMutationGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl AppMutationCoordinator {
    pub fn new(runtime_directory: PathBuf) -> Result<Self, CoordinatorError> {
        let locks_directory = runtime_directory.join("locks");
        ensure_private_directory(&locks_directory)?;
        Ok(Self {
            locks_directory,
            apps: Arc::default(),
            catalog: Arc::default(),
            compose: Arc::new(Semaphore::new(1)),
        })
    }

    pub async fn catalog_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.catalog.lock().await
    }

    pub fn try_app(&self, app_id: Uuid) -> Result<AppMutationGuard, CoordinatorError> {
        let lock = self
            .apps
            .lock()
            .expect("mutation lock registry is not poisoned")
            .entry(app_id)
            .or_default()
            .clone();
        let process = lock.try_lock_owned().map_err(|_| CoordinatorError::Busy)?;
        let path = self.locks_directory.join(format!("{app_id}.lock"));
        let file = match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                check_private(&path, false)?;
                OpenOptions::new().write(true).open(&path)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?,
            Err(error) => return Err(error.into()),
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(CoordinatorError::Busy);
        }
        Ok(AppMutationGuard {
            _process: process,
            file,
        })
    }

    pub fn try_compose(&self) -> Result<SemaphorePermit<'_>, CoordinatorError> {
        self.compose
            .try_acquire()
            .map_err(|_| CoordinatorError::Busy)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("application mutation is busy")]
    Busy,
    #[error(transparent)]
    Permission(#[from] crate::security::permissions::PermissionError),
    #[error("mutation lock I/O failed")]
    Io(#[from] std::io::Error),
}
