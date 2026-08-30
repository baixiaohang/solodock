pub mod atomic;
pub mod config_revision;
pub mod recovery;
pub mod releases;

use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::{AppMetadata, DesiredState, NormalizedDraft};
use crate::security::permissions::{PermissionError, check_private, ensure_private_directory};

#[derive(Clone)]
pub struct AppStore {
    apps_directory: PathBuf,
    integrity_key: Option<Arc<Vec<u8>>>,
    allowed_bind_roots: Arc<Vec<PathBuf>>,
}

impl AppStore {
    pub fn initialize(apps_directory: PathBuf) -> Result<Self, StoreError> {
        ensure_private_directory(&apps_directory)?;
        Ok(Self {
            apps_directory,
            integrity_key: None,
            allowed_bind_roots: Arc::default(),
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
        store.allowed_bind_roots = Arc::new(allowed_bind_roots);
        Ok(store)
    }

    pub fn scan(&self) -> Result<recovery::RecoveryReport, StoreError> {
        recovery::scan_with_options(
            &self.apps_directory,
            self.integrity_key.as_deref().map(Vec::as_slice),
            &self.allowed_bind_roots,
        )
    }

    /// Validate and project the current filesystem facts without performing
    /// startup-only cleanup. Runtime readers must never remove a writer's
    /// temporary or newly published-but-not-yet-referenced artifacts.
    pub fn scan_read_only(&self) -> Result<recovery::RecoveryReport, StoreError> {
        recovery::scan_read_only_with_options(
            &self.apps_directory,
            self.integrity_key.as_deref().map(Vec::as_slice),
            &self.allowed_bind_roots,
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

    pub fn allowed_bind_roots(&self) -> &[PathBuf] {
        &self.allowed_bind_roots
    }

    pub fn app_directory(&self, app_id: Uuid) -> PathBuf {
        self.apps_directory.join(app_id.to_string())
    }

    pub fn create_app(
        &self,
        app_id: Uuid,
        revision_id: Uuid,
        operation_id: Uuid,
        draft: &NormalizedDraft,
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
            config_revision::publish(&temporary, revision_id, draft)?;
            let metadata = AppMetadata {
                schema_version: 1,
                id: app_id,
                slug: draft.slug.clone(),
                display_name: draft.display_name.clone(),
                project_name: AppMetadata::project_name(app_id),
                discovery_image_ref: draft.discovery_image_ref.clone(),
                credential_ref: draft.credential_ref,
                draft_revision: revision_id,
                draft_config_sha256: draft.metadata.config_sha256.clone(),
                desired_state: DesiredState::Stopped,
                auto_deploy_enabled: draft.auto_deploy_enabled,
                poll_interval_seconds: draft.poll_interval_seconds,
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
        expected_revision: Uuid,
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
        metadata.slug = draft.slug.clone();
        metadata.display_name = draft.display_name.clone();
        metadata.discovery_image_ref = draft.discovery_image_ref.clone();
        metadata.credential_ref = draft.credential_ref;
        metadata.auto_deploy_enabled = draft.auto_deploy_enabled;
        metadata.draft_revision = revision_id;
        metadata.draft_config_sha256 = draft.metadata.config_sha256.clone();
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
    toml::from_str(&contents).map_err(|_| StoreError::ContentInvalid)
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
                schema_version: 1,
                id: app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                project_name: AppMetadata::project_name(app_id),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                draft_revision: Uuid::new_v4(),
                draft_config_sha256: "hash".into(),
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
                schema_version: 1,
                id: app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                project_name: AppMetadata::project_name(app_id),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                draft_revision: Uuid::new_v4(),
                draft_config_sha256: "hash".into(),
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
