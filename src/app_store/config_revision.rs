use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{StoreError, atomic::rename_no_replace, sync_directory};
use crate::{
    domain::{
        ConfigMetadata, DraftInput, EnvironmentInput, ExistingSecrets, ManagedFileContent,
        ManagedFileInput, NormalizedDraft, PublicEnvInput, SecretEnvInput, SecretOperation,
        dto::{DraftResponse, ManagedFileResponse},
    },
    security::permissions::{check_private, check_private_tree},
};

const REVISION_TEMP_PREFIX: &str = ".solodock-config-tmp-";

pub struct LoadedRevision {
    pub metadata: ConfigMetadata,
    pub public_environment: Vec<PublicEnvInput>,
    pub public_files: BTreeMap<String, String>,
    pub secrets: ExistingSecrets,
}

impl LoadedRevision {
    pub fn response(
        &self,
        discovery_image_ref: String,
        credential_ref: Option<Uuid>,
        auto_deploy_enabled: bool,
        poll_interval_seconds: u32,
    ) -> DraftResponse {
        DraftResponse {
            discovery_image_ref,
            credential_ref,
            auto_deploy_enabled,
            poll_interval_seconds,
            public_environment: self.public_environment.clone(),
            secret_keys: self.metadata.secret_keys.clone(),
            files: self
                .metadata
                .files
                .iter()
                .cloned()
                .map(|metadata| ManagedFileResponse {
                    content: if metadata.sensitive {
                        None
                    } else {
                        self.public_files.get(&metadata.logical_name).cloned()
                    },
                    metadata,
                })
                .collect(),
            ports: self.metadata.ports.clone(),
            volumes: self.metadata.volumes.clone(),
            binds: self.metadata.binds.clone(),
            owned_default_network: self.metadata.owned_default_network,
            networks: self.metadata.networks.clone(),
            health: self.metadata.health.clone(),
        }
    }

    pub fn known_secrets(&self) -> Vec<Vec<u8>> {
        self.secrets
            .environment
            .values()
            .chain(self.secrets.files.values())
            .filter(|value| !value.is_empty())
            .map(|value| value.as_bytes().to_vec())
            .collect()
    }

    pub fn input(
        &self,
        slug: String,
        display_name: String,
        discovery_image_ref: String,
        credential_ref: Option<Uuid>,
        auto_deploy_enabled: bool,
        poll_interval_seconds: u32,
    ) -> DraftInput {
        DraftInput {
            slug,
            display_name,
            discovery_image_ref,
            credential_ref,
            auto_deploy_enabled,
            auto_deploy_acknowledged: false,
            poll_interval_seconds,
            environment: EnvironmentInput {
                public: self.public_environment.clone(),
                secrets: self
                    .metadata
                    .secret_keys
                    .iter()
                    .map(|key| SecretEnvInput {
                        key: key.clone(),
                        operation: SecretOperation::Keep,
                    })
                    .collect(),
            },
            files: self
                .metadata
                .files
                .iter()
                .map(|file| ManagedFileInput {
                    logical_name: file.logical_name.clone(),
                    target_path: file.target_path.clone(),
                    sensitive: file.sensitive,
                    readonly: true,
                    content: if file.sensitive {
                        ManagedFileContent::Secret(SecretOperation::Keep)
                    } else {
                        ManagedFileContent::Public(crate::domain::PublicFileContent {
                            content: self
                                .public_files
                                .get(&file.logical_name)
                                .cloned()
                                .unwrap_or_default(),
                        })
                    },
                })
                .collect(),
            ports: self.metadata.ports.clone(),
            volumes: self.metadata.volumes.clone(),
            binds: self.metadata.binds.clone(),
            owned_default_network: self.metadata.owned_default_network,
            networks: self.metadata.networks.clone(),
            health: self.metadata.health.clone(),
        }
    }
}

