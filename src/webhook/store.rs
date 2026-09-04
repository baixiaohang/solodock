use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    app_store::{AppStore, StoreError, atomic::rename_no_replace, sync_directory},
    security::{
        permissions::{PermissionError, check_private_tree},
        secret::SecretValue,
    },
};

use super::protocol::decode_secret;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookMetadata {
    pub schema_version: u32,
    pub app_id: Uuid,
    pub enabled: bool,
    pub metadata_revision: Uuid,
    #[serde(default)]
    pub secret_revision: Option<Uuid>,
    pub last_operation_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub rotated_at: Option<OffsetDateTime>,
    pub integrity_hmac: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebhookStatus {
    pub configured: bool,
    pub degraded: bool,
    pub metadata_revision: Option<Uuid>,
    pub secret_revision: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub rotated_at: Option<OffsetDateTime>,
}

pub struct LoadedWebhook {
    pub metadata: WebhookMetadata,
    pub secret: SecretValue,
}

#[derive(Clone, Debug)]
pub struct WebhookRecoveryInventory {
    pub apps: Vec<WebhookRecoveryApp>,
}

#[derive(Clone, Debug)]
pub struct WebhookRecoveryApp {
    pub app_id: Uuid,
    pub metadata: Option<WebhookMetadata>,
    pub revisions: Vec<WebhookRecoveryRevision>,
    pub operation_temps: Vec<Uuid>,
}

#[derive(Clone, Debug)]
pub struct WebhookRecoveryRevision {
    pub revision_id: Uuid,
    pub operation_id: Uuid,
    pub current: bool,
}

#[derive(Clone)]
pub struct WebhookStore {
    apps: AppStore,
    key: Arc<Vec<u8>>,
    #[cfg(test)]
    fail_next_cleanup_remove: Arc<std::sync::atomic::AtomicBool>,
}

impl WebhookStore {
    pub fn new(apps: AppStore, key: Vec<u8>) -> Self {
        Self {
            apps,
            key: Arc::new(key),
            #[cfg(test)]
            fail_next_cleanup_remove: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn status(&self, app_id: Uuid) -> Result<WebhookStatus, StoreError> {
        match self.load_metadata(app_id) {
            Ok(value) => {
                let degraded = if let Some(revision) = value.secret_revision {
                    match self.load_revision(app_id, revision, Some(&value)) {
                        Ok(_) => false,
                        Err(StoreError::ContentInvalid) => true,
                        Err(StoreError::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
                            ) =>
                        {
                            true
                        }
                        Err(StoreError::Permission(PermissionError::Io(error)))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
                            ) =>
                        {
                            true
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    false
                };
                Ok(WebhookStatus {
                    configured: value.enabled,
                    degraded,
                    metadata_revision: Some(value.metadata_revision),
                    secret_revision: value.secret_revision,
                    created_at: Some(value.created_at),
                    rotated_at: value.rotated_at,
                })
            }
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let revisions = self
                    .apps
                    .app_directory(app_id)
                    .join("webhook-secret-revisions");
                let degraded = match fs::symlink_metadata(&revisions) {
                    Ok(_) => {
                        check_private_tree(self.apps.apps_directory(), &revisions, true)?;
                        true
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                };
                Ok(WebhookStatus {
                    configured: false,
                    degraded,
                    metadata_revision: None,
                    secret_revision: None,
                    created_at: None,
                    rotated_at: None,
                })
            }
            Err(StoreError::ContentInvalid) => Ok(WebhookStatus {
                configured: false,
                degraded: true,
                metadata_revision: None,
                secret_revision: None,
                created_at: None,
                rotated_at: None,
            }),
            Err(error) => Err(error),
        }
    }

    pub fn load_current(&self, app_id: Uuid) -> Result<LoadedWebhook, StoreError> {
        if !self.apps.read_metadata(app_id)?.is_configured() {
            return Err(StoreError::ContentInvalid);
        }
        let metadata = self.load_metadata(app_id)?;
        if !metadata.enabled {
            return Err(StoreError::ContentInvalid);
        }
        let revision = metadata.secret_revision.ok_or(StoreError::ContentInvalid)?;
        let secret = self.load_revision(app_id, revision, Some(&metadata))?;
        Ok(LoadedWebhook { metadata, secret })
    }

    pub fn configure(
        &self,
        app_id: Uuid,
        expected: Option<Uuid>,
        operation_id: Uuid,
        secret: &SecretValue,
    ) -> Result<WebhookMetadata, StoreError> {
        if !self.apps.read_metadata(app_id)?.is_configured() {
            return Err(StoreError::ContentInvalid);
        }
        let decoded_secret = decode_secret(secret).map_err(|_| StoreError::ContentInvalid)?;
        let existing = match self.load_metadata(app_id) {
            Ok(value) => Some(value),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(StoreError::ContentInvalid) if expected.is_none() => None,
            Err(error) => return Err(error),
        };
        if let Some(value) = &existing {
            if value.last_operation_id == operation_id {
                self.repair_durability(app_id, value.secret_revision)?;
                return Ok(value.clone());
            }
            if value.enabled && expected != Some(value.metadata_revision) {
                return Err(StoreError::RevisionStale);
            }
            if !value.enabled && expected.is_some() {
                return Err(StoreError::RevisionStale);
            }
        } else if expected.is_some() {
            return Err(StoreError::RevisionStale);
        }
        let app = self.apps.app_directory(app_id);
        let revisions = app.join("webhook-secret-revisions");
        if !revisions.exists() {
            fs::DirBuilder::new().mode(0o700).create(&revisions)?;
            sync_directory(&app)?;
        }
        check_private_tree(self.apps.apps_directory(), &revisions, true)?;
        // The operation ID is also the immutable revision ID. This makes the
        // post-rename/pre-metadata crash window exactly resumable.
        let revision_id = operation_id;
        let temporary = revisions.join(format!(".solodock-webhook-tmp-{}", operation_id.simple()));
        let target = revisions.join(revision_id.to_string());
        if !target.exists() {
            match fs::symlink_metadata(&temporary) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(StoreError::SymlinkBoundary);
                    }
                    check_private_tree(self.apps.apps_directory(), &temporary, true)?;
                    // This exact directory is owned by the idempotency
                    // operation. A resumed attempt can safely discard a
                    // partial write and deterministically rebuild it.
                    fs::remove_dir_all(&temporary)?;
                    sync_directory(&revisions)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            create_dir(&temporary)?;
            write_new(&temporary.join("secret"), &decoded_secret)?;
            let revision = SecretRevision {
                schema_version: 1,
                app_id,
                secret_revision: revision_id,
                operation_id,
                created_at: OffsetDateTime::now_utc(),
                secret_hmac: sign(
                    &self.key,
                    b"solodock/webhook-secret-value/v1\0",
                    &decoded_secret,
                ),
                integrity_hmac: String::new(),
            };
            let revision = self.sign_revision(revision);
            write_new(
                &temporary.join("revision.toml"),
                toml::to_string(&revision)
                    .map_err(|_| StoreError::ContentInvalid)?
                    .as_bytes(),
            )?;
            sync_directory(&temporary)?;
            rename_no_replace(&temporary, &target)
                .map_err(|_| StoreError::ConfigRevisionConflict)?;
            sync_directory(&revisions)?;
        } else {
            let existing_secret = self.load_revision(app_id, revision_id, None)?;
            if !existing_secret.constant_time_eq(secret.expose()) {
                return Err(StoreError::ConfigRevisionConflict);
            }
        }
        let now = OffsetDateTime::now_utc();
        let created_at = existing.as_ref().map_or(now, |value| value.created_at);
        let mut metadata = WebhookMetadata {
            schema_version: 1,
            app_id,
            enabled: true,
            metadata_revision: Uuid::new_v4(),
            secret_revision: Some(revision_id),
            last_operation_id: operation_id,
            created_at,
            updated_at: now,
            rotated_at: existing.as_ref().map(|_| now),
            integrity_hmac: String::new(),
        };
        metadata.integrity_hmac = self.sign_metadata(&metadata);
        crate::app_store::atomic::AtomicWriter::write(
            &app.join("webhook.toml"),
            toml::to_string(&metadata)
                .map_err(|_| StoreError::ContentInvalid)?
                .as_bytes(),
            0o600,
        )?;
        Ok(metadata)
    }

    pub fn revoke(
        &self,
        app_id: Uuid,
        expected: Uuid,
        operation_id: Uuid,
    ) -> Result<WebhookMetadata, StoreError> {
        if !self.apps.read_metadata(app_id)?.is_configured() {
            return Err(StoreError::ContentInvalid);
        }
        let mut metadata = self.load_metadata(app_id)?;
        if metadata.last_operation_id == operation_id {
            self.repair_durability(app_id, metadata.secret_revision)?;
            return Ok(metadata);
        }
        if !metadata.enabled || metadata.metadata_revision != expected {
            return Err(StoreError::RevisionStale);
        }
        metadata.enabled = false;
        metadata.secret_revision = None;
        metadata.metadata_revision = Uuid::new_v4();
        metadata.last_operation_id = operation_id;
        metadata.updated_at = OffsetDateTime::now_utc();
        metadata.integrity_hmac = self.sign_metadata(&metadata);
        crate::app_store::atomic::AtomicWriter::write(
            &self.apps.app_directory(app_id).join("webhook.toml"),
            toml::to_string(&metadata)
                .map_err(|_| StoreError::ContentInvalid)?
                .as_bytes(),
            0o600,
        )?;
        Ok(metadata)
    }

    pub fn all_secret_bytes(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut result = Vec::new();
        for entry in fs::read_dir(self.apps.apps_directory())? {
            let entry = entry?;
            let Some(app_id) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.parse::<Uuid>().ok())
            else {
                continue;
            };
            if entry.file_name() != std::ffi::OsStr::new(&app_id.to_string()) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            check_private_tree(self.apps.apps_directory(), &entry.path(), true)?;
            let expected_current = match self.load_metadata(app_id) {
                Ok(metadata) if metadata.enabled => metadata.secret_revision,
                Ok(_) => None,
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                // A damaged metadata file does not prevent us from loading
                // and redacting every immutable revision. Recovery exposes
                // the damaged substate separately for repair.
                Err(StoreError::ContentInvalid) => None,
                Err(error) => return Err(error),
            };
            let mut found_current = expected_current.is_none();
            let revisions = entry.path().join("webhook-secret-revisions");
            match fs::read_dir(&revisions) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if expected_current.is_some() {
                        return Err(StoreError::ContentInvalid);
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
                Ok(entries) => {
                    for revision in entries {
                        let revision = revision?;
                        let Some(id) = revision
                            .file_name()
                            .to_str()
                            .and_then(|v| v.parse::<Uuid>().ok())
                        else {
                            if revision
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".solodock-webhook-tmp-")
                            {
                                continue;
                            }
                            return Err(StoreError::ContentInvalid);
                        };
                        found_current |= expected_current == Some(id);
                        let secret = self.load_revision(app_id, id, None)?;
                        let decoded =
                            decode_secret(&secret).map_err(|_| StoreError::ContentInvalid)?;
                        result.push(secret.expose().as_bytes().to_vec());
                        result.push(decoded.to_vec());
                    }
                }
            }
            if !found_current {
                return Err(StoreError::ContentInvalid);
            }
        }
        Ok(result)
    }

    pub fn configured_count(&self) -> Result<usize, StoreError> {
        let report = self.apps.scan_read_only()?;
        report.valid_apps.iter().try_fold(0usize, |count, app| {
            self.status(app.app_id).and_then(|status| {
                if status.degraded {
                    Err(StoreError::ContentInvalid)
                } else {
                    Ok(count + usize::from(status.configured))
                }
            })
        })
    }

    pub fn cleanup_unreferenced(&self, app_id: Uuid) -> Result<(), StoreError> {
        let current = match self.load_metadata(app_id) {
            Ok(value) => value.secret_revision,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let revisions = self
            .apps
            .app_directory(app_id)
            .join("webhook-secret-revisions");
        let entries = match fs::read_dir(&revisions) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let remove = if let Ok(id) = name.parse::<Uuid>() {
                if name != id.to_string() {
                    return Err(StoreError::ContentInvalid);
                }
                Some(id) != current
            } else if let Some(simple) = name.strip_prefix(".solodock-webhook-tmp-") {
                let operation = Uuid::parse_str(simple).map_err(|_| StoreError::ContentInvalid)?;
                if simple != operation.simple().to_string() {
                    return Err(StoreError::ContentInvalid);
                }
                // Temp cleanup requires an exact ledger operation and is
                // handled by `discard_operation_temp`.
                false
            } else {
                return Err(StoreError::ContentInvalid);
            };
            if remove {
                check_private_tree(self.apps.apps_directory(), &entry.path(), true)?;
                #[cfg(test)]
                if self
                    .fail_next_cleanup_remove
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(std::io::Error::other("injected webhook cleanup failure").into());
                }
                fs::remove_dir_all(entry.path())?;
            }
        }
        sync_directory(&revisions)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_cleanup_remove_for_test(&self) {
        self.fail_next_cleanup_remove
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Returns one integrity-checked view of every webhook artifact that can
    /// depend on an idempotency proof. GC and finalizers must share this view
    /// so damaged metadata can never be interpreted as an empty dependency set.
    pub fn recovery_inventory(&self) -> Result<WebhookRecoveryInventory, StoreError> {
        let mut apps = Vec::new();
        for entry in fs::read_dir(self.apps.apps_directory())? {
            let entry = entry?;
            let Some(app_id) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<Uuid>().ok())
            else {
                continue;
            };
            if entry.file_name() != std::ffi::OsStr::new(&app_id.to_string()) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            check_private_tree(self.apps.apps_directory(), &entry.path(), true)?;
            let metadata = match self.load_metadata(app_id) {
                Ok(value) => Some(value),
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let revisions_path = entry.path().join("webhook-secret-revisions");
            let revisions = match fs::read_dir(&revisions_path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if metadata
                        .as_ref()
                        .is_some_and(|value| value.secret_revision.is_some())
                    {
                        return Err(StoreError::ContentInvalid);
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            check_private_tree(self.apps.apps_directory(), &revisions_path, true)?;
            let mut canonical = Vec::new();
            let mut operation_temps = Vec::new();
            let mut found_current = metadata
                .as_ref()
                .and_then(|value| value.secret_revision)
                .is_none();
            for revision in revisions {
                let revision = revision?;
                let file_type = revision.file_type()?;
                if file_type.is_symlink() || !file_type.is_dir() {
                    return Err(StoreError::SymlinkBoundary);
                }
                let text = revision.file_name().to_string_lossy().into_owned();
                if let Some(simple) = text.strip_prefix(".solodock-webhook-tmp-") {
                    check_private_tree(self.apps.apps_directory(), &revision.path(), true)?;
                    let operation =
                        Uuid::parse_str(simple).map_err(|_| StoreError::ContentInvalid)?;
                    if simple != operation.simple().to_string() {
                        return Err(StoreError::ContentInvalid);
                    }
                    validate_revision_directory_entries(
                        self.apps.apps_directory(),
                        &revision.path(),
                        true,
                    )?;
                    operation_temps.push(operation);
                    continue;
                }
                let revision_id = Uuid::parse_str(&text).map_err(|_| StoreError::ContentInvalid)?;
                if text != revision_id.to_string() {
                    return Err(StoreError::ContentInvalid);
                }
                let (record, _) = self.load_revision_record(app_id, revision_id, None)?;
                let current =
                    metadata.as_ref().and_then(|value| value.secret_revision) == Some(revision_id);
                found_current |= current;
                canonical.push(WebhookRecoveryRevision {
                    revision_id,
                    operation_id: record.operation_id,
                    current,
                });
            }
            if !found_current {
                return Err(StoreError::ContentInvalid);
            }
            canonical.sort_unstable_by_key(|value| value.revision_id);
            operation_temps.sort_unstable();
            apps.push(WebhookRecoveryApp {
                app_id,
                metadata,
                revisions: canonical,
                operation_temps,
            });
        }
        apps.sort_unstable_by_key(|value| value.app_id);
        Ok(WebhookRecoveryInventory { apps })
    }

    pub fn discard_operation_temp(
        &self,
        app_id: Uuid,
        operation_id: Uuid,
    ) -> Result<(), StoreError> {
        let revisions = self
            .apps
            .app_directory(app_id)
            .join("webhook-secret-revisions");
        let temporary = revisions.join(format!(".solodock-webhook-tmp-{}", operation_id.simple()));
        match fs::symlink_metadata(&temporary) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(StoreError::SymlinkBoundary);
                }
                check_private_tree(self.apps.apps_directory(), &temporary, true)?;
                validate_revision_directory_entries(self.apps.apps_directory(), &temporary, true)?;
                fs::remove_dir_all(&temporary)?;
                sync_directory(&revisions)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn repair_durability(
        &self,
        app_id: Uuid,
        secret_revision: Option<Uuid>,
    ) -> Result<(), StoreError> {
        let app = self.apps.app_directory(app_id);
        check_private_tree(self.apps.apps_directory(), &app, true)?;
        let revisions = app.join("webhook-secret-revisions");
        if let Some(revision) = secret_revision {
            let revision = revisions.join(revision.to_string());
            check_private_tree(self.apps.apps_directory(), &revision, true)?;
            sync_directory(&revision)?;
        }
        match fs::symlink_metadata(&revisions) {
            Ok(_) => {
                check_private_tree(self.apps.apps_directory(), &revisions, true)?;
                sync_directory(&revisions)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        sync_directory(&app)
    }

    fn load_metadata(&self, app_id: Uuid) -> Result<WebhookMetadata, StoreError> {
        let path = self.apps.app_directory(app_id).join("webhook.toml");
        fs::symlink_metadata(&path)?;
        check_private_tree(self.apps.apps_directory(), &path, false)?;
        let value: WebhookMetadata =
            toml::from_str(&fs::read_to_string(path)?).map_err(|_| StoreError::ContentInvalid)?;
        if value.schema_version != 1 || value.app_id != app_id || value.integrity_hmac.len() != 64 {
            return Err(StoreError::ContentInvalid);
        }
        let expected = self.sign_metadata(&value);
        if !bool::from(expected.as_bytes().ct_eq(value.integrity_hmac.as_bytes())) {
            return Err(StoreError::ContentInvalid);
        }
        if value.enabled != value.secret_revision.is_some() {
            return Err(StoreError::ContentInvalid);
        }
        Ok(value)
    }

    fn load_revision(
        &self,
        app_id: Uuid,
        revision_id: Uuid,
        metadata: Option<&WebhookMetadata>,
    ) -> Result<SecretValue, StoreError> {
        self.load_revision_record(app_id, revision_id, metadata)
            .map(|(_, secret)| secret)
    }

    fn load_revision_record(
        &self,
        app_id: Uuid,
        revision_id: Uuid,
        metadata: Option<&WebhookMetadata>,
    ) -> Result<(SecretRevision, SecretValue), StoreError> {
        let directory = self
            .apps
            .app_directory(app_id)
            .join("webhook-secret-revisions")
            .join(revision_id.to_string());
        check_private_tree(self.apps.apps_directory(), &directory, true)?;
        validate_revision_directory_entries(self.apps.apps_directory(), &directory, false)?;
        let revision_path = directory.join("revision.toml");
        check_private_tree(self.apps.apps_directory(), &revision_path, false)?;
        let revision: SecretRevision = toml::from_str(&fs::read_to_string(revision_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        if revision.schema_version != 1
            || revision.app_id != app_id
            || revision.secret_revision != revision_id
            || self.sign_revision(revision.clone()).integrity_hmac != revision.integrity_hmac
        {
            return Err(StoreError::ContentInvalid);
        }
        if metadata.is_some_and(|v| v.secret_revision != Some(revision_id)) {
            return Err(StoreError::ContentInvalid);
        }
        let path = directory.join("secret");
        check_private_tree(self.apps.apps_directory(), &path, false)?;
        let raw = Zeroizing::new(fs::read(path)?);
        if raw.len() != 32
            || !bool::from(
                sign(&self.key, b"solodock/webhook-secret-value/v1\0", &raw)
                    .as_bytes()
                    .ct_eq(revision.secret_hmac.as_bytes()),
            )
        {
            return Err(StoreError::ContentInvalid);
        }
        let value = SecretValue::new(URL_SAFE_NO_PAD.encode(&raw));
        Ok((revision, value))
    }

    fn sign_metadata(&self, value: &WebhookMetadata) -> String {
        let mut canonical = value.clone();
        canonical.integrity_hmac.clear();
        sign(
            &self.key,
            b"solodock/webhook-metadata/v1\0",
            toml::to_string(&canonical)
                .expect("webhook metadata serializes")
                .as_bytes(),
        )
    }

    fn sign_revision(&self, mut value: SecretRevision) -> SecretRevision {
        value.integrity_hmac.clear();
        value.integrity_hmac = sign(
            &self.key,
            b"solodock/webhook-secret-revision/v1\0",
            toml::to_string(&value)
                .expect("webhook revision serializes")
                .as_bytes(),
        );
        value
    }
}

/// Validates every persisted webhook artifact without mutating it. Recovery
/// uses this for both the live state tree and relocated restore staging.
pub fn validate_app_directory(
    apps_root: &Path,
    app_directory: &Path,
    app_id: Uuid,
    key: &[u8],
) -> Result<bool, StoreError> {
    let metadata_path = app_directory.join("webhook.toml");
    let revisions = app_directory.join("webhook-secret-revisions");
    let metadata_exists = match fs::symlink_metadata(&metadata_path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let revisions_exist = match fs::symlink_metadata(&revisions) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !metadata_exists && !revisions_exist {
        return Ok(false);
    }
    if !metadata_exists || !revisions_exist {
        return Err(StoreError::ContentInvalid);
    }
    check_private_tree(apps_root, &metadata_path, false)?;
    check_private_tree(apps_root, &revisions, true)?;
    let metadata: WebhookMetadata = toml::from_str(&fs::read_to_string(&metadata_path)?)
        .map_err(|_| StoreError::ContentInvalid)?;
    if metadata.schema_version != 1
        || metadata.app_id != app_id
        || metadata.enabled != metadata.secret_revision.is_some()
    {
        return Err(StoreError::ContentInvalid);
    }
    let mut unsigned = metadata.clone();
    unsigned.integrity_hmac.clear();
    let expected = sign(
        key,
        b"solodock/webhook-metadata/v1\0",
        toml::to_string(&unsigned)
            .map_err(|_| StoreError::ContentInvalid)?
            .as_bytes(),
    );
    if !bool::from(
        expected
            .as_bytes()
            .ct_eq(metadata.integrity_hmac.as_bytes()),
    ) {
        return Err(StoreError::ContentInvalid);
    }
    let mut found_current = !metadata.enabled;
    for entry in fs::read_dir(&revisions)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(StoreError::SymlinkBoundary);
        }
        let text = entry
            .file_name()
            .to_str()
            .ok_or(StoreError::ContentInvalid)?
            .to_owned();
        if let Some(simple) = text.strip_prefix(".solodock-webhook-tmp-") {
            let operation = Uuid::parse_str(simple).map_err(|_| StoreError::ContentInvalid)?;
            if simple != operation.simple().to_string() {
                return Err(StoreError::ContentInvalid);
            }
            check_private_tree(apps_root, &entry.path(), true)?;
            validate_revision_directory_entries(apps_root, &entry.path(), true)?;
            continue;
        }
        let revision_id = text
            .parse::<Uuid>()
            .map_err(|_| StoreError::ContentInvalid)?;
        if text != revision_id.to_string() {
            return Err(StoreError::ContentInvalid);
        }
        check_private_tree(apps_root, &entry.path(), true)?;
        let revision_path = entry.path().join("revision.toml");
        let secret_path = entry.path().join("secret");
        validate_revision_directory_entries(apps_root, &entry.path(), false)?;
        check_private_tree(apps_root, &revision_path, false)?;
        check_private_tree(apps_root, &secret_path, false)?;
        let revision: SecretRevision = toml::from_str(&fs::read_to_string(revision_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        let mut unsigned_revision = revision.clone();
        unsigned_revision.integrity_hmac.clear();
        let expected_revision = sign(
            key,
            b"solodock/webhook-secret-revision/v1\0",
            toml::to_string(&unsigned_revision)
                .map_err(|_| StoreError::ContentInvalid)?
                .as_bytes(),
        );
        let raw = Zeroizing::new(fs::read(secret_path)?);
        let expected_secret = sign(key, b"solodock/webhook-secret-value/v1\0", &raw);
        if revision.schema_version != 1
            || revision.app_id != app_id
            || revision.secret_revision != revision_id
            || revision.integrity_hmac != expected_revision
            || raw.len() != 32
            || !bool::from(
                expected_secret
                    .as_bytes()
                    .ct_eq(revision.secret_hmac.as_bytes()),
            )
        {
            return Err(StoreError::ContentInvalid);
        }
        found_current |= metadata.secret_revision == Some(revision_id);
    }
    if !found_current {
        return Err(StoreError::ContentInvalid);
    }
    Ok(metadata.enabled)
}

fn validate_revision_directory_entries(
    apps_root: &Path,
    directory: &Path,
    partial: bool,
) -> Result<(), StoreError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(StoreError::SymlinkBoundary);
        }
        check_private_tree(apps_root, &entry.path(), false)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| StoreError::ContentInvalid)?;
        if !matches!(name.as_str(), "secret" | "revision.toml") {
            return Err(StoreError::ContentInvalid);
        }
        names.push(name);
    }
    names.sort_unstable();
    names.dedup();
    if !partial && names != ["revision.toml", "secret"] {
        return Err(StoreError::ContentInvalid);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SecretRevision {
    schema_version: u32,
    app_id: Uuid,
    secret_revision: Uuid,
    operation_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    secret_hmac: String,
    integrity_hmac: String,
}

fn sign(key: &[u8], domain: &[u8], data: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key accepted");
    mac.update(domain);
    mac.update(data);
    format!("{:x}", mac.finalize().into_bytes())
}

fn create_dir(path: &Path) -> Result<(), StoreError> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AppMetadata, DesiredState, DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy,
        normalize_draft,
    };
    use std::os::unix::fs::PermissionsExt;

    fn create_managed_app(app_store: &AppStore, key: &[u8], slug: &str) -> Uuid {
        let draft = normalize_draft(
            DraftInput {
                display_name: slug.into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![],
                health: HealthPolicy::default(),
            },
            &ExistingSecrets::default(),
            key,
            &[],
        )
        .unwrap();
        let app = Uuid::new_v4();
        app_store
            .create_app(
                app,
                slug,
                Uuid::new_v4(),
                Some((Uuid::new_v4(), &draft)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        app
    }

    fn fixture() -> (tempfile::TempDir, WebhookStore, Uuid) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let apps = root.path().join("apps");
        fs::create_dir(&apps).unwrap();
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let app = apps.join(app_id.to_string());
        fs::create_dir(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(app.join("releases")).unwrap();
        fs::set_permissions(app.join("releases"), fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = AppMetadata {
            schema_version: 2,
            id: app_id,
            slug: "example".into(),
            display_name: "Example".into(),
            resource_name_schema_version: crate::domain::RESOURCE_NAME_SCHEMA_LEGACY,
            discovery_image_ref: Some("registry.example/app:stable".into()),
            credential_ref: None,
            draft_revision: Some(Uuid::new_v4()),
            draft_config_sha256: Some("hash".into()),
            desired_state: DesiredState::Stopped,
            auto_deploy_enabled: true,
            poll_interval_seconds: 300,
            last_operation_id: Uuid::new_v4(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        write_new(
            &app.join("app.toml"),
            toml::to_string(&metadata).unwrap().as_bytes(),
        )
        .unwrap();
        let app_store = AppStore::initialize_verified(apps, vec![7; 32]).unwrap();
        (root, WebhookStore::new(app_store, vec![7; 32]), app_id)
    }

    #[test]
    fn configure_rotate_revoke_is_write_only_and_integrity_checked() {
        let (_root, store, app) = fixture();
        let first = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        let created = store.configure(app, None, Uuid::new_v4(), &first).unwrap();
        let raw = fs::read_to_string(store.apps.app_directory(app).join("webhook.toml")).unwrap();
        let parsed: WebhookMetadata = toml::from_str(&raw).unwrap();
        assert_eq!(store.sign_metadata(&parsed), parsed.integrity_hmac);
        assert!(store.status(app).unwrap().configured);
        let second = SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into());
        let rotated = store
            .configure(
                app,
                Some(created.metadata_revision),
                Uuid::new_v4(),
                &second,
            )
            .unwrap();
        assert_eq!(
            store.load_current(app).unwrap().secret.expose(),
            second.expose()
        );
        assert_eq!(
            store.all_secret_bytes().unwrap().len(),
            4,
            "old secret remains redacted until durable cleanup"
        );
        store.cleanup_unreferenced(app).unwrap();
        assert_eq!(store.all_secret_bytes().unwrap().len(), 2);
        let revoked = store
            .revoke(app, rotated.metadata_revision, Uuid::new_v4())
            .unwrap();
        assert!(!revoked.enabled);
        assert!(store.load_current(app).is_err());
    }

    #[test]
    fn unconfigured_app_cannot_create_or_load_webhook_artifacts() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let apps = root.path().join("apps");
        let app_store = AppStore::initialize_verified(apps, vec![7; 32]).unwrap();
        let app_id = Uuid::new_v4();
        app_store
            .create_app(
                app_id,
                "empty",
                Uuid::new_v4(),
                None,
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let store = WebhookStore::new(app_store.clone(), vec![7; 32]);
        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());

        assert!(matches!(
            store.configure(app_id, None, Uuid::new_v4(), &secret),
            Err(StoreError::ContentInvalid)
        ));
        assert!(matches!(
            store.load_current(app_id),
            Err(StoreError::ContentInvalid)
        ));
        assert!(
            !app_store
                .app_directory(app_id)
                .join("webhook.toml")
                .exists()
        );
        assert!(
            !app_store
                .app_directory(app_id)
                .join("webhook-secret-revisions")
                .exists()
        );
    }

    #[test]
    fn operation_revision_resume_and_tamper_fail_closed() {
        let (root, store, app) = fixture();
        let operation = Uuid::new_v4();
        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        let created = store.configure(app, None, operation, &secret).unwrap();
        assert_eq!(
            store.configure(app, None, operation, &secret).unwrap(),
            created
        );
        let secret_path = root
            .path()
            .join("apps")
            .join(app.to_string())
            .join("webhook-secret-revisions")
            .join(operation.to_string())
            .join("secret");
        fs::write(secret_path, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert!(store.load_current(app).is_err());
        let status = store.status(app).unwrap();
        assert!(status.configured);
        assert!(status.degraded);
    }

    #[test]
    fn missing_current_revision_is_manageable_but_cold_inventory_fails_closed() {
        let (_root, store, app) = fixture();
        let operation = Uuid::new_v4();
        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        let metadata = store.configure(app, None, operation, &secret).unwrap();
        fs::remove_dir_all(
            store
                .apps
                .app_directory(app)
                .join("webhook-secret-revisions")
                .join(operation.to_string()),
        )
        .unwrap();

        let status = store.status(app).unwrap();
        assert!(status.configured);
        assert!(status.degraded);
        assert_eq!(status.metadata_revision, Some(metadata.metadata_revision));
        assert_eq!(status.secret_revision, Some(operation));
        assert!(matches!(
            store.all_secret_bytes(),
            Err(StoreError::ContentInvalid)
        ));
    }

    #[test]
    fn operation_owned_partial_temp_is_rebuilt_on_same_operation_resume() {
        let (_root, store, app) = fixture();
        let operation = Uuid::new_v4();
        let revisions = store
            .apps
            .app_directory(app)
            .join("webhook-secret-revisions");
        fs::create_dir(&revisions).unwrap();
        fs::set_permissions(&revisions, fs::Permissions::from_mode(0o700)).unwrap();
        let temporary = revisions.join(format!(".solodock-webhook-tmp-{}", operation.simple()));
        fs::create_dir(&temporary).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).unwrap();
        write_new(&temporary.join("secret"), b"partial-secret").unwrap();

        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        let metadata = store.configure(app, None, operation, &secret).unwrap();
        assert_eq!(metadata.secret_revision, Some(operation));
        assert!(!temporary.exists());
        assert!(
            store
                .load_current(app)
                .unwrap()
                .secret
                .constant_time_eq(secret.expose())
        );
    }

    #[test]
    fn cleanup_never_treats_corrupt_metadata_as_no_current_revision() {
        let (_root, store, app) = fixture();
        let operation = Uuid::new_v4();
        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        store.configure(app, None, operation, &secret).unwrap();
        fs::write(
            store.apps.app_directory(app).join("webhook.toml"),
            "corrupt",
        )
        .unwrap();
        assert!(store.cleanup_unreferenced(app).is_err());
        assert!(
            store
                .apps
                .app_directory(app)
                .join("webhook-secret-revisions")
                .join(operation.to_string())
                .exists()
        );
    }

    #[test]
    fn corrupt_webhook_substate_degrades_without_hiding_the_app() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = vec![7; 32];
        let apps = root.path().join("apps");
        let app_store = AppStore::initialize_managed(apps, key.clone(), vec![]).unwrap();
        let app = create_managed_app(&app_store, &key, "example");
        let webhook_store = WebhookStore::new(app_store.clone(), key);
        webhook_store
            .configure(
                app,
                None,
                Uuid::new_v4(),
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap();
        fs::write(app_store.app_directory(app).join("webhook.toml"), "corrupt").unwrap();

        let report = app_store.scan_read_only().unwrap();
        assert_eq!(report.valid_apps.len(), 1);
        assert_eq!(report.valid_apps[0].app_id, app);
        assert!(
            report.issues.iter().any(|issue| {
                issue.app_id == Some(app) && issue.code == "WEBHOOK_CONFIG_INVALID"
            })
        );
        assert!(webhook_store.load_current(app).is_err());
        assert!(webhook_store.status(app).unwrap().degraded);
        let repaired = SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into());
        let metadata = webhook_store
            .configure(app, None, Uuid::new_v4(), &repaired)
            .unwrap();
        assert!(metadata.enabled);
        assert!(
            webhook_store
                .load_current(app)
                .unwrap()
                .secret
                .constant_time_eq(repaired.expose())
        );

        let metadata_path = app_store.app_directory(app).join("webhook.toml");
        fs::set_permissions(&metadata_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            app_store.scan_read_only(),
            Err(StoreError::Permission(
                crate::security::permissions::PermissionError::Mode(_)
            ))
        ));
        fs::set_permissions(&metadata_path, fs::Permissions::from_mode(0o600)).unwrap();

        let revision = metadata.secret_revision.unwrap();
        let revision_directory = app_store
            .app_directory(app)
            .join("webhook-secret-revisions")
            .join(revision.to_string());
        fs::set_permissions(&revision_directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            app_store.scan_read_only(),
            Err(StoreError::Permission(
                crate::security::permissions::PermissionError::Mode(_)
            ))
        ));
        fs::set_permissions(&revision_directory, fs::Permissions::from_mode(0o700)).unwrap();

        let secret_path = revision_directory.join("secret");
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            app_store.scan_read_only(),
            Err(StoreError::Permission(
                crate::security::permissions::PermissionError::Mode(_)
            ))
        ));
    }

    #[test]
    fn incomplete_cold_inventory_fails_instead_of_dropping_other_app_secrets() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = vec![7; 32];
        let apps = root.path().join("apps");
        let app_store = AppStore::initialize_managed(apps, key.clone(), vec![]).unwrap();
        let damaged_app = create_managed_app(&app_store, &key, "damaged");
        let healthy_app = create_managed_app(&app_store, &key, "healthy");
        let webhook_store = WebhookStore::new(app_store.clone(), key);
        let damaged_revision = webhook_store
            .configure(
                damaged_app,
                None,
                Uuid::new_v4(),
                &SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into()),
            )
            .unwrap()
            .secret_revision
            .unwrap();
        webhook_store
            .configure(
                healthy_app,
                None,
                Uuid::new_v4(),
                &SecretValue::new("Hx4dHBsaGRgXFhUUExIREA8ODQwLCgkIBwYFBAMCAQA".into()),
            )
            .unwrap();
        fs::write(
            app_store
                .app_directory(damaged_app)
                .join("webhook-secret-revisions")
                .join(damaged_revision.to_string())
                .join("secret"),
            [0_u8; 32],
        )
        .unwrap();

        assert!(matches!(
            webhook_store.all_secret_bytes(),
            Err(StoreError::ContentInvalid)
        ));
    }
}
