use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::{
    AppStore, StoreError,
    atomic::{AtomicWriter, rename_no_replace},
    sync_directory,
};
use crate::security::permissions::check_private;

pub const CLEANUP_TRASH_DIRECTORY: &str = ".cleanup-trash";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupFault {
    MarkerPublished,
    Rename,
    SourceSync,
    DestinationSync,
    PayloadRemoved,
    MarkerRetired,
    DirectoryRemoved,
    RetiredMarkerRemoved,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CleanupArtifact {
    Release {
        app_id: Uuid,
        release_id: Uuid,
        config_revision_id: Uuid,
    },
    ConfigRevision {
        app_id: Uuid,
        revision_id: Uuid,
    },
    Temporary {
        app_id: Option<Uuid>,
        location: CleanupTempLocation,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CleanupTempLocation {
    AppsRoot,
    AppRoot,
    Releases,
    ConfigRevisions,
}

impl CleanupArtifact {
    pub fn app_id(&self) -> Option<Uuid> {
        match self {
            Self::Release { app_id, .. } | Self::ConfigRevision { app_id, .. } => Some(*app_id),
            Self::Temporary { app_id, .. } => *app_id,
        }
    }

    pub fn public_id(&self) -> String {
        match self {
            Self::Release { release_id, .. } => release_id.to_string(),
            Self::ConfigRevision { revision_id, .. } => revision_id.to_string(),
            Self::Temporary { name, .. } => name.clone(),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Release { .. } => "release",
            Self::ConfigRevision { .. } => "config_revision",
            Self::Temporary { .. } => "temporary",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupMarker {
    schema_version: u32,
    pub operation_id: Uuid,
    pub plan_hash: String,
    pub items: Vec<CleanupArtifact>,
    integrity_hmac: String,
}

impl AppStore {
    #[cfg(any(test, feature = "docker-e2e"))]
    pub fn fail_cleanup_once(&self, point: CleanupFault) {
        *self.cleanup_fault.lock().expect("cleanup fault lock") = Some(point);
    }

    fn cleanup_checkpoint(&self, point: CleanupFault) -> Result<(), StoreError> {
        #[cfg(any(test, feature = "docker-e2e"))]
        {
            let mut fault = self.cleanup_fault.lock().expect("cleanup fault lock");
            if *fault == Some(point) {
                *fault = None;
                return Err(std::io::Error::other("injected cleanup filesystem failure").into());
            }
        }
        let _ = point;
        Ok(())
    }

    fn cleanup_retired_marker_path(&self, operation_id: Uuid) -> PathBuf {
        self.apps_directory()
            .join(CLEANUP_TRASH_DIRECTORY)
            .join(format!("{operation_id}.retired.toml"))
    }
    pub fn cleanup_tombstone_path(&self, operation_id: Uuid) -> PathBuf {
        self.apps_directory()
            .join(CLEANUP_TRASH_DIRECTORY)
            .join(operation_id.to_string())
    }

    pub fn prepare_cleanup_tombstone(
        &self,
        operation_id: Uuid,
        plan_hash: &[u8],
        items: &[CleanupArtifact],
    ) -> Result<(), StoreError> {
        if items.len() > crate::storage_cleanup::MAX_CLEANUP_ITEMS {
            return Err(StoreError::ContentInvalid);
        }
        let root = self.apps_directory().join(CLEANUP_TRASH_DIRECTORY);
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                check_private(&root, true)?;
            }
            Ok(_) => return Err(StoreError::SymlinkBoundary),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&root)?;
                sync_directory(self.apps_directory())?;
            }
            Err(error) => return Err(error.into()),
        }
        let operation = self.cleanup_tombstone_path(operation_id);
        let temporary = root.join(format!(".solodock-tmp-{}", operation_id.simple()));
        match fs::symlink_metadata(&operation) {
            Ok(_) => {
                remove_incomplete_preparation(&temporary, &root)?;
                let marker = self.read_cleanup_marker(operation_id)?;
                if marker.plan_hash != encode_hex(plan_hash) || marker.items != items {
                    return Err(StoreError::ContentInvalid);
                }
                sync_directory(&operation.join("payload"))?;
                sync_directory(&operation)?;
                sync_directory(&root)?;
                sync_directory(self.apps_directory())?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        remove_incomplete_preparation(&temporary, &root)?;
        let result = (|| {
            fs::DirBuilder::new().mode(0o700).create(&temporary)?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(temporary.join("payload"))?;
            let mut marker = CleanupMarker {
                schema_version: 1,
                operation_id,
                plan_hash: encode_hex(plan_hash),
                items: items.to_vec(),
                integrity_hmac: String::new(),
            };
            marker.integrity_hmac = sign_marker(&marker, self.integrity_key()?);
            let encoded = toml::to_string(&marker).map_err(|_| StoreError::ContentInvalid)?;
            AtomicWriter::write(&temporary.join("marker.toml"), encoded.as_bytes(), 0o600)?;
            sync_directory(&temporary)?;
            rename_no_replace(&temporary, &operation)?;
            self.cleanup_checkpoint(CleanupFault::MarkerPublished)?;
            sync_directory(&root)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
            let _ = sync_directory(&root);
        }
        result
    }

    pub fn read_cleanup_marker(&self, operation_id: Uuid) -> Result<CleanupMarker, StoreError> {
        let root = self.apps_directory().join(CLEANUP_TRASH_DIRECTORY);
        check_private(&root, true)?;
        let operation = self.cleanup_tombstone_path(operation_id);
        let retired = self.cleanup_retired_marker_path(operation_id);
        let is_retired = fs::symlink_metadata(&retired).is_ok();
        let marker_path = if is_retired {
            retired
        } else {
            operation.join("marker.toml")
        };
        check_private(&marker_path, false)?;
        let marker: CleanupMarker = toml::from_str(&fs::read_to_string(marker_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        if marker.schema_version != 1
            || marker.operation_id != operation_id
            || marker.items.len() > crate::storage_cleanup::MAX_CLEANUP_ITEMS
            || !bool::from(
                marker
                    .integrity_hmac
                    .as_bytes()
                    .ct_eq(sign_marker(&marker, self.integrity_key()?).as_bytes()),
            )
        {
            return Err(StoreError::ContentInvalid);
        }
        if is_retired {
            match fs::symlink_metadata(&operation) {
                Ok(_) => {
                    check_private(&operation, true)?;
                    if fs::read_dir(&operation)?.next().is_some() {
                        return Err(StoreError::ContentInvalid);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(marker);
        }
        check_private(&operation, true)?;
        for entry in fs::read_dir(&operation)? {
            let entry = entry?;
            if !matches!(entry.file_name().to_str(), Some("marker.toml" | "payload")) {
                return Err(StoreError::ContentInvalid);
            }
        }
        let payload = operation.join("payload");
        match fs::symlink_metadata(&payload) {
            Ok(_) => check_private(&payload, true)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(marker),
            Err(error) => return Err(error.into()),
        }
        for entry in fs::read_dir(&payload)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::ContentInvalid)?;
            if name
                .parse::<usize>()
                .ok()
                .filter(|ordinal| ordinal.to_string() == name && *ordinal < marker.items.len())
                .is_none()
            {
                return Err(StoreError::ContentInvalid);
            }
            let ordinal = name
                .parse::<usize>()
                .map_err(|_| StoreError::ContentInvalid)?;
            logical_artifact_bytes(&entry.path(), &marker.items[ordinal])?;
        }
        for (ordinal, artifact) in marker.items.iter().enumerate() {
            let path = payload.join(ordinal.to_string());
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    logical_artifact_bytes(&payload.join(ordinal.to_string()), artifact)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(marker)
    }

    pub fn cleanup_tombstones(&self) -> Result<Vec<Uuid>, StoreError> {
        let root = self.apps_directory().join(CLEANUP_TRASH_DIRECTORY);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        check_private(&root, true)?;
        let mut operations = Vec::new();
        for entry in entries {
            let entry = entry?;
            if exact_internal_temp_name(&entry.file_name()) {
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(StoreError::SymlinkBoundary);
                }
                validate_cleanup_source(&entry.path())?;
                continue;
            }
            if let Some(operation) = retired_marker_operation(&entry.file_name()) {
                self.read_cleanup_marker(operation)?;
                operations.push(operation);
                continue;
            }
            let metadata = entry.metadata()?;
            if entry.file_type()?.is_symlink() || !metadata.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            let name = entry.file_name();
            let operation = name
                .to_str()
                .and_then(|value| value.parse::<Uuid>().ok())
                .filter(|value| name.as_os_str() == std::ffi::OsStr::new(&value.to_string()))
                .ok_or(StoreError::ContentInvalid)?;
            self.read_cleanup_marker(operation)?;
            operations.push(operation);
        }
        operations.sort_unstable();
        operations.dedup();
        Ok(operations)
    }

    pub fn cleanup_preparations(&self) -> Result<Vec<Uuid>, StoreError> {
        let root = self.apps_directory().join(CLEANUP_TRASH_DIRECTORY);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        check_private(&root, true)?;
        let mut operations = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !exact_internal_temp_name(&entry.file_name()) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            validate_cleanup_source(&entry.path())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::ContentInvalid)?;
            let raw = name
                .strip_prefix(".solodock-tmp-")
                .ok_or(StoreError::ContentInvalid)?;
            let operation = raw
                .parse::<Uuid>()
                .ok()
                .filter(|operation| operation.simple().to_string() == raw)
                .ok_or(StoreError::ContentInvalid)?;
            operations.push(operation);
        }
        operations.sort_unstable();
        Ok(operations)
    }

    pub fn detach_cleanup_artifact(
        &self,
        operation_id: Uuid,
        ordinal: usize,
        artifact: &CleanupArtifact,
    ) -> Result<DetachResult, StoreError> {
        let marker = self.read_cleanup_marker(operation_id)?;
        if marker.items.get(ordinal) != Some(artifact) {
            return Err(StoreError::ContentInvalid);
        }
        let source = self.cleanup_source_path(artifact)?;
        let destination = self
            .cleanup_tombstone_path(operation_id)
            .join("payload")
            .join(ordinal.to_string());
        let source_before = file_identity_if_exists(&source)?;
        let destination_before = file_identity_if_exists(&destination)?;
        let source_identity = match (source_before, destination_before) {
            (None, Some(_)) => {
                logical_artifact_bytes(&destination, artifact)?;
                self.sync_cleanup_rename(&source, &destination)?;
                return Ok(DetachResult::AlreadyDetached);
            }
            (None, None) => return Ok(DetachResult::ConfirmedMissing),
            (Some(_), Some(_)) => return Err(StoreError::ContentInvalid),
            (Some(identity), None) => identity,
        };
        logical_artifact_bytes(&source, artifact)?;
        if file_identity(&source)? != source_identity {
            return Err(StoreError::ContentInvalid);
        }
        if let Err(error) = self.rename_cleanup_no_replace(&source, &destination) {
            let after_source = file_identity_if_exists(&source)?;
            let after_destination = file_identity_if_exists(&destination)?;
            if after_source == Some(source_identity) && after_destination.is_none() {
                return Ok(DetachResult::ConfirmedRetained);
            }
            if after_source.is_none() && after_destination == Some(source_identity) {
                self.sync_cleanup_rename(&source, &destination)?;
                return Ok(DetachResult::Detached);
            }
            return Err(error);
        }
        if file_identity_if_exists(&source)?.is_some()
            || file_identity_if_exists(&destination)? != Some(source_identity)
        {
            return Err(StoreError::ContentInvalid);
        }
        self.sync_cleanup_rename(&source, &destination)?;
        Ok(DetachResult::Detached)
    }

    pub fn cleanup_artifact_is_detached(
        &self,
        operation_id: Uuid,
        ordinal: usize,
        artifact: &CleanupArtifact,
    ) -> Result<bool, StoreError> {
        let marker = self.read_cleanup_marker(operation_id)?;
        if marker.items.get(ordinal) != Some(artifact) {
            return Err(StoreError::ContentInvalid);
        }
        let source = self.cleanup_source_path(artifact)?;
        let destination = self
            .cleanup_tombstone_path(operation_id)
            .join("payload")
            .join(ordinal.to_string());
        match (
            file_identity_if_exists(&source)?,
            file_identity_if_exists(&destination)?,
        ) {
            (None, Some(_)) => {
                logical_artifact_bytes(&destination, artifact)?;
                Ok(true)
            }
            (Some(_), None) => Ok(false),
            _ => Err(StoreError::ContentInvalid),
        }
    }

    fn sync_cleanup_rename(&self, source: &Path, destination: &Path) -> Result<(), StoreError> {
        self.cleanup_checkpoint(CleanupFault::SourceSync)?;
        sync_directory(source.parent().ok_or(StoreError::ContentInvalid)?)?;
        self.cleanup_checkpoint(CleanupFault::DestinationSync)?;
        sync_directory(destination.parent().ok_or(StoreError::ContentInvalid)?)
    }

    pub fn finalize_cleanup_tombstone(&self, operation_id: Uuid) -> Result<(), StoreError> {
        self.read_cleanup_marker(operation_id)?;
        let path = self.cleanup_tombstone_path(operation_id);
        #[cfg(test)]
        if self
            .cleanup_fail_next_finalize
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Io(std::io::Error::other(
                "injected cleanup finalizer failure",
            )));
        }
        let root = path.parent().ok_or(StoreError::ContentInvalid)?;
        let retired = self.cleanup_retired_marker_path(operation_id);
        if fs::symlink_metadata(&retired).is_err() {
            let payload = path.join("payload");
            match fs::remove_dir_all(&payload) {
                Ok(()) => self.cleanup_checkpoint(CleanupFault::PayloadRemoved)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            sync_directory(&path)?;
            // Keep the signed marker outside the empty directory until its
            // removal is durable. Every interrupted stage remains identifiable.
            rename_no_replace(&path.join("marker.toml"), &retired)?;
            self.cleanup_checkpoint(CleanupFault::MarkerRetired)?;
        }
        // Repeat both rename barriers even when resuming a visible retirement.
        sync_directory(root)?;
        if path.exists() {
            sync_directory(&path)?;
        }
        match fs::remove_dir(&path) {
            Ok(()) => self.cleanup_checkpoint(CleanupFault::DirectoryRemoved)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        sync_directory(root)?;
        fs::remove_file(&retired)?;
        self.cleanup_checkpoint(CleanupFault::RetiredMarkerRemoved)?;
        sync_directory(root)
    }

    /// Called only with an exact terminal proof and durable retirement intent.
    /// An absent marker is not evidence that the preceding unlink was durable.
    pub fn sync_cleanup_retirement(&self, operation_id: Uuid) -> Result<(), StoreError> {
        let root = self.apps_directory.join(CLEANUP_TRASH_DIRECTORY);
        check_private(&root, true)?;
        for path in [
            self.cleanup_tombstone_path(operation_id),
            self.cleanup_retired_marker_path(operation_id),
        ] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(StoreError::ContentInvalid),
            }
        }
        sync_directory(&root)
    }

    fn rename_cleanup_no_replace(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<(), StoreError> {
        self.cleanup_checkpoint(CleanupFault::Rename)?;
        #[cfg(test)]
        if self
            .cleanup_fail_next_rename
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Io(std::io::Error::other(
                "injected cleanup rename failure",
            )));
        }
        rename_no_replace(source, destination)
    }

    #[cfg(test)]
    fn fail_next_cleanup_rename(&self) {
        self.cleanup_fail_next_rename
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_cleanup_finalize(&self) {
        self.cleanup_fail_next_finalize
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn cleanup_source_path(&self, artifact: &CleanupArtifact) -> Result<PathBuf, StoreError> {
        let path = match artifact {
            CleanupArtifact::Release {
                app_id, release_id, ..
            } => self
                .app_directory(*app_id)
                .join("releases")
                .join(release_id.to_string()),
            CleanupArtifact::ConfigRevision {
                app_id,
                revision_id,
            } => self
                .app_directory(*app_id)
                .join("config-revisions")
                .join(revision_id.to_string()),
            CleanupArtifact::Temporary {
                app_id,
                location,
                name,
            } => {
                if !valid_temp_name(*location, name) {
                    return Err(StoreError::ContentInvalid);
                }
                match (app_id, location) {
                    (None, CleanupTempLocation::AppsRoot) => self.apps_directory().join(name),
                    (Some(app_id), CleanupTempLocation::AppRoot) => {
                        self.app_directory(*app_id).join(name)
                    }
                    (Some(app_id), CleanupTempLocation::Releases) => {
                        self.app_directory(*app_id).join("releases").join(name)
                    }
                    (Some(app_id), CleanupTempLocation::ConfigRevisions) => self
                        .app_directory(*app_id)
                        .join("config-revisions")
                        .join(name),
                    _ => return Err(StoreError::ContentInvalid),
                }
            }
        };
        crate::security::permissions::check_private_tree(
            self.apps_directory(),
            path.parent().ok_or(StoreError::ContentInvalid)?,
            true,
        )?;
        Ok(path)
    }
}

fn remove_incomplete_preparation(path: &Path, root: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(StoreError::SymlinkBoundary)
        }
        Ok(_) => {
            validate_cleanup_source(path)?;
            fs::remove_dir_all(path)?;
            sync_directory(root)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn retired_marker_operation(name: &std::ffi::OsStr) -> Option<Uuid> {
    let raw = name.to_str()?.strip_suffix(".retired.toml")?;
    raw.parse::<Uuid>()
        .ok()
        .filter(|operation| operation.to_string() == raw)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachResult {
    Detached,
    AlreadyDetached,
    ConfirmedMissing,
    ConfirmedRetained,
}

pub fn exact_internal_temp_name(name: &std::ffi::OsStr) -> bool {
    exact_hex_suffix(name, ".solodock-tmp-")
}

pub fn valid_temp_name(location: CleanupTempLocation, name: &str) -> bool {
    match location {
        CleanupTempLocation::AppsRoot => {
            exact_hex_suffix(std::ffi::OsStr::new(name), ".solodock-tmp-app-")
                || exact_internal_temp_name(std::ffi::OsStr::new(name))
        }
        CleanupTempLocation::AppRoot | CleanupTempLocation::Releases => {
            exact_internal_temp_name(std::ffi::OsStr::new(name))
        }
        CleanupTempLocation::ConfigRevisions => {
            exact_hex_suffix(std::ffi::OsStr::new(name), ".solodock-config-tmp-")
                || exact_internal_temp_name(std::ffi::OsStr::new(name))
        }
    }
}

fn exact_hex_suffix(name: &std::ffi::OsStr, prefix: &str) -> bool {
    let Some(value) = name.to_str() else {
        return false;
    };
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_cleanup_source(path: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(StoreError::SymlinkBoundary);
    }
    if metadata.is_dir() {
        check_private(path, true)?;
        let mut pending = vec![path.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                if ty.is_symlink() {
                    return Err(StoreError::SymlinkBoundary);
                }
                if ty.is_dir() {
                    check_private(&entry.path(), true)?;
                    pending.push(entry.path());
                } else if ty.is_file() {
                    check_private(&entry.path(), false)?;
                } else {
                    return Err(StoreError::SymlinkBoundary);
                }
            }
        }
    } else if metadata.is_file() {
        check_private(path, false)?;
    } else {
        return Err(StoreError::SymlinkBoundary);
    }
    Ok(())
}

/// Validate and measure an artifact using the same leaf-mode classification as
/// recovery. The synthetic path is never opened; it only retains source type.
pub(crate) fn logical_artifact_bytes(
    path: &Path,
    artifact: &CleanupArtifact,
) -> Result<u64, StoreError> {
    let (logical_root, disposable) = match artifact {
        CleanupArtifact::ConfigRevision { revision_id, .. } => (
            PathBuf::from("config-revisions").join(revision_id.to_string()),
            false,
        ),
        CleanupArtifact::Temporary {
            location: CleanupTempLocation::ConfigRevisions,
            name,
            ..
        } => (PathBuf::from("config-revisions").join(name), false),
        CleanupArtifact::Temporary {
            location: CleanupTempLocation::AppsRoot,
            name,
            ..
        } if name.starts_with(".solodock-tmp-app-") => (PathBuf::new(), true),
        _ => (PathBuf::from("private-artifact"), false),
    };
    let mut pending = vec![(path.to_owned(), logical_root)];
    let mut total = 0u64;
    while let Some((physical, logical)) = pending.pop() {
        let metadata = fs::symlink_metadata(&physical)?;
        if metadata.file_type().is_symlink() {
            return Err(StoreError::SymlinkBoundary);
        }
        if metadata.is_dir() {
            check_private(&physical, true)?;
            for entry in fs::read_dir(&physical)? {
                let entry = entry?;
                pending.push((entry.path(), logical.join(entry.file_name())));
            }
        } else if metadata.is_file() {
            super::recovery::check_managed_tree_file(
                &physical,
                Path::new(""),
                &logical,
                disposable,
            )?;
            total = total
                .checked_add(metadata.len())
                .ok_or(StoreError::ContentInvalid)?;
        } else {
            return Err(StoreError::SymlinkBoundary);
        }
    }
    Ok(total)
}

fn file_identity(path: &Path) -> Result<(u64, u64, u32), StoreError> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.mode() & libc::S_IFMT,
    ))
}

fn file_identity_if_exists(path: &Path) -> Result<Option<(u64, u64, u32)>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some((
            metadata.dev(),
            metadata.ino(),
            metadata.mode() & libc::S_IFMT,
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn sign_marker(marker: &CleanupMarker, key: &[u8]) -> String {
    #[derive(Serialize)]
    struct Signed<'a> {
        schema_version: u32,
        operation_id: Uuid,
        plan_hash: &'a str,
        items: &'a [CleanupArtifact],
    }
    let payload = toml::to_string(&Signed {
        schema_version: marker.schema_version,
        operation_id: marker.operation_id,
        plan_hash: &marker.plan_hash,
        items: &marker.items,
    })
    .expect("cleanup marker serializes");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(payload.as_bytes());
    encode_hex(&mac.finalize().into_bytes())
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    use super::*;

    #[test]
    fn prepare_resumes_an_exact_incomplete_operation_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = AppStore::initialize_verified(
            root.path().join("apps"),
            b"cleanup-preparation-test-key".to_vec(),
        )
        .unwrap();
        let trash = store.apps_directory().join(CLEANUP_TRASH_DIRECTORY);
        fs::DirBuilder::new().mode(0o700).create(&trash).unwrap();
        let operation = Uuid::new_v4();
        let preparation = trash.join(format!(".solodock-tmp-{}", operation.simple()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&preparation)
            .unwrap();
        fs::write(
            preparation.join("interrupted"),
            b"before marker publication",
        )
        .unwrap();
        fs::set_permissions(
            preparation.join("interrupted"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        assert!(store.cleanup_tombstones().unwrap().is_empty());
        store
            .prepare_cleanup_tombstone(operation, &[0x12, 0x34], &[])
            .unwrap();
        assert!(!preparation.exists());
        assert_eq!(store.cleanup_tombstones().unwrap(), vec![operation]);
        let marker = store.read_cleanup_marker(operation).unwrap();
        assert_eq!(marker.plan_hash, "1234");
        assert!(marker.items.is_empty());
    }

    #[test]
    fn detach_reports_a_confirmed_retained_item_when_rename_cannot_start() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = AppStore::initialize_verified(
            root.path().join("apps"),
            b"cleanup-rename-test-key".to_vec(),
        )
        .unwrap();
        let name = format!(".solodock-tmp-{}", Uuid::new_v4().simple());
        let source = store.apps_directory().join(&name);
        fs::write(&source, b"retained").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let artifact = CleanupArtifact::Temporary {
            app_id: None,
            location: CleanupTempLocation::AppsRoot,
            name,
        };
        let operation = Uuid::new_v4();
        store
            .prepare_cleanup_tombstone(operation, &[0x56], std::slice::from_ref(&artifact))
            .unwrap();
        let payload = store.cleanup_tombstone_path(operation).join("payload");
        store.fail_next_cleanup_rename();
        assert_eq!(
            store
                .detach_cleanup_artifact(operation, 0, &artifact)
                .unwrap(),
            DetachResult::ConfirmedRetained
        );
        assert_eq!(fs::read(&source).unwrap(), b"retained");
        assert!(!payload.join("0").exists());
    }
}
