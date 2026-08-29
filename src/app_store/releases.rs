use std::{
    fs,
    path::{Path, PathBuf},
};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{AppStore, StoreError, atomic::AtomicWriter, config_revision};
use crate::{
    compose::{ComposeInput, generate},
    domain::{AppMetadata, DesiredState, normalize_draft},
    registry::ResolvedImage,
    security::permissions::check_private_tree,
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseTrigger {
    Manual,
    Rollback,
    Poll,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseV2 {
    pub schema_version: u32,
    pub id: Uuid,
    pub app_id: Uuid,
    pub config_revision: Uuid,
    pub config_sha256: String,
    pub source_image_ref: String,
    pub logical_registry: String,
    pub repository: String,
    pub source_tag: String,
    pub source_descriptor_digest: String,
    pub index_digest: Option<String>,
    pub manifest_digest: String,
    pub runnable_image_ref: String,
    pub platform_os: String,
    pub platform_architecture: String,
    pub platform_variant: Option<String>,
    pub local_image_id: String,
    pub compose_sha256: String,
    pub credential_ref: Option<Uuid>,
    pub trigger: ReleaseTrigger,
    pub source_release_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub integrity_hmac: String,
}

impl AppStore {
    pub fn publish_v2_release(
        &self,
        app: &AppMetadata,
        release_id: Uuid,
        resolved: &ResolvedImage,
        trigger: ReleaseTrigger,
        source_release_id: Option<Uuid>,
    ) -> Result<ReleaseV2, StoreError> {
        let app_directory = self.app_directory(app.id);
        let loaded = config_revision::load_verified(
            &app_directory,
            app.draft_revision,
            self.integrity_key()?,
        )?;
        let input = loaded.input(
            app.slug.clone(),
            app.display_name.clone(),
            app.discovery_image_ref.clone(),
            app.credential_ref,
            app.auto_deploy_enabled,
            app.poll_interval_seconds,
        );
        let draft = normalize_draft(
            input,
            &loaded.secrets,
            self.integrity_key()?,
            self.allowed_bind_roots(),
        )
        .map_err(|_| StoreError::ContentInvalid)?;
        if draft.metadata.config_sha256 != app.draft_config_sha256 {
            return Err(StoreError::ContentInvalid);
        }
        let revision_directory = app_directory
            .join("config-revisions")
            .join(app.draft_revision.to_string());
        let (compose, _) = generate(
            ComposeInput {
                app_id: app.id,
                release_id,
                image_ref: &resolved.runnable_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
            },
            true,
        )
        .map_err(|_| StoreError::ContentInvalid)?;
        let compose_sha256 = format!("{:x}", Sha256::digest(compose.as_bytes()));
        let mut release = ReleaseV2 {
            schema_version: 2,
            id: release_id,
            app_id: app.id,
            config_revision: app.draft_revision,
            config_sha256: app.draft_config_sha256.clone(),
            source_image_ref: resolved.source_image_ref.clone(),
            logical_registry: resolved.logical_registry.clone(),
            repository: resolved.repository.clone(),
            source_tag: resolved.source_tag.clone(),
            source_descriptor_digest: resolved.source_descriptor_digest.clone(),
            index_digest: resolved.index_digest.clone(),
            manifest_digest: resolved.manifest_digest.clone(),
            runnable_image_ref: resolved.runnable_image_ref.clone(),
            platform_os: resolved.platform.os.clone(),
            platform_architecture: resolved.platform.architecture.clone(),
            platform_variant: resolved.platform.variant.clone(),
            local_image_id: resolved.local_image_id.clone(),
            compose_sha256,
            credential_ref: app.credential_ref,
            trigger,
            source_release_id,
            created_at: OffsetDateTime::now_utc(),
            integrity_hmac: String::new(),
        };
        release.integrity_hmac = sign(&release, self.integrity_key()?);
        let header = toml::to_string(&release).map_err(|_| StoreError::ContentInvalid)?;
        AtomicWriter::publish_release(
            &app_directory.join("releases"),
            release_id,
            header.as_bytes(),
            compose.as_bytes(),
        )?;
        Ok(release)
    }

    pub fn load_v2_release(&self, app_id: Uuid, release_id: Uuid) -> Result<ReleaseV2, StoreError> {
        let app = self.read_metadata(app_id)?;
        let directory = self
            .app_directory(app_id)
            .join("releases")
            .join(release_id.to_string());
        let header_path = directory.join("release.toml");
        let compose_path = directory.join("compose.yaml");
        check_private_tree(&self.app_directory(app_id), &header_path, false)?;
        check_private_tree(&self.app_directory(app_id), &compose_path, false)?;
        let release: ReleaseV2 = toml::from_str(&fs::read_to_string(header_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        if release.schema_version != 2
            || release.id != release_id
            || release.app_id != app_id
            || sign(&release, self.integrity_key()?) != release.integrity_hmac
        {
            return Err(StoreError::ContentInvalid);
        }
        crate::domain::validate_runnable_image(&release.runnable_image_ref)
            .map_err(|_| StoreError::ContentInvalid)?;
        let compose = fs::read(&compose_path)?;
        if format!("{:x}", Sha256::digest(&compose)) != release.compose_sha256 {
            return Err(StoreError::ContentInvalid);
        }
        let loaded = config_revision::load_verified(
            &self.app_directory(app_id),
            release.config_revision,
            self.integrity_key()?,
        )?;
        if loaded.metadata.config_sha256 != release.config_sha256 {
            return Err(StoreError::ContentInvalid);
        }
        let input = loaded.input(
            app.slug,
            app.display_name,
            release.source_image_ref.clone(),
            release.credential_ref,
            app.auto_deploy_enabled,
            app.poll_interval_seconds,
        );
        let draft = normalize_draft(
            input,
            &loaded.secrets,
            self.integrity_key()?,
            self.allowed_bind_roots(),
        )
        .map_err(|_| StoreError::ContentInvalid)?;
        let revision_directory = self
            .app_directory(app_id)
            .join("config-revisions")
            .join(release.config_revision.to_string());
        let (canonical, _) = generate(
            ComposeInput {
                app_id,
                release_id,
                image_ref: &release.runnable_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
            },
            true,
        )
        .map_err(|_| StoreError::ContentInvalid)?;
        if compose != canonical.as_bytes() {
            return Err(StoreError::ContentInvalid);
        }
        Ok(release)
    }

    pub fn release_compose_path(&self, app_id: Uuid, release_id: Uuid) -> PathBuf {
        self.app_directory(app_id)
            .join("releases")
            .join(release_id.to_string())
            .join("compose.yaml")
    }

    pub fn set_pending(&self, app_id: Uuid, release_id: Uuid) -> Result<(), StoreError> {
        self.load_v2_release(app_id, release_id)?;
        AtomicWriter::switch_release_link(&self.app_directory(app_id), "pending", release_id)
    }

    pub fn commit_active(
        &self,
        app_id: Uuid,
        expected_active: Option<Uuid>,
        candidate: Uuid,
    ) -> Result<(), StoreError> {
        if self.read_release_link(app_id, "active")? != expected_active
            || self.read_release_link(app_id, "pending")? != Some(candidate)
        {
            return Err(StoreError::RevisionStale);
        }
        self.load_v2_release(app_id, candidate)?;
        let app_directory = self.app_directory(app_id);
        if let Err(error) = AtomicWriter::switch_release_link(&app_directory, "active", candidate) {
            if self.read_release_link(app_id, "active")? != Some(candidate) {
                return Err(error);
            }
            super::sync_directory(&app_directory)?;
        }
        if let Err(error) = self.remove_release_link_if(app_id, "pending", candidate) {
            if self.read_release_link(app_id, "pending")? == Some(candidate) {
                return Err(error);
            }
            super::sync_directory(&app_directory)?;
        }
        Ok(())
    }

    /// Completes or repairs the filesystem side of a successful deployment.
    /// Each step is observed after an ambiguous error so the same operation
    /// can safely replay a visible active-link or metadata rename.
    pub fn finalize_active(
        &self,
        app_id: Uuid,
        expected_active: Option<Uuid>,
        candidate: Uuid,
        desired_state: DesiredState,
        operation_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let active = self.read_release_link(app_id, "active")?;
        if active == Some(candidate) {
            self.load_v2_release(app_id, candidate)?;
            self.remove_release_link_if(app_id, "pending", candidate)?;
            self.repair_app_durability(app_id)?;
        } else if active == expected_active {
            self.commit_active(app_id, expected_active, candidate)?;
        } else {
            return Err(StoreError::RevisionStale);
        }

        let metadata = self.read_metadata(app_id)?;
        if (metadata.desired_state != desired_state || metadata.last_operation_id != operation_id)
            && let Err(error) = self.set_desired_state(app_id, desired_state, operation_id, now)
        {
            let visible = self.read_metadata(app_id)?;
            if visible.desired_state != desired_state || visible.last_operation_id != operation_id {
                return Err(error);
            }
        }
        self.repair_app_durability(app_id)
    }

    pub fn remove_release_link_if(
        &self,
        app_id: Uuid,
        name: &str,
        expected: Uuid,
    ) -> Result<(), StoreError> {
        if self.read_release_link(app_id, name)? == Some(expected)
            && let Err(error) = AtomicWriter::remove_release_link(&self.app_directory(app_id), name)
        {
            if self.read_release_link(app_id, name)? == Some(expected) {
                return Err(error);
            }
            super::sync_directory(&self.app_directory(app_id))?;
        }
        Ok(())
    }

    pub fn read_release_link(&self, app_id: Uuid, name: &str) -> Result<Option<Uuid>, StoreError> {
        let path = self.app_directory(app_id).join(name);
        match fs::read_link(path) {
            Ok(target) => {
                let text = target.to_str().ok_or(StoreError::ContentInvalid)?;
                let id_text = text
                    .strip_prefix("releases/")
                    .ok_or(StoreError::SymlinkBoundary)?;
                let id = id_text
                    .parse::<Uuid>()
                    .map_err(|_| StoreError::ContentInvalid)?;
                if target != Path::new("releases").join(id.to_string()) {
                    return Err(StoreError::SymlinkBoundary);
                }
                Ok(Some(id))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

pub(crate) fn sign(release: &ReleaseV2, key: &[u8]) -> String {
    let mut canonical = release.clone();
    canonical.integrity_hmac.clear();
    let encoded = toml::to_string(&canonical).expect("release serializes");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key accepted");
    mac.update(b"solodock/release/v2\0");
    mac.update(encoded.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::{
        domain::{DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy},
        registry::Platform,
    };

    #[test]
    fn active_visible_finalizer_repairs_desired_state_and_pending_cleanup() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"release-finalizer-test-key".to_vec();
        let store = AppStore::initialize_verified(root.path().join("apps"), key.clone()).unwrap();
        let draft = normalize_draft(
            DraftInput {
                slug: "example".into(),
                display_name: "Example".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                environment: EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                networks: vec![],
                health: HealthPolicy::default(),
            },
            &ExistingSecrets::default(),
            &key,
            &[],
        )
        .unwrap();
        let app_id = Uuid::new_v4();
        let revision_id = Uuid::new_v4();
        let create_operation = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let metadata = store
            .create_app(app_id, revision_id, create_operation, &draft, now)
            .unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let release_id = Uuid::new_v4();
        store
            .publish_v2_release(
                &metadata,
                release_id,
                &ResolvedImage {
                    source_image_ref: "registry.example/app:stable".into(),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: format!("registry.example/app@{digest}"),
                    platform: Platform::canonical("linux", "amd64", None).unwrap(),
                    local_image_id: format!("sha256:{}", "b".repeat(64)),
                },
                ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        store.set_pending(app_id, release_id).unwrap();

        // Model the observable state after active rename and pending unlink,
        // but before the desired-state metadata write became durable.
        AtomicWriter::switch_release_link(&store.app_directory(app_id), "active", release_id)
            .unwrap();
        AtomicWriter::remove_release_link(&store.app_directory(app_id), "pending").unwrap();
        let finalize_operation = Uuid::new_v4();
        store
            .finalize_active(
                app_id,
                None,
                release_id,
                DesiredState::Running,
                finalize_operation,
                now,
            )
            .unwrap();
        let repaired = store.read_metadata(app_id).unwrap();
        assert_eq!(repaired.desired_state, DesiredState::Running);
        assert_eq!(repaired.last_operation_id, finalize_operation);
        assert_eq!(
            store.read_release_link(app_id, "active").unwrap(),
            Some(release_id)
        );
        assert_eq!(store.read_release_link(app_id, "pending").unwrap(), None);
    }
}