pub fn publish(
    app_directory: &Path,
    revision_id: Uuid,
    draft: &NormalizedDraft,
) -> Result<PathBuf, StoreError> {
    let revisions = app_directory.join("config-revisions");
    ensure_dir(&revisions)?;
    let temp = revisions.join(format!("{REVISION_TEMP_PREFIX}{}", revision_id.simple()));
    let target = revisions.join(revision_id.to_string());
    let result = (|| {
        create_dir(&temp)?;
        create_dir(&temp.join("env"))?;
        create_dir(&temp.join("secrets"))?;
        create_dir(&temp.join("files"))?;
        create_dir(&temp.join("files/public"))?;
        create_dir(&temp.join("files/secret"))?;

        let config = toml::to_string(&draft.metadata).map_err(|_| StoreError::ContentInvalid)?;
        write_new(&temp.join("config.toml"), config.as_bytes())?;
        write_new(
            &temp.join("env/public.env"),
            serialize_environment(
                draft
                    .public_environment
                    .iter()
                    .map(|entry| (&entry.key, entry.value.as_str())),
            )
            .as_bytes(),
        )?;
        write_new(
            &temp.join("secrets/runtime.env"),
            serialize_environment(
                draft
                    .secret_environment
                    .iter()
                    .map(|(key, value)| (key, value.expose())),
            )
            .as_bytes(),
        )?;
        for (name, content) in &draft.public_files {
            write_new(&temp.join("files/public").join(name), content.as_bytes())?;
        }
        for (name, content) in &draft.secret_files {
            write_new(
                &temp.join("files/secret").join(name),
                content.expose().as_bytes(),
            )?;
        }
        sync_directory(&temp.join("env"))?;
        sync_directory(&temp.join("secrets"))?;
        sync_directory(&temp.join("files/public"))?;
        sync_directory(&temp.join("files/secret"))?;
        sync_directory(&temp.join("files"))?;
        sync_directory(&temp)?;
        rename_no_replace(&temp, &target).map_err(|error| match error {
            StoreError::ReleaseConflict => StoreError::ConfigRevisionConflict,
            other => other,
        })?;
        sync_directory(&revisions)?;
        Ok(target.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

pub fn load(app_directory: &Path, revision_id: Uuid) -> Result<LoadedRevision, StoreError> {
    let directory = app_directory
        .join("config-revisions")
        .join(revision_id.to_string());
    check_private_tree(app_directory, &directory, true)?;
    let config_path = directory.join("config.toml");
    check_private_tree(app_directory, &config_path, false)?;
    let config = fs::read_to_string(config_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            StoreError::ContentInvalid
        } else {
            error.into()
        }
    })?;
    let metadata: ConfigMetadata =
        toml::from_str(&config).map_err(|_| StoreError::ContentInvalid)?;
    if metadata.schema_version != 1 {
        return Err(StoreError::ContentInvalid);
    }
    let public_environment = read_environment(
        app_directory,
        &directory.join("env/public.env"),
        &metadata.public_env_keys,
    )?;
    let secret_environment = read_environment(
        app_directory,
        &directory.join("secrets/runtime.env"),
        &metadata.secret_keys,
    )?
    .into_iter()
    .map(|entry| (entry.key, entry.value))
    .collect();
    let mut public_files = BTreeMap::new();
    let mut secret_files = BTreeMap::new();
    let mut file_metadata = BTreeMap::new();
    for item in &metadata.files {
        let base = if item.sensitive {
            "files/secret"
        } else {
            "files/public"
        };
        let path = directory.join(base).join(&item.logical_name);
        check_private_tree(app_directory, &path, false)?;
        let value = fs::read_to_string(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                StoreError::ContentInvalid
            } else {
                error.into()
            }
        })?;
        file_metadata.insert(item.logical_name.clone(), item.clone());
        if item.sensitive {
            secret_files.insert(item.logical_name.clone(), value);
        } else {
            public_files.insert(item.logical_name.clone(), value);
        }
    }
    Ok(LoadedRevision {
        metadata,
        public_environment,
        public_files,
        secrets: ExistingSecrets {
            environment: secret_environment,
            files: secret_files,
            file_metadata,
        },
    })
}

pub fn load_verified(
    app_directory: &Path,
    revision_id: Uuid,
    hmac_key: &[u8],
) -> Result<LoadedRevision, StoreError> {
    let loaded = load(app_directory, revision_id)?;
    crate::domain::verify_config_integrity(
        &loaded.metadata,
        &loaded.public_environment,
        &loaded.public_files,
        &loaded.secrets,
        hmac_key,
    )
    .map_err(|_| StoreError::ContentInvalid)?;
    Ok(loaded)
}

fn create_dir(path: &Path) -> Result<(), StoreError> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

