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
    domain::{AppMetadata, DesiredState},
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
    pub compose_schema_version: u32,
    pub id: Uuid,
    pub app_id: Uuid,
    pub config_revision: Uuid,
    pub config_sha256: String,
    #[serde(default = "crate::domain::default_stop_grace_period_seconds")]
    pub stop_grace_period_seconds: u16,
    #[serde(default)]
    pub service_discovery_enabled: bool,
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

impl ReleaseV2 {
    pub(crate) fn apply_schema_defaults(&mut self) {
        if self.schema_version == 3 {
            // The field was not covered by schema 3's HMAC. Its only safe
            // effective value is the legacy default, even if injected into
            // the TOML artifact explicitly.
            self.stop_grace_period_seconds = crate::domain::default_stop_grace_period_seconds();
        }
        if matches!(self.schema_version, 3 | 4) {
            self.service_discovery_enabled = false;
        }
    }
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
        let draft_revision = app.draft_revision.ok_or(StoreError::ContentInvalid)?;
        let draft_config_sha256 = app
            .draft_config_sha256
            .clone()
            .ok_or(StoreError::ContentInvalid)?;
        let discovery_image_ref = app
            .discovery_image_ref
            .clone()
            .ok_or(StoreError::ContentInvalid)?;
        let loaded =
            config_revision::load_verified(&app_directory, draft_revision, self.integrity_key()?)?;
        let allowed_bind_roots = self.allowed_bind_roots();
        let draft = loaded
            .normalize_verified(
                app.display_name.clone(),
                discovery_image_ref,
                app.credential_ref,
                app.auto_deploy_enabled,
                app.poll_interval_seconds,
                self.integrity_key()?,
                &allowed_bind_roots,
            )
            .map_err(|_| StoreError::ContentInvalid)?;
        if draft.metadata.config_sha256 != draft_config_sha256 {
            return Err(StoreError::ContentInvalid);
        }
        let revision_directory = app_directory
            .join("config-revisions")
            .join(draft_revision.to_string());
        let (compose, _) = generate(
            ComposeInput {
                resource_identity: app.resource_identity(),
                release_id,
                image_ref: &resolved.runnable_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
                include_stop_grace_period: true,
            },
            true,
        )
        .map_err(|_| StoreError::ContentInvalid)?;
        let compose_sha256 = format!("{:x}", Sha256::digest(compose.as_bytes()));
        let mut release = ReleaseV2 {
            schema_version: 5,
            compose_schema_version: 4,
            id: release_id,
            app_id: app.id,
            config_revision: draft_revision,
            config_sha256: draft_config_sha256,
            stop_grace_period_seconds: draft.stop_grace_period_seconds,
            service_discovery_enabled: draft.service_discovery_enabled,
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
        let mut release: ReleaseV2 = toml::from_str(&fs::read_to_string(header_path)?)
            .map_err(|_| StoreError::ContentInvalid)?;
        release.apply_schema_defaults();
        if !matches!(
            (release.schema_version, release.compose_schema_version),
            (3, 2) | (4, 3) | (5, 4)
        ) || release.id != release_id
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
        let allowed_bind_roots = self.allowed_bind_roots();
        let draft = loaded
            .normalize_verified(
                app.display_name.clone(),
                release.source_image_ref.clone(),
                release.credential_ref,
                app.auto_deploy_enabled,
                app.poll_interval_seconds,
                self.integrity_key()?,
                &allowed_bind_roots,
            )
            .map_err(|_| StoreError::ContentInvalid)?;
        if release.stop_grace_period_seconds != loaded.metadata.stop_grace_period_seconds {
            return Err(StoreError::ContentInvalid);
        }
        if release.service_discovery_enabled != loaded.metadata.service_discovery_enabled {
            return Err(StoreError::ContentInvalid);
        }
        let revision_directory = self
            .app_directory(app_id)
            .join("config-revisions")
            .join(release.config_revision.to_string());
        let (canonical, _) = generate(
            ComposeInput {
                resource_identity: app.resource_identity(),
                release_id,
                image_ref: &release.runnable_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
                include_stop_grace_period: release.compose_schema_version >= 3,
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

impl ReleaseV2 {
    pub fn image_identity(
        &self,
    ) -> Result<crate::registry::ImageIdentity, crate::registry::RegistryError> {
        crate::registry::ImageIdentity::new(
            &self.manifest_digest,
            &self.local_image_id,
            &crate::registry::Platform {
                os: self.platform_os.clone(),
                architecture: self.platform_architecture.clone(),
                variant: self.platform_variant.clone(),
            },
        )
    }
}

pub(crate) fn sign(release: &ReleaseV2, key: &[u8]) -> String {
    let encoded = if matches!(release.schema_version, 3 | 4) {
        #[derive(Serialize)]
        struct LegacyRelease<'a> {
            schema_version: u32,
            compose_schema_version: u32,
            id: Uuid,
            app_id: Uuid,
            config_revision: Uuid,
            config_sha256: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            stop_grace_period_seconds: Option<u16>,
            source_image_ref: &'a str,
            logical_registry: &'a str,
            repository: &'a str,
            source_tag: &'a str,
            source_descriptor_digest: &'a str,
            index_digest: Option<&'a str>,
            manifest_digest: &'a str,
            runnable_image_ref: &'a str,
            platform_os: &'a str,
            platform_architecture: &'a str,
            platform_variant: Option<&'a str>,
            local_image_id: &'a str,
            compose_sha256: &'a str,
            credential_ref: Option<Uuid>,
            trigger: ReleaseTrigger,
            source_release_id: Option<Uuid>,
            #[serde(with = "time::serde::rfc3339")]
            created_at: OffsetDateTime,
            integrity_hmac: &'static str,
        }
        toml::to_string(&LegacyRelease {
            schema_version: release.schema_version,
            compose_schema_version: release.compose_schema_version,
            id: release.id,
            app_id: release.app_id,
            config_revision: release.config_revision,
            config_sha256: &release.config_sha256,
            stop_grace_period_seconds: (release.schema_version == 4)
                .then_some(release.stop_grace_period_seconds),
            source_image_ref: &release.source_image_ref,
            logical_registry: &release.logical_registry,
            repository: &release.repository,
            source_tag: &release.source_tag,
            source_descriptor_digest: &release.source_descriptor_digest,
            index_digest: release.index_digest.as_deref(),
            manifest_digest: &release.manifest_digest,
            runnable_image_ref: &release.runnable_image_ref,
            platform_os: &release.platform_os,
            platform_architecture: &release.platform_architecture,
            platform_variant: release.platform_variant.as_deref(),
            local_image_id: &release.local_image_id,
            compose_sha256: &release.compose_sha256,
            credential_ref: release.credential_ref,
            trigger: release.trigger,
            source_release_id: release.source_release_id,
            created_at: release.created_at,
            integrity_hmac: "",
        })
        .expect("legacy release serializes")
    } else {
        let mut canonical = release.clone();
        canonical.integrity_hmac.clear();
        toml::to_string(&canonical).expect("release serializes")
    };
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
        domain::{DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy, normalize_draft},
        registry::Platform,
    };

    #[test]
    fn serialized_current_release_round_trips_with_compose_schema_marker() {
        let manifest = format!("sha256:{}", "a".repeat(64));
        let mut release = ReleaseV2 {
            schema_version: 4,
            compose_schema_version: 3,
            id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
            app_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
            config_revision: Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap(),
            config_sha256: "0".repeat(64),
            stop_grace_period_seconds: 60,
            service_discovery_enabled: false,
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: manifest.clone(),
            index_digest: None,
            manifest_digest: manifest.clone(),
            runnable_image_ref: format!("registry.example/app@{manifest}"),
            platform_os: "linux".into(),
            platform_architecture: "amd64".into(),
            platform_variant: None,
            local_image_id: format!("sha256:{}", "b".repeat(64)),
            compose_sha256: "1".repeat(64),
            credential_ref: None,
            trigger: ReleaseTrigger::Manual,
            source_release_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            integrity_hmac: String::new(),
        };
        let key = b"existing-v2-release-key";
        release.integrity_hmac = sign(&release, key);
        assert_eq!(release.integrity_hmac.len(), 64);
        let serialized = toml::to_string(&release).unwrap();
        assert!(serialized.contains("local_image_id = "));
        assert!(serialized.contains("compose_schema_version = 3"));
        assert!(serialized.contains("stop_grace_period_seconds = 60"));

        let parsed: ReleaseV2 = toml::from_str(&serialized).unwrap();
        assert_eq!(sign(&parsed, key), parsed.integrity_hmac);
        assert_eq!(toml::to_string(&parsed).unwrap(), serialized);
        let identity = parsed.image_identity().unwrap();
        assert!(identity.matches_engine_image_id(Some(&parsed.local_image_id)));
        assert!(identity.matches_engine_image_id(Some(&parsed.manifest_digest)));
    }

    #[test]
    fn legacy_release_without_stop_grace_keeps_hmac_and_defaults_to_ten_seconds() {
        let manifest = format!("sha256:{}", "a".repeat(64));
        let mut release = ReleaseV2 {
            schema_version: 3,
            compose_schema_version: 2,
            id: Uuid::new_v4(),
            app_id: Uuid::new_v4(),
            config_revision: Uuid::new_v4(),
            config_sha256: "0".repeat(64),
            stop_grace_period_seconds: 10,
            service_discovery_enabled: false,
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: manifest.clone(),
            index_digest: None,
            manifest_digest: manifest.clone(),
            runnable_image_ref: format!("registry.example/app@{manifest}"),
            platform_os: "linux".into(),
            platform_architecture: "amd64".into(),
            platform_variant: None,
            local_image_id: manifest,
            compose_sha256: "1".repeat(64),
            credential_ref: None,
            trigger: ReleaseTrigger::Manual,
            source_release_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            integrity_hmac: String::new(),
        };
        let key = b"legacy-release-key";
        release.integrity_hmac = sign(&release, key);
        let fixture = toml::to_string(&release)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("stop_grace_period_seconds = "))
            .collect::<Vec<_>>()
            .join("\n");
        let mut parsed: ReleaseV2 = toml::from_str(&fixture).unwrap();
        parsed.apply_schema_defaults();
        assert_eq!(parsed.stop_grace_period_seconds, 10);
        assert_eq!(sign(&parsed, key), parsed.integrity_hmac);

        let injected = fixture.replace(
            "compose_schema_version = 2",
            "compose_schema_version = 2\nstop_grace_period_seconds = 600",
        );
        let mut parsed: ReleaseV2 = toml::from_str(&injected).unwrap();
        parsed.apply_schema_defaults();
        assert_eq!(parsed.stop_grace_period_seconds, 10);
        assert_eq!(sign(&parsed, key), parsed.integrity_hmac);
    }

    #[test]
    fn legacy_config_and_release_remain_loadable_and_can_publish_current_release() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"legacy-config-release-key".to_vec();
        let store = AppStore::initialize_verified(root.path().join("apps"), key.clone()).unwrap();
        let mut draft = normalize_draft(
            DraftInput {
                display_name: "Legacy".into(),
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
                service_discovery_enabled: false,
                networks: vec![],
                health: HealthPolicy::default(),
            },
            &ExistingSecrets::default(),
            &key,
            &[],
        )
        .unwrap();
        draft.metadata.schema_version = 1;
        draft.metadata.config_sha256 = crate::domain::recompute_config_hash_for_test(
            &draft.metadata,
            &draft.public_environment,
            &draft.public_files,
        );
        let app_id = Uuid::new_v4();
        let revision_id = Uuid::new_v4();
        let metadata = store
            .create_app(
                app_id,
                "legacy",
                Uuid::new_v4(),
                Some((revision_id, &draft)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let config_path = store
            .app_directory(app_id)
            .join("config-revisions")
            .join(revision_id.to_string())
            .join("config.toml");
        let legacy_config = fs::read_to_string(&config_path)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("stop_grace_period_seconds = "))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&config_path, &legacy_config).unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let resolved = ResolvedImage {
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: digest.clone(),
            index_digest: None,
            manifest_digest: digest.clone(),
            runnable_image_ref: format!("registry.example/app@{digest}"),
            platform: Platform::canonical("linux", "amd64", None).unwrap(),
            local_image_id: digest,
        };

        let legacy_release_id = Uuid::new_v4();
        let mut legacy_release = store
            .publish_v2_release(
                &metadata,
                legacy_release_id,
                &resolved,
                ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        let revision_directory = store
            .app_directory(app_id)
            .join("config-revisions")
            .join(revision_id.to_string());
        let (legacy_compose, _) = generate(
            ComposeInput {
                resource_identity: metadata.resource_identity(),
                release_id: legacy_release_id,
                image_ref: &resolved.runnable_image_ref,
                revision_directory: &revision_directory,
                draft: &draft,
                include_stop_grace_period: false,
            },
            true,
        )
        .unwrap();
        legacy_release.schema_version = 3;
        legacy_release.compose_schema_version = 2;
        legacy_release.compose_sha256 = format!("{:x}", Sha256::digest(legacy_compose.as_bytes()));
        legacy_release.integrity_hmac = sign(&legacy_release, &key);
        let legacy_directory = store
            .app_directory(app_id)
            .join("releases")
            .join(legacy_release_id.to_string());
        let legacy_release_toml = toml::to_string(&legacy_release)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("stop_grace_period_seconds = "))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(legacy_directory.join("release.toml"), &legacy_release_toml).unwrap();
        fs::write(legacy_directory.join("compose.yaml"), legacy_compose).unwrap();

        assert_eq!(
            store
                .load_v2_release(app_id, legacy_release_id)
                .unwrap()
                .stop_grace_period_seconds,
            10
        );
        store.set_pending(app_id, legacy_release_id).unwrap();
        store
            .commit_active(app_id, None, legacy_release_id)
            .unwrap();
        let recovered = store.scan().unwrap();
        assert!(recovered.issues.is_empty(), "{:?}", recovered.issues);
        assert_eq!(recovered.valid_apps.len(), 1);

        // Neither legacy signature covered the new field. Injecting it must
        // therefore never change the effective lifecycle control value.
        fs::write(
            &config_path,
            legacy_config.replace(
                "schema_version = 1",
                "schema_version = 1\nstop_grace_period_seconds = 600",
            ),
        )
        .unwrap();
        fs::write(
            legacy_directory.join("release.toml"),
            legacy_release_toml.replace(
                "compose_schema_version = 2",
                "compose_schema_version = 2\nstop_grace_period_seconds = 600",
            ),
        )
        .unwrap();
        assert_eq!(
            config_revision::load_verified(&store.app_directory(app_id), revision_id, &key)
                .unwrap()
                .metadata
                .stop_grace_period_seconds,
            10
        );
        assert_eq!(
            store
                .load_v2_release(app_id, legacy_release_id)
                .unwrap()
                .stop_grace_period_seconds,
            10
        );
        assert!(store.scan().unwrap().issues.is_empty());

        let current_release_id = Uuid::new_v4();
        let current = store
            .publish_v2_release(
                &metadata,
                current_release_id,
                &resolved,
                ReleaseTrigger::Manual,
                Some(legacy_release_id),
            )
            .unwrap();
        assert_eq!(
            (current.schema_version, current.compose_schema_version),
            (5, 4)
        );
        let current_compose =
            fs::read_to_string(store.release_compose_path(app_id, current_release_id)).unwrap();
        assert!(current_compose.contains("stop_grace_period:"));
        assert!(current_compose.contains("10s"));
        store.load_v2_release(app_id, current_release_id).unwrap();
    }

    #[test]
    fn active_visible_finalizer_repairs_desired_state_and_pending_cleanup() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"release-finalizer-test-key".to_vec();
        let store = AppStore::initialize_verified(root.path().join("apps"), key.clone()).unwrap();
        let draft = normalize_draft(
            DraftInput {
                display_name: "Example".into(),
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
            &key,
            &[],
        )
        .unwrap();
        let app_id = Uuid::new_v4();
        let revision_id = Uuid::new_v4();
        let create_operation = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let metadata = store
            .create_app(
                app_id,
                "example",
                create_operation,
                Some((revision_id, &draft)),
                now,
            )
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
