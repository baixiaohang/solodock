use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{error::RegistryError, reference::validate_logical_registry};
use crate::{
    app_store::{StoreError, atomic::rename_no_replace, sync_directory},
    security::{
        permissions::{check_private, check_private_tree, ensure_private_directory},
        secret::SecretValue,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialMetadata {
    pub schema_version: u32,
    pub id: Uuid,
    pub revision: Uuid,
    pub registry: String,
    pub username: String,
    pub secret_revision: Uuid,
    pub last_operation_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub rotated_at: OffsetDateTime,
    pub integrity_hmac: String,
}

pub struct LoadedCredential {
    pub metadata: CredentialMetadata,
    pub secret: SecretValue,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialDeletionMarker {
    schema_version: u32,
    credential_id: Uuid,
    operation_id: Uuid,
}

#[derive(Clone)]
pub struct CredentialStore {
    root: PathBuf,
    key: Arc<Vec<u8>>,
    #[cfg(test)]
    fail_finalize_remove: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    fail_finalize_sync: Arc<std::sync::atomic::AtomicBool>,
}

impl CredentialStore {
    pub fn initialize(root: PathBuf, key: Vec<u8>) -> Result<Self, StoreError> {
        ensure_private_directory(&root)?;
        ensure_private_directory(&root.join(".trash"))?;
        Ok(Self {
            root,
            key: Arc::new(key),
            #[cfg(test)]
            fail_finalize_remove: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            fail_finalize_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn list(&self) -> Result<Vec<CredentialMetadata>, StoreError> {
        check_private(&self.root, true)?;
        let mut credentials = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name() == ".trash" {
                continue;
            }
            if let Some(raw) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.strip_prefix(".solodock-credential-tmp-"))
            {
                let operation = Uuid::parse_str(raw).map_err(|_| StoreError::ContentInvalid)?;
                if raw == operation.simple().to_string() && entry.file_type()?.is_dir() {
                    continue;
                }
                return Err(StoreError::ContentInvalid);
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(StoreError::SymlinkBoundary);
            }
            if !kind.is_dir() {
                return Err(StoreError::ContentInvalid);
            }
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.parse::<Uuid>().ok())
            else {
                return Err(StoreError::ContentInvalid);
            };
            if entry.file_name() != std::ffi::OsStr::new(&id.to_string()) {
                return Err(StoreError::ContentInvalid);
            }
            credentials.push(self.load_metadata(id)?);
        }
        credentials.sort_by_key(|value| value.created_at);
        if credentials.len() > 32 {
            return Err(StoreError::ContentInvalid);
        }
        Ok(credentials)
    }

    /// Removes only implementation-owned staging artifacts and unreferenced
    /// immutable secret revisions. This is startup-only recovery and must run
    /// before HTTP mutations are accepted.
    pub fn startup_cleanup(&self) -> Result<(), StoreError> {
        check_private(&self.root, true)?;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or(StoreError::ContentInvalid)?
                .to_owned();
            if name == ".trash" {
                check_private(&entry.path(), true)?;
                for tombstone in fs::read_dir(entry.path())? {
                    let tombstone = tombstone?;
                    let text = tombstone
                        .file_name()
                        .to_str()
                        .ok_or(StoreError::ContentInvalid)?
                        .to_owned();
                    let (credential, operation) = (text.len() == 73)
                        .then(|| text.get(..36).zip(text.get(37..)))
                        .flatten()
                        .ok_or(StoreError::ContentInvalid)?;
                    let credential =
                        Uuid::parse_str(credential).map_err(|_| StoreError::ContentInvalid)?;
                    let operation =
                        Uuid::parse_str(operation).map_err(|_| StoreError::ContentInvalid)?;
                    if text != format!("{credential}-{operation}")
                        || self.exact_tombstone(credential, operation)?.is_none()
                    {
                        return Err(StoreError::ContentInvalid);
                    }
                }
                continue;
            }
            if let Some(raw) = name.strip_prefix(".solodock-credential-tmp-") {
                let operation = Uuid::parse_str(raw).map_err(|_| StoreError::ContentInvalid)?;
                if raw != operation.simple().to_string() || !entry.file_type()?.is_dir() {
                    return Err(StoreError::ContentInvalid);
                }
                check_private_tree(&self.root, &entry.path(), true)?;
                fs::remove_dir_all(entry.path())?;
                continue;
            }
            let id = Uuid::parse_str(&name).map_err(|_| StoreError::ContentInvalid)?;
            if name != id.to_string() || !entry.file_type()?.is_dir() {
                return Err(StoreError::ContentInvalid);
            }
            let metadata = self.load_metadata(id)?;
            let revisions = entry.path().join("secret-revisions");
            check_private_tree(&self.root, &revisions, true)?;
            for revision in fs::read_dir(&revisions)? {
                let revision = revision?;
                let revision_name = revision
                    .file_name()
                    .to_str()
                    .ok_or(StoreError::ContentInvalid)?
                    .to_owned();
                let removable = if let Some(raw) =
                    revision_name.strip_prefix(".solodock-secret-tmp-")
                {
                    let operation = Uuid::parse_str(raw).map_err(|_| StoreError::ContentInvalid)?;
                    raw == operation.simple().to_string()
                } else {
                    let revision_id =
                        Uuid::parse_str(&revision_name).map_err(|_| StoreError::ContentInvalid)?;
                    if revision_name != revision_id.to_string() {
                        return Err(StoreError::ContentInvalid);
                    }
                    revision_id != metadata.secret_revision
                };
                if !revision.file_type()?.is_dir() {
                    return Err(StoreError::ContentInvalid);
                }
                if removable {
                    check_private_tree(&self.root, &revision.path(), true)?;
                    fs::remove_dir_all(revision.path())?;
                }
            }
            sync_directory(&revisions)?;
        }
        sync_directory(&self.root)
    }

    pub fn load(&self, id: Uuid) -> Result<LoadedCredential, StoreError> {
        let metadata = self.load_metadata(id)?;
        let path = self
            .root
            .join(id.to_string())
            .join("secret-revisions")
            .join(metadata.secret_revision.to_string())
            .join("token");
        check_private_tree(&self.root, &path, false)?;
        let mut token = fs::read_to_string(path)?;
        validate_secret(&token).map_err(|_| StoreError::ContentInvalid)?;
        let secret = SecretValue::new(token.clone());
        token.zeroize();
        Ok(LoadedCredential { metadata, secret })
    }

    pub fn create(
        &self,
        id: Uuid,
        operation_id: Uuid,
        registry: &str,
        username: &str,
        secret: &SecretValue,
    ) -> Result<CredentialMetadata, StoreError> {
        let target = self.root.join(id.to_string());
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                let existing = self.load_metadata(id)?;
                return if existing.last_operation_id == operation_id {
                    Ok(existing)
                } else {
                    Err(StoreError::AppConflict)
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if self.list()?.len() >= 32 {
            return Err(StoreError::ContentInvalid);
        }
        let registry =
            validate_logical_registry(registry).map_err(|_| StoreError::ContentInvalid)?;
        validate_username(username).map_err(|_| StoreError::ContentInvalid)?;
        validate_secret(secret.expose()).map_err(|_| StoreError::ContentInvalid)?;
        if self
            .list()?
            .iter()
            .any(|item| item.registry == registry && item.username == username)
        {
            return Err(StoreError::AppConflict);
        }
        let temp = self.root.join(format!(
            ".solodock-credential-tmp-{}",
            operation_id.simple()
        ));
        let now = OffsetDateTime::now_utc();
        let secret_revision = Uuid::new_v4();
        let mut metadata = CredentialMetadata {
            schema_version: 1,
            id,
            revision: Uuid::new_v4(),
            registry,
            username: username.into(),
            secret_revision,
            last_operation_id: operation_id,
            created_at: now,
            rotated_at: now,
            integrity_hmac: String::new(),
        };
        metadata.integrity_hmac = self.sign(&metadata, secret.expose());
        let result = (|| {
            create_dir(&temp)?;
            create_dir(&temp.join("secret-revisions"))?;
            let revision = temp
                .join("secret-revisions")
                .join(secret_revision.to_string());
            create_dir(&revision)?;
            write_new(&revision.join("token"), secret.expose().as_bytes())?;
            sync_directory(&revision)?;
            sync_directory(&temp.join("secret-revisions"))?;
            write_new(
                &temp.join("credential.toml"),
                toml::to_string(&metadata)
                    .map_err(|_| StoreError::ContentInvalid)?
                    .as_bytes(),
            )?;
            sync_directory(&temp)?;
            rename_no_replace(&temp, &target).map_err(|_| StoreError::AppConflict)?;
            sync_directory(&self.root)?;
            Ok(metadata.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temp);
        }
        result
    }

    pub fn update(
        &self,
        id: Uuid,
        expected: Uuid,
        operation_id: Uuid,
        username: &str,
        replacement: Option<&SecretValue>,
    ) -> Result<CredentialMetadata, StoreError> {
        validate_username(username).map_err(|_| StoreError::ContentInvalid)?;
        let loaded = self.load(id)?;
        if loaded.metadata.last_operation_id == operation_id {
            return Ok(loaded.metadata);
        }
        if loaded.metadata.revision != expected {
            return Err(StoreError::RevisionStale);
        }
        if self.list()?.iter().any(|candidate| {
            candidate.id != id
                && candidate.registry == loaded.metadata.registry
                && candidate.username == username
        }) {
            return Err(StoreError::AppConflict);
        }
        let directory = self.root.join(id.to_string());
        let mut metadata = loaded.metadata.clone();
        let now = OffsetDateTime::now_utc();
        if let Some(secret) = replacement {
            validate_secret(secret.expose()).map_err(|_| StoreError::ContentInvalid)?;
            let revision_id = Uuid::new_v4();
            let revisions = directory.join("secret-revisions");
            let temp = revisions.join(format!(".solodock-secret-tmp-{}", operation_id.simple()));
            let target = revisions.join(revision_id.to_string());
            create_dir(&temp)?;
            write_new(&temp.join("token"), secret.expose().as_bytes())?;
            sync_directory(&temp)?;
            rename_no_replace(&temp, &target).map_err(|_| StoreError::ConfigRevisionConflict)?;
            sync_directory(&revisions)?;
            metadata.secret_revision = revision_id;
            metadata.rotated_at = now;
        }
        metadata.username = username.into();
        metadata.revision = Uuid::new_v4();
        metadata.last_operation_id = operation_id;
        let secret = replacement
            .map(|s| s.expose())
            .unwrap_or_else(|| loaded.secret.expose());
        metadata.integrity_hmac = self.sign(&metadata, secret);
        let contents = toml::to_string(&metadata).map_err(|_| StoreError::ContentInvalid)?;
        crate::app_store::atomic::AtomicWriter::write(
            &directory.join("credential.toml"),
            contents.as_bytes(),
            0o600,
        )?;
        Ok(metadata)
    }

    pub fn tombstone(&self, id: Uuid, operation_id: Uuid) -> Result<PathBuf, StoreError> {
        if let Some(path) = self.exact_tombstone(id, operation_id)? {
            return Ok(path);
        }
        self.load_metadata(id)?;
        let source = self.root.join(id.to_string());
        let target = self
            .root
            .join(".trash")
            .join(format!("{id}-{operation_id}"));
        let marker = CredentialDeletionMarker {
            schema_version: 1,
            credential_id: id,
            operation_id,
        };
        crate::app_store::atomic::AtomicWriter::write(
            &source.join("deletion.toml"),
            toml::to_string(&marker)
                .map_err(|_| StoreError::ContentInvalid)?
                .as_bytes(),
            0o600,
        )?;
        rename_no_replace(&source, &target).map_err(|_| StoreError::AppConflict)?;
        sync_directory(&self.root.join(".trash"))?;
        sync_directory(&self.root)?;
        Ok(target)
    }

    pub fn finalize_tombstone(&self, id: Uuid, operation_id: Uuid) -> Result<(), StoreError> {
        if let Some(path) = self.exact_tombstone(id, operation_id)? {
            check_private_tree(&self.root, &path, true)?;
            #[cfg(test)]
            if self
                .fail_finalize_remove
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(
                    std::io::Error::other("injected credential tombstone remove failure").into(),
                );
            }
            fs::remove_dir_all(path)?;
        }
        // An earlier attempt may have made the removal visible before its
        // parent fsync failed. Always sync the parent even when the exact
        // tombstone is already absent so a retry repairs that ambiguous edge.
        #[cfg(test)]
        if self
            .fail_finalize_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::other("injected credential tombstone sync failure").into());
        }
        sync_directory(&self.root.join(".trash"))
    }

    /// Enumerates only canonical, integrity-checked deletion markers. The
    /// caller must independently prove the matching mutation response is
    /// durable before finalizing any returned tombstone.
    pub fn tombstones(&self) -> Result<Vec<(Uuid, Uuid)>, StoreError> {
        let trash = self.root.join(".trash");
        check_private_tree(&self.root, &trash, true)?;
        let mut values = Vec::new();
        for entry in fs::read_dir(trash)? {
            let entry = entry?;
            let text = entry
                .file_name()
                .to_str()
                .ok_or(StoreError::ContentInvalid)?
                .to_owned();
            let (credential, operation) = (text.len() == 73)
                .then(|| text.get(..36).zip(text.get(37..)))
                .flatten()
                .ok_or(StoreError::ContentInvalid)?;
            let credential = Uuid::parse_str(credential).map_err(|_| StoreError::ContentInvalid)?;
            let operation = Uuid::parse_str(operation).map_err(|_| StoreError::ContentInvalid)?;
            if text != format!("{credential}-{operation}")
                || self.exact_tombstone(credential, operation)?.is_none()
            {
                return Err(StoreError::ContentInvalid);
            }
            values.push((credential, operation));
        }
        values.sort_unstable();
        Ok(values)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_finalize_remove_for_test(&self) {
        self.fail_finalize_remove
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_finalize_sync_for_test(&self) {
        self.fail_finalize_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn exact_tombstone(
        &self,
        id: Uuid,
        operation_id: Uuid,
    ) -> Result<Option<PathBuf>, StoreError> {
        let path = self
            .root
            .join(".trash")
            .join(format!("{id}-{operation_id}"));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(StoreError::ContentInvalid);
            }
            Ok(_) => {}
        }
        let marker_path = path.join("deletion.toml");
        check_private_tree(&self.root, &marker_path, false)?;
        let marker: CredentialDeletionMarker = toml::from_str(&fs::read_to_string(marker_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        if marker.schema_version != 1
            || marker.credential_id != id
            || marker.operation_id != operation_id
        {
            return Err(StoreError::ContentInvalid);
        }
        Ok(Some(path))
    }

    fn load_metadata(&self, id: Uuid) -> Result<CredentialMetadata, StoreError> {
        let directory = self.root.join(id.to_string());
        let path = directory.join("credential.toml");
        check_private_tree(&self.root, &path, false)?;
        let metadata: CredentialMetadata =
            toml::from_str(&fs::read_to_string(path)?).map_err(|_| StoreError::ContentInvalid)?;
        if metadata.schema_version != 1 || metadata.id != id || metadata.integrity_hmac.len() != 64
        {
            return Err(StoreError::ContentInvalid);
        }
        let token_path = directory
            .join("secret-revisions")
            .join(metadata.secret_revision.to_string())
            .join("token");
        check_private_tree(&self.root, &token_path, false)?;
        let mut token = fs::read_to_string(token_path)?;
        let expected = self.sign(&metadata, &token);
        let valid: bool =
            subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), metadata.integrity_hmac.as_bytes())
                .into();
        token.zeroize();
        if !valid {
            return Err(StoreError::ContentInvalid);
        }
        Ok(metadata)
    }

    fn sign(&self, metadata: &CredentialMetadata, secret: &str) -> String {
        let mut canonical = metadata.clone();
        canonical.integrity_hmac.clear();
        let metadata = toml::to_string(&canonical).expect("credential metadata serializes");
        let secret_hash = Sha256::digest(secret.as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key accepted");
        mac.update(b"solodock/registry-credential/v1\0");
        mac.update(metadata.as_bytes());
        mac.update(&secret_hash);
        format!("{:x}", mac.finalize().into_bytes())
    }
}

fn create_dir(path: &Path) -> Result<(), StoreError> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn write_new(path: &Path, value: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(value)?;
    file.sync_all()?;
    Ok(())
}
fn validate_username(value: &str) -> Result<(), RegistryError> {
    if value.is_empty() || value.len() > 255 || value.bytes().any(|b| b.is_ascii_control()) {
        Err(RegistryError::CredentialInvalid)
    } else {
        Ok(())
    }
}
fn validate_secret(value: &str) -> Result<(), RegistryError> {
    if value.is_empty() || value.len() > 8192 || value.bytes().any(|b| b.is_ascii_control()) {
        Err(RegistryError::CredentialInvalid)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn credential_is_write_only_rotatable_and_integrity_checked() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store =
            CredentialStore::initialize(root.path().join("credentials"), vec![7; 32]).unwrap();
        let id = Uuid::new_v4();
        let created = store
            .create(
                id,
                Uuid::new_v4(),
                "ghcr.io",
                "admin",
                &SecretValue::new("canary-one".into()),
            )
            .unwrap();
        let rotated = store
            .update(
                id,
                created.revision,
                Uuid::new_v4(),
                "admin",
                Some(&SecretValue::new("canary-two".into())),
            )
            .unwrap();
        assert_ne!(created.secret_revision, rotated.secret_revision);
        assert_eq!(store.load(id).unwrap().secret.expose(), "canary-two");
        let path = root
            .path()
            .join("credentials")
            .join(id.to_string())
            .join("secret-revisions")
            .join(rotated.secret_revision.to_string())
            .join("token");
        fs::write(path, "tampered").unwrap();
        assert!(store.load(id).is_err());
    }

    #[test]
    fn operation_owned_artifacts_resume_and_tombstone_is_not_finalized_early() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store =
            CredentialStore::initialize(root.path().join("credentials"), vec![9; 32]).unwrap();
        let id = Uuid::new_v4();
        let create_operation = Uuid::new_v4();
        let created = store
            .create(
                id,
                create_operation,
                "ghcr.io",
                "admin",
                &SecretValue::new("canary-create".into()),
            )
            .unwrap();
        assert_eq!(
            store
                .create(
                    id,
                    create_operation,
                    "ghcr.io",
                    "admin",
                    &SecretValue::new("canary-create".into()),
                )
                .unwrap()
                .revision,
            created.revision
        );

        let update_operation = Uuid::new_v4();
        let updated = store
            .update(id, created.revision, update_operation, "robot", None)
            .unwrap();
        assert_eq!(
            store
                .update(id, created.revision, update_operation, "robot", None)
                .unwrap()
                .revision,
            updated.revision
        );

        let delete_operation = Uuid::new_v4();
        let tombstone = store.tombstone(id, delete_operation).unwrap();
        assert!(tombstone.exists());
        assert_eq!(store.tombstone(id, delete_operation).unwrap(), tombstone);
        store.startup_cleanup().unwrap();
        assert!(
            tombstone.exists(),
            "startup recovery preserves resumable deletion"
        );
        store.finalize_tombstone(id, delete_operation).unwrap();
        assert!(!tombstone.exists());
        store.finalize_tombstone(id, delete_operation).unwrap();
    }
}