fn ensure_dir(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(check_private(path, true)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_dir(path)?;
            sync_directory(path.parent().ok_or(StoreError::ContentInvalid)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn write_new(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn serialize_environment<'a>(entries: impl Iterator<Item = (&'a String, &'a str)>) -> String {
    let mut result = String::new();
    for (key, value) in entries {
        result.push_str(key);
        result.push_str("='");
        result.push_str(&value.replace('\\', "\\\\").replace('\'', "\\'"));
        result.push_str("'\n");
    }
    result
}

fn read_environment(
    app_directory: &Path,
    path: &Path,
    expected: &[String],
) -> Result<Vec<PublicEnvInput>, StoreError> {
    check_private_tree(app_directory, path, false)?;
    let contents = fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            StoreError::ContentInvalid
        } else {
            error.into()
        }
    })?;
    let mut result = Vec::new();
    for line in contents.lines() {
        let (key, quoted) = line.split_once('=').ok_or(StoreError::ContentInvalid)?;
        let inner = quoted
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .ok_or(StoreError::ContentInvalid)?;
        let mut value = String::new();
        let mut escaped = false;
        for character in inner.chars() {
            if escaped {
                match character {
                    '\\' | '\'' => value.push(character),
                    _ => return Err(StoreError::ContentInvalid),
                }
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                value.push(character);
            }
        }
        if escaped {
            return Err(StoreError::ContentInvalid);
        }
        result.push(PublicEnvInput {
            key: key.to_owned(),
            value,
        });
    }
    if result.iter().map(|item| &item.key).collect::<Vec<_>>()
        != expected.iter().collect::<Vec<_>>()
    {
        return Err(StoreError::ContentInvalid);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn environment_round_trip_is_deterministic() {
        let entries = BTreeMap::from([
            ("A".to_owned(), "plain".to_owned()),
            ("B".to_owned(), "quote'and\\slash".to_owned()),
        ]);
        let serialized =
            serialize_environment(entries.iter().map(|(key, value)| (key, value.as_str())));
        assert_eq!(serialized, "A='plain'\nB='quote\\'and\\\\slash'\n");
    }

    #[test]
    fn immutable_revision_detects_secret_corruption() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let input = DraftInput {
            slug: "example".into(),
            display_name: "Example".into(),
            discovery_image_ref: "registry.example/app:stable".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: EnvironmentInput {
                public: vec![PublicEnvInput {
                    key: "MODE".into(),
                    value: "production".into(),
                }],
                secrets: vec![SecretEnvInput {
                    key: "TOKEN".into(),
                    operation: SecretOperation::Replace {
                        value: "secret-canary".into(),
                    },
                }],
            },
            files: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            binds: Vec::new(),
            owned_default_network: true,
            networks: Vec::new(),
            health: crate::domain::HealthPolicy::default(),
        };
        let key = b"revision-integrity-key";
        let draft =
            crate::domain::normalize_draft(input, &ExistingSecrets::default(), key, &[]).unwrap();
        let revision = Uuid::new_v4();
        publish(root.path(), revision, &draft).unwrap();
        assert_eq!(
            load_verified(root.path(), revision, key).unwrap().metadata,
            draft.metadata
        );
        let secret_path = root
            .path()
            .join("config-revisions")
            .join(revision.to_string())
            .join("secrets/runtime.env");
        fs::write(&secret_path, b"TOKEN='changed'\n").unwrap();
        assert!(matches!(
            load_verified(root.path(), revision, key),
            Err(StoreError::ContentInvalid)
        ));
    }

    #[test]
    fn load_rejects_intermediate_directory_symlink_swap() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let input = DraftInput {
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
            owned_default_network: true,
            networks: vec![],
            health: crate::domain::HealthPolicy::default(),
        };
        let key = b"revision-integrity-key";
        let draft =
            crate::domain::normalize_draft(input, &ExistingSecrets::default(), key, &[]).unwrap();
        let revision = Uuid::new_v4();
        publish(root.path(), revision, &draft).unwrap();
        let revision_dir = root
            .path()
            .join("config-revisions")
            .join(revision.to_string());
        let env = revision_dir.join("env");
        let displaced = revision_dir.join("env-real");
        fs::rename(&env, &displaced).unwrap();
        symlink(&displaced, &env).unwrap();
        assert!(load_verified(root.path(), revision, key).is_err());
    }
}
