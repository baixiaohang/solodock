pub mod atomic;
pub mod cleanup;
pub mod config_revision;
pub mod recovery;
pub mod releases;

use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::{
    APP_METADATA_SCHEMA_VERSION, AppMetadata, DesiredState, NormalizedDraft,
    RESOURCE_NAME_SCHEMA_CURRENT, RESOURCE_NAME_SCHEMA_LEGACY, validate_slug_for_resource_schema,
};
use crate::security::permissions::{PermissionError, check_private, ensure_private_directory};

#[derive(Clone)]
pub struct AppStore {
    apps_directory: PathBuf,
    canonical_apps_directory: PathBuf,
    integrity_key: Option<Arc<Vec<u8>>>,
    allowed_bind_roots: Arc<RwLock<Vec<PathBuf>>>,
    #[cfg(any(test, feature = "docker-e2e"))]
    cleanup_fault: Arc<std::sync::Mutex<Option<cleanup::CleanupFault>>>,
    #[cfg(test)]
    cleanup_fail_next_rename: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    cleanup_fail_next_finalize: Arc<std::sync::atomic::AtomicBool>,
}

impl AppStore {
    pub fn initialize(apps_directory: PathBuf) -> Result<Self, StoreError> {
        ensure_private_directory(&apps_directory)?;
        Ok(Self {
            canonical_apps_directory: apps_directory.clone(),
            apps_directory,
            integrity_key: None,
            allowed_bind_roots: Arc::new(RwLock::new(Vec::new())),
            #[cfg(any(test, feature = "docker-e2e"))]
            cleanup_fault: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            cleanup_fail_next_rename: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            cleanup_fail_next_finalize: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn initialize_verified(
        apps_directory: PathBuf,
        integrity_key: Vec<u8>,
    ) -> Result<Self, StoreError> {
        let mut store = Self::initialize(apps_directory)?;
        store.integrity_key = Some(Arc::new(integrity_key));
        Ok(store)
    }

    pub fn initialize_managed(
        apps_directory: PathBuf,
        integrity_key: Vec<u8>,
        allowed_bind_roots: Vec<PathBuf>,
    ) -> Result<Self, StoreError> {
        let mut store = Self::initialize_verified(apps_directory, integrity_key)?;
        store.allowed_bind_roots = Arc::new(RwLock::new(allowed_bind_roots));
        Ok(store)
    }

    /// Initialize a store whose managed files live below a staging root while
    /// path-sensitive Compose artifacts retain their installed path identity.
    /// This is used by offline package/restore validation; runtime callers
    /// should use [`Self::initialize_managed`].
    pub fn initialize_managed_relocated(
        apps_directory: PathBuf,
        canonical_apps_directory: PathBuf,
        integrity_key: Vec<u8>,
        allowed_bind_roots: Vec<PathBuf>,
    ) -> Result<Self, StoreError> {
        if !canonical_apps_directory.is_absolute() {
            return Err(StoreError::ContentInvalid);
        }
        let mut store =
            Self::initialize_managed(apps_directory, integrity_key, allowed_bind_roots)?;
        store.canonical_apps_directory = canonical_apps_directory;
        Ok(store)
    }

    pub fn scan(&self) -> Result<recovery::RecoveryReport, StoreError> {
        config_revision::normalize_managed_file_permissions(&self.apps_directory)?;
        let allowed_bind_roots = self.allowed_bind_roots();
        recovery::scan_relocated_with_options(
            &self.apps_directory,
            &self.canonical_apps_directory,
            self.integrity_key.as_deref().map(Vec::as_slice),
            &allowed_bind_roots,
        )
    }

    /// Validate and project the current filesystem facts without performing
    /// startup-only cleanup. Runtime readers must never remove a writer's
    /// temporary or newly published-but-not-yet-referenced artifacts.
    pub fn scan_read_only(&self) -> Result<recovery::RecoveryReport, StoreError> {
        let allowed_bind_roots = self.allowed_bind_roots();
        recovery::scan_read_only_relocated(
            &self.apps_directory,
            &self.canonical_apps_directory,
            self.integrity_key.as_deref().map(Vec::as_slice),
            &allowed_bind_roots,
        )
    }

    pub fn apps_directory(&self) -> &std::path::Path {
        &self.apps_directory
    }

    pub fn integrity_key(&self) -> Result<&[u8], StoreError> {
        self.integrity_key
            .as_deref()
            .map(Vec::as_slice)
            .ok_or(StoreError::ContentInvalid)
    }

    pub fn allowed_bind_roots(&self) -> Vec<PathBuf> {
        self.allowed_bind_roots
            .read()
            .expect("bind root lock poisoned")
            .clone()
    }

    pub fn replace_allowed_bind_roots(&self, roots: Vec<PathBuf>) {
        *self
            .allowed_bind_roots
            .write()
            .expect("bind root lock poisoned") = roots;
    }

    pub fn app_directory(&self, app_id: Uuid) -> PathBuf {
        self.apps_directory.join(app_id.to_string())
    }

    pub(crate) fn canonical_app_directory(&self, app_id: Uuid) -> PathBuf {
        self.canonical_apps_directory.join(app_id.to_string())
    }

    pub fn create_app(
        &self,
        app_id: Uuid,
        slug: &str,
        operation_id: Uuid,
        initial_draft: Option<(Uuid, &NormalizedDraft)>,
        now: OffsetDateTime,
    ) -> Result<AppMetadata, StoreError> {
        let temporary = self
            .apps_directory
            .join(format!(".solodock-tmp-app-{}", operation_id.simple()));
        let target = self.app_directory(app_id);
        let result = (|| {
            fs::DirBuilder::new().mode(0o700).create(&temporary)?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(temporary.join("releases"))?;
            if let Some((revision_id, draft)) = initial_draft {
                config_revision::publish(&temporary, revision_id, draft)?;
            }
            let (
                display_name,
                discovery_image_ref,
                credential_ref,
                draft_revision,
                draft_hash,
                auto_deploy_enabled,
                poll_interval_seconds,
            ) = match initial_draft {
                Some((revision_id, draft)) => (
                    draft.display_name.clone(),
                    Some(draft.discovery_image_ref.clone()),
                    draft.credential_ref,
                    Some(revision_id),
                    Some(draft.metadata.config_sha256.clone()),
                    draft.auto_deploy_enabled,
                    draft.poll_interval_seconds,
                ),
                None => (
                    slug.to_owned(),
                    None,
                    None,
                    None,
                    None,
                    false,
                    crate::domain::default_poll_interval(),
                ),
            };
            let metadata = AppMetadata {
                schema_version: APP_METADATA_SCHEMA_VERSION,
                id: app_id,
                slug: slug.to_owned(),
                display_name,
                resource_name_schema_version: RESOURCE_NAME_SCHEMA_CURRENT,
                discovery_image_ref,
                credential_ref,
                draft_revision,
                draft_config_sha256: draft_hash,
                desired_state: DesiredState::Stopped,
                auto_deploy_enabled,
                poll_interval_seconds,
                last_operation_id: operation_id,
                created_at: now,
                updated_at: now,
            };
            write_metadata(&temporary, &metadata)?;
            sync_directory(&temporary)?;
            atomic::rename_no_replace(&temporary, &target).map_err(|error| match error {
                StoreError::ReleaseConflict => StoreError::AppConflict,
                other => other,
            })?;
            sync_directory(&self.apps_directory)?;
            Ok(metadata)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    pub fn update_draft(
        &self,
        app_id: Uuid,
        expected_revision: Option<Uuid>,
        revision_id: Uuid,
        operation_id: Uuid,
        draft: &NormalizedDraft,
        now: OffsetDateTime,
    ) -> Result<AppMetadata, StoreError> {
        let directory = self.app_directory(app_id);
        let mut metadata = read_metadata(&directory)?;
        if metadata.id != app_id {
            return Err(StoreError::ContentInvalid);
        }
        if metadata.draft_revision != expected_revision {
            return Err(StoreError::RevisionStale);
        }
        match config_revision::publish(&directory, revision_id, draft) {
            Ok(_) => {}
            Err(StoreError::ConfigRevisionConflict) => {
                let key = self
                    .integrity_key
                    .as_deref()
                    .ok_or(StoreError::ContentInvalid)?;
                let existing = config_revision::load_verified(&directory, revision_id, key)?;
                if existing.metadata != draft.metadata {
                    return Err(StoreError::ConfigRevisionConflict);
                }
            }
            Err(error) => return Err(error),
        }
        metadata.display_name = draft.display_name.clone();
        metadata.discovery_image_ref = Some(draft.discovery_image_ref.clone());
        metadata.credential_ref = draft.credential_ref;
        metadata.auto_deploy_enabled = draft.auto_deploy_enabled;
        metadata.draft_revision = Some(revision_id);
        metadata.draft_config_sha256 = Some(draft.metadata.config_sha256.clone());
        metadata.poll_interval_seconds = draft.poll_interval_seconds;
        metadata.last_operation_id = operation_id;
        metadata.updated_at = now;
        write_metadata(&directory, &metadata)?;
        Ok(metadata)
    }

    pub fn read_metadata(&self, app_id: Uuid) -> Result<AppMetadata, StoreError> {
        read_metadata(&self.app_directory(app_id))
    }

    /// Re-issue directory fsyncs after an ambiguous post-rename error. The
    /// visible metadata/revision marker is checked by the caller before this
    /// method is used, making the operation safe and idempotent.
    pub fn repair_app_durability(&self, app_id: Uuid) -> Result<(), StoreError> {
        let app = self.app_directory(app_id);
        check_private(&app, true)?;
        let revisions = app.join("config-revisions");
        match fs::symlink_metadata(&revisions) {
            Ok(_) => {
                check_private(&revisions, true)?;
                sync_directory(&revisions)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        sync_directory(&app)?;
        sync_directory(&self.apps_directory)
    }

    pub fn set_desired_state(
        &self,
        app_id: Uuid,
        desired_state: DesiredState,
        operation_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<AppMetadata, StoreError> {
        let directory = self.app_directory(app_id);
        let mut metadata = read_metadata(&directory)?;
        metadata.desired_state = desired_state;
        metadata.last_operation_id = operation_id;
        metadata.updated_at = now;
        write_metadata(&directory, &metadata)?;
        Ok(metadata)
    }

    pub fn tombstone(&self, app_id: Uuid, operation_id: Uuid) -> Result<PathBuf, StoreError> {
        self.tombstone_with_after_rename(app_id, operation_id, || Ok(()))
    }

    fn tombstone_with_after_rename(
        &self,
        app_id: Uuid,
        operation_id: Uuid,
        after_rename: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<PathBuf, StoreError> {
        let source = self.app_directory(app_id);
        let mut metadata = read_metadata(&source)?;
        if metadata.id != app_id {
            return Err(StoreError::ContentInvalid);
        }
        metadata.last_operation_id = operation_id;
        metadata.updated_at = OffsetDateTime::now_utc();
        write_metadata(&source, &metadata)?;
        let marker =
            format!("schema_version=1\napp_id='{app_id}'\noperation_id='{operation_id}'\n");
        atomic::AtomicWriter::write(&source.join("deletion.toml"), marker.as_bytes(), 0o600)?;
        let trash = self.apps_directory.join(".trash");
        ensure_private_directory(&trash)?;
        let target = trash.join(format!("{app_id}-{operation_id}"));
        atomic::rename_no_replace(&source, &target).map_err(|error| match error {
            StoreError::ReleaseConflict => StoreError::AppConflict,
            other => other,
        })?;
        after_rename()?;
        sync_directory(&trash)?;
        sync_directory(&self.apps_directory)?;
        Ok(target)
    }

    pub fn tombstone_path(&self, app_id: Uuid, operation_id: Uuid) -> PathBuf {
        self.apps_directory
            .join(".trash")
            .join(format!("{app_id}-{operation_id}"))
    }

    pub fn read_tombstone_metadata(
        &self,
        app_id: Uuid,
        operation_id: Uuid,
    ) -> Result<AppMetadata, StoreError> {
        let path = self.tombstone_path(app_id, operation_id);
        crate::security::permissions::check_private(&path, true)?;
        let marker_path = path.join("deletion.toml");
        crate::security::permissions::check_private(&marker_path, false)?;
        let marker: DeletionMarker = toml::from_str(&fs::read_to_string(marker_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        if marker.schema_version != 1
            || marker.app_id != app_id
            || marker.operation_id != operation_id
        {
            return Err(StoreError::ContentInvalid);
        }
        let metadata = read_metadata(&path)?;
        if metadata.id != app_id || metadata.last_operation_id != operation_id {
            return Err(StoreError::ContentInvalid);
        }
        Ok(metadata)
    }

    pub fn finalize_tombstone(&self, app_id: Uuid, operation_id: Uuid) -> Result<(), StoreError> {
        let path = self.tombstone_path(app_id, operation_id);
        self.read_tombstone_metadata(app_id, operation_id)?;
        fs::remove_dir_all(path)?;
        sync_directory(&self.apps_directory.join(".trash"))?;
        Ok(())
    }

    /// Enumerates only canonical, internally consistent application tombstones.
    /// Unknown entries fail closed so recovery never broad-deletes state.
    pub fn tombstones(&self) -> Result<Vec<(Uuid, Uuid)>, StoreError> {
        let trash = self.apps_directory.join(".trash");
        let entries = match fs::read_dir(&trash) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::ContentInvalid)?;
            if name.len() != 73 || name.as_bytes().get(36) != Some(&b'-') {
                return Err(StoreError::ContentInvalid);
            }
            let app_id = name[..36]
                .parse::<Uuid>()
                .map_err(|_| StoreError::ContentInvalid)?;
            let operation_id = name[37..]
                .parse::<Uuid>()
                .map_err(|_| StoreError::ContentInvalid)?;
            if name != format!("{app_id}-{operation_id}") {
                return Err(StoreError::ContentInvalid);
            }
            self.read_tombstone_metadata(app_id, operation_id)?;
            result.push((app_id, operation_id));
        }
        result.sort_unstable();
        Ok(result)
    }
}

#[derive(serde::Deserialize)]
struct DeletionMarker {
    schema_version: u32,
    app_id: Uuid,
    operation_id: Uuid,
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
    #[error("config revision already exists")]
    ConfigRevisionConflict,
    #[error("application already exists")]
    AppConflict,
    #[error("application revision is stale")]
    RevisionStale,
    #[error("managed store content is invalid")]
    ContentInvalid,
    #[error("managed file permission is invalid")]
    ManagedFilePermissionInvalid,
}

fn read_metadata(directory: &Path) -> Result<AppMetadata, StoreError> {
    crate::security::permissions::check_private(directory, true)?;
    let path = directory.join("app.toml");
    crate::security::permissions::check_private(&path, false)?;
    let contents = fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            StoreError::ContentInvalid
        } else {
            error.into()
        }
    })?;
    let metadata: AppMetadata =
        toml::from_str(&contents).map_err(|_| StoreError::ContentInvalid)?;
    if !valid_metadata_identity_versions(
        metadata.schema_version,
        metadata.resource_name_schema_version,
    ) || validate_slug_for_resource_schema(&metadata.slug, metadata.resource_name_schema_version)
        .is_err()
        || metadata.draft_revision.is_some() != metadata.draft_config_sha256.is_some()
        || (metadata.draft_revision.is_none()
            && (metadata.discovery_image_ref.is_some()
                || metadata.credential_ref.is_some()
                || metadata.desired_state != DesiredState::Stopped
                || metadata.auto_deploy_enabled))
    {
        return Err(StoreError::ContentInvalid);
    }
    Ok(metadata)
}

pub(crate) const fn valid_metadata_identity_versions(
    metadata_schema_version: u32,
    resource_name_schema_version: u32,
) -> bool {
    matches!(
        (metadata_schema_version, resource_name_schema_version),
        (2, RESOURCE_NAME_SCHEMA_LEGACY)
            | (
                APP_METADATA_SCHEMA_VERSION,
                RESOURCE_NAME_SCHEMA_LEGACY | RESOURCE_NAME_SCHEMA_CURRENT
            )
    )
}

fn write_metadata(directory: &Path, metadata: &AppMetadata) -> Result<(), StoreError> {
    let contents = toml::to_string(metadata).map_err(|_| StoreError::ContentInvalid)?;
    atomic::AtomicWriter::write(&directory.join("app.toml"), contents.as_bytes(), 0o600)
}

pub(crate) fn sync_directory(path: &std::path::Path) -> Result<(), StoreError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn metadata_and_resource_identity_version_matrix_is_single_and_legacy_stable() {
        for (metadata, naming, expected) in [
            (2, RESOURCE_NAME_SCHEMA_LEGACY, true),
            (2, RESOURCE_NAME_SCHEMA_CURRENT, false),
            (
                APP_METADATA_SCHEMA_VERSION,
                RESOURCE_NAME_SCHEMA_LEGACY,
                true,
            ),
            (
                APP_METADATA_SCHEMA_VERSION,
                RESOURCE_NAME_SCHEMA_CURRENT,
                true,
            ),
            (1, RESOURCE_NAME_SCHEMA_LEGACY, false),
            (APP_METADATA_SCHEMA_VERSION, 99, false),
        ] {
            assert_eq!(
                valid_metadata_identity_versions(metadata, naming),
                expected,
                "metadata={metadata}, naming={naming}"
            );
        }
        let app_id = Uuid::new_v4();
        let identity = crate::domain::AppResourceIdentity {
            app_id,
            slug: "legacy",
            schema_version: RESOURCE_NAME_SCHEMA_LEGACY,
        };
        assert_eq!(identity.resource_names().bridge_name, "sd-legacy");
    }

    #[test]
    fn tombstone_records_the_delete_operation_and_only_finalizes_that_entry() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = AppStore::initialize(root.path().join("apps")).unwrap();
        let app_id = Uuid::new_v4();
        let app = store.app_directory(app_id);
        fs::create_dir(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        let now = OffsetDateTime::now_utc();
        write_metadata(
            &app,
            &AppMetadata {
                schema_version: 2,
                id: app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                resource_name_schema_version: RESOURCE_NAME_SCHEMA_LEGACY,
                discovery_image_ref: Some("registry.example/app:stable".into()),
                credential_ref: None,
                draft_revision: Some(Uuid::new_v4()),
                draft_config_sha256: Some("hash".into()),
                desired_state: DesiredState::Stopped,
                auto_deploy_enabled: false,
                poll_interval_seconds: 300,
                last_operation_id: Uuid::new_v4(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
        let delete_operation = Uuid::new_v4();
        let path = store.tombstone(app_id, delete_operation).unwrap();
        assert_eq!(
            store
                .read_tombstone_metadata(app_id, delete_operation)
                .unwrap()
                .last_operation_id,
            delete_operation
        );
        store.finalize_tombstone(app_id, delete_operation).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn tombstone_rename_is_observable_when_parent_sync_fails() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = AppStore::initialize(root.path().join("apps")).unwrap();
        let app_id = Uuid::new_v4();
        let app = store.app_directory(app_id);
        fs::create_dir(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        let now = OffsetDateTime::now_utc();
        write_metadata(
            &app,
            &AppMetadata {
                schema_version: 2,
                id: app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                resource_name_schema_version: RESOURCE_NAME_SCHEMA_LEGACY,
                discovery_image_ref: Some("registry.example/app:stable".into()),
                credential_ref: None,
                draft_revision: Some(Uuid::new_v4()),
                draft_config_sha256: Some("hash".into()),
                desired_state: DesiredState::Stopped,
                auto_deploy_enabled: false,
                poll_interval_seconds: 300,
                last_operation_id: Uuid::new_v4(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
        let operation_id = Uuid::new_v4();
        let error = store
            .tombstone_with_after_rename(app_id, operation_id, || {
                Err(StoreError::Io(std::io::Error::other(
                    "injected parent sync failure",
                )))
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::Io(_)));
        assert!(!store.app_directory(app_id).exists());
        assert!(store.read_tombstone_metadata(app_id, operation_id).is_ok());
    }
}
