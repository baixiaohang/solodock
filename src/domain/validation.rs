use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::*;

pub const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_ENV_ENTRIES: usize = 256;
pub const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_ENV_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_FILES: usize = 32;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_FILE_TOTAL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigMetadata {
    pub schema_version: u32,
    pub public_env_keys: Vec<String>,
    pub secret_keys: Vec<String>,
    pub secret_hmacs: BTreeMap<String, String>,
    pub files: Vec<ManagedFileMetadata>,
    pub public_file_sha256s: BTreeMap<String, String>,
    pub secret_file_hmacs: BTreeMap<String, String>,
    pub ports: Vec<PortInput>,
    pub volumes: Vec<VolumeInput>,
    pub binds: Vec<BindMountInput>,
    #[serde(
        default = "default_owned_default_network",
        skip_serializing_if = "is_true"
    )]
    pub owned_default_network: bool,
    pub networks: Vec<NetworkInput>,
    pub health: HealthPolicy,
    pub config_sha256: String,
}

#[derive(Default)]
pub struct ExistingSecrets {
    pub environment: BTreeMap<String, String>,
    pub files: BTreeMap<String, String>,
    pub file_metadata: BTreeMap<String, ManagedFileMetadata>,
}

pub struct NormalizedDraft {
    pub slug: String,
    pub display_name: String,
    pub discovery_image_ref: String,
    pub credential_ref: Option<uuid::Uuid>,
    pub auto_deploy_enabled: bool,
    pub auto_deploy_acknowledged: bool,
    pub poll_interval_seconds: u32,
    pub public_environment: Vec<PublicEnvInput>,
    pub secret_environment: SecretMap,
    pub files: Vec<ManagedFileMetadata>,
    pub public_files: BTreeMap<String, String>,
    pub secret_files: SecretMap,
    pub ports: Vec<PortInput>,
    pub volumes: Vec<VolumeInput>,
    pub binds: Vec<BindMountInput>,
    pub owned_default_network: bool,
    pub networks: Vec<NetworkInput>,
    pub health: HealthPolicy,
    pub metadata: ConfigMetadata,
}

impl NormalizedDraft {
    pub fn known_secrets(&self) -> Vec<Vec<u8>> {
        self.secret_environment
            .values()
            .chain(self.secret_files.values())
            .filter(|value| !value.expose().is_empty())
            .map(|value| value.expose().as_bytes().to_vec())
            .collect()
    }
}

pub fn normalize_draft(
    input: DraftInput,
    existing: &ExistingSecrets,
    hmac_key: &[u8],
    allowed_bind_roots: &[PathBuf],
) -> Result<NormalizedDraft, DomainError> {
    validate_slug(&input.slug)?;
    validate_display_name(&input.display_name)?;
    validate_discovery_image(&input.discovery_image_ref)?;
    if !(60..=86_400).contains(&input.poll_interval_seconds) {
        return Err(DomainError::ConfigInvalid);
    }

    let (public_environment, secret_environment) =
        normalize_environment(input.environment, existing, hmac_key)?;
    let (files, public_files, secret_files) = normalize_files(input.files, existing, hmac_key)?;
    let mut ports = input.ports;
    validate_ports(&ports)?;
    ports.sort_by(|left, right| {
        (
            &left.host_ip,
            left.host_port,
            left.protocol,
            left.container_port,
        )
            .cmp(&(
                &right.host_ip,
                right.host_port,
                right.protocol,
                right.container_port,
            ))
    });
    let mut volumes = input.volumes;
    validate_volumes(&volumes)?;
    volumes.sort_by_key(volume_sort_key);
    let mut binds = input.binds;
    validate_binds(&binds, allowed_bind_roots)?;
    binds.sort_by(|left, right| left.target_path.cmp(&right.target_path));
    let mut networks = input.networks;
    normalize_networks(input.owned_default_network, &mut networks)?;
    validate_health(&input.health)?;
    validate_mount_target_conflicts(&files, &volumes, &binds)?;

    let public_env_keys = public_environment
        .iter()
        .map(|entry| entry.key.clone())
        .collect();
    let secret_keys = secret_environment.keys().cloned().collect();
    let secret_hmacs = secret_environment
        .iter()
        .map(|(key, value)| Ok((key.clone(), hmac_hex(hmac_key, value.expose().as_bytes())?)))
        .collect::<Result<_, DomainError>>()?;
    let public_file_sha256s = public_files
        .iter()
        .map(|(name, content)| {
            (
                name.clone(),
                hex(Sha256::digest(content.as_bytes()).as_slice()),
            )
        })
        .collect();
    let secret_file_hmacs = secret_files
        .iter()
        .map(|(key, value)| Ok((key.clone(), hmac_hex(hmac_key, value.expose().as_bytes())?)))
        .collect::<Result<_, DomainError>>()?;
    let mut metadata = ConfigMetadata {
        schema_version: 1,
        public_env_keys,
        secret_keys,
        secret_hmacs,
        files: files.clone(),
        public_file_sha256s,
        secret_file_hmacs,
        ports: ports.clone(),
        volumes: volumes.clone(),
        binds: binds.clone(),
        owned_default_network: input.owned_default_network,
        networks: networks.clone(),
        health: input.health.clone(),
        config_sha256: String::new(),
    };
    let canonical = canonical_non_secret(&metadata, &public_environment, &public_files)?;
    let digest = Sha256::digest(canonical);
    metadata.config_sha256 = hex(&digest);
    Ok(NormalizedDraft {
        slug: input.slug,
        display_name: input.display_name.trim().to_owned(),
        discovery_image_ref: input.discovery_image_ref,
        credential_ref: input.credential_ref,
        auto_deploy_enabled: input.auto_deploy_enabled,
        auto_deploy_acknowledged: input.auto_deploy_acknowledged,
        poll_interval_seconds: input.poll_interval_seconds,
        public_environment,
        secret_environment,
        files,
        public_files,
        secret_files,
        ports,
        volumes,
        binds,
        owned_default_network: input.owned_default_network,
        networks,
        health: input.health,
        metadata,
    })
}

fn normalize_environment(
    input: EnvironmentInput,
    existing: &ExistingSecrets,
    _hmac_key: &[u8],
) -> Result<(Vec<PublicEnvInput>, SecretMap), DomainError> {
    if input.public.len() + input.secrets.len() > MAX_ENV_ENTRIES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    let mut public_keys = HashSet::new();
    let mut total = 0usize;
    let mut public = Vec::with_capacity(input.public.len());
    for entry in input.public {
        validate_env_key(&entry.key)?;
        validate_text(&entry.value)?;
        validate_env_value(&entry.value)?;
        if entry.value.len() > MAX_ENV_VALUE_BYTES {
            return Err(DomainError::ConfigQuotaExceeded);
        }
        if !public_keys.insert(entry.key.clone()) {
            return Err(DomainError::EnvDuplicate);
        }
        total = total.saturating_add(entry.value.len());
        public.push(entry);
    }
    let mut secret = BTreeMap::new();
    let mut operations = HashSet::new();
    for entry in input.secrets {
        validate_env_key(&entry.key)?;
        if !operations.insert(entry.key.clone()) {
            return Err(DomainError::EnvDuplicate);
        }
        if public_keys.contains(&entry.key) && !matches!(entry.operation, SecretOperation::Delete) {
            return Err(DomainError::EnvDuplicate);
        }
        match entry.operation {
            SecretOperation::Keep => {
                let value = existing
                    .environment
                    .get(&entry.key)
                    .ok_or(DomainError::SecretOperationRequired)?;
                total = total.saturating_add(value.len());
                secret.insert(entry.key, SecretMaterial::new(value.clone()));
            }
            SecretOperation::Replace { value } => {
                validate_text(&value)?;
                validate_env_value(&value)?;
                if value.len() > MAX_ENV_VALUE_BYTES {
                    return Err(DomainError::ConfigQuotaExceeded);
                }
                total = total.saturating_add(value.len());
                secret.insert(entry.key, SecretMaterial::new(value));
            }
            SecretOperation::Delete => {
                if !existing.environment.contains_key(&entry.key) {
                    return Err(DomainError::SecretOperationRequired);
                }
            }
        }
    }
    if existing
        .environment
        .keys()
        .any(|key| !operations.contains(key))
    {
        return Err(DomainError::SecretOperationRequired);
    }
    if total > MAX_ENV_TOTAL_BYTES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    public.sort_by(|left, right| left.key.cmp(&right.key));
    Ok((public, secret))
}

fn validate_env_value(value: &str) -> Result<(), DomainError> {
    if value.contains(['\n', '\r']) {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

type NormalizedFiles = (
    Vec<ManagedFileMetadata>,
    BTreeMap<String, String>,
    SecretMap,
);

fn normalize_files(
    input: Vec<ManagedFileInput>,
    existing: &ExistingSecrets,
    _hmac_key: &[u8],
) -> Result<NormalizedFiles, DomainError> {
    if input.len() > MAX_FILES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    let mut final_names = HashSet::new();
    let mut targets = HashSet::new();
    let mut secret_operations = HashSet::new();
    let mut public_names = HashSet::new();
    let mut metadata = Vec::new();
    let mut public = BTreeMap::new();
    let mut secret = BTreeMap::new();
    let mut total = 0usize;
    for item in input {
        validate_logical_name(&item.logical_name)?;
        validate_container_target(&item.target_path)?;
        if !item.readonly {
            return Err(DomainError::ConfigInvalid);
        }
        let descriptor = ManagedFileMetadata {
            logical_name: item.logical_name.clone(),
            target_path: item.target_path,
            sensitive: item.sensitive,
            readonly: true,
        };
        match (item.sensitive, item.content) {
            (false, ManagedFileContent::Public(PublicFileContent { content })) => {
                if !public_names.insert(item.logical_name.clone())
                    || !final_names.insert(item.logical_name.clone())
                    || !targets.insert(descriptor.target_path.clone())
                {
                    return Err(DomainError::FileTargetConflict);
                }
                validate_content(&content)?;
                check_file_quota(&content, &mut total)?;
                public.insert(item.logical_name, content);
                metadata.push(descriptor);
            }
            (true, ManagedFileContent::Secret(operation)) => {
                if !secret_operations.insert(item.logical_name.clone()) {
                    return Err(DomainError::FileTargetConflict);
                }
                match operation {
                    SecretOperation::Keep => {
                        let value = existing
                            .files
                            .get(&item.logical_name)
                            .ok_or(DomainError::SecretOperationRequired)?;
                        if public_names.contains(&item.logical_name)
                            || !final_names.insert(item.logical_name.clone())
                            || !targets.insert(descriptor.target_path.clone())
                        {
                            return Err(DomainError::FileTargetConflict);
                        }
                        check_file_quota(value, &mut total)?;
                        secret.insert(item.logical_name, SecretMaterial::new(value.clone()));
                        metadata.push(descriptor);
                    }
                    SecretOperation::Replace { value } => {
                        if public_names.contains(&item.logical_name)
                            || !final_names.insert(item.logical_name.clone())
                            || !targets.insert(descriptor.target_path.clone())
                        {
                            return Err(DomainError::FileTargetConflict);
                        }
                        validate_content(&value)?;
                        check_file_quota(&value, &mut total)?;
                        secret.insert(item.logical_name, SecretMaterial::new(value));
                        metadata.push(descriptor);
                    }
                    SecretOperation::Delete => {
                        if !existing.files.contains_key(&item.logical_name) {
                            return Err(DomainError::SecretOperationRequired);
                        }
                    }
                }
            }
            _ => return Err(DomainError::ConfigInvalid),
        }
    }
    if existing
        .files
        .keys()
        .any(|name| !secret_operations.contains(name))
    {
        return Err(DomainError::SecretOperationRequired);
    }
    if total > MAX_FILE_TOTAL_BYTES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    metadata.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
    Ok((metadata, public, secret))
}

fn check_file_quota(value: &str, total: &mut usize) -> Result<(), DomainError> {
    if value.len() > MAX_FILE_BYTES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    *total = total.saturating_add(value.len());
    Ok(())
}

fn validate_slug(value: &str) -> Result<(), DomainError> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_display_name(value: &str) -> Result<(), DomainError> {
    validate_text(value)?;
    let trimmed = value.trim();
    if trimmed != value || !(1..=80).contains(&value.chars().count()) {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_discovery_image(value: &str) -> Result<(), DomainError> {
    crate::registry::ImageReference::parse(value)
        .map(|_| ())
        .map_err(|_| DomainError::ConfigInvalid)
}

pub fn validate_runnable_image(value: &str) -> Result<(), DomainError> {
    if value.len() > 320 || value.contains(['$', '{', '}', '`', '\\', '"', '\'']) {
        return Err(DomainError::ConfigInvalid);
    }
    let (repository, digest) = value
        .split_once("@sha256:")
        .ok_or(DomainError::ConfigInvalid)?;
    if repository.contains('@')
        || !valid_repository(repository)
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn valid_repository(value: &str) -> bool {
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return false;
    }
    let mut parts = value.split('/').peekable();
    let first = match parts.next() {
        Some(first) if !first.is_empty() => first,
        _ => return false,
    };
    let first_is_registry = first.contains('.') || first.contains(':') || first == "localhost";
    if first_is_registry && !valid_registry(first) {
        return false;
    }
    let repository_parts = if first_is_registry {
        parts.collect::<Vec<_>>()
    } else {
        std::iter::once(first).chain(parts).collect::<Vec<_>>()
    };
    !repository_parts.is_empty()
        && repository_parts
            .iter()
            .all(|part| valid_repository_component(part))
}

fn valid_registry(value: &str) -> bool {
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && port
            .is_none_or(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

fn validate_env_key(value: &str) -> Result<(), DomainError> {
    if value.is_empty()
        || value.len() > 128
        || !(value.as_bytes()[0].is_ascii_alphabetic() || value.as_bytes()[0] == b'_')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_logical_name(value: &str) -> Result<(), DomainError> {
    if value.is_empty()
        || value.len() > 64
        || matches!(value, "." | "..")
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

pub fn validate_container_target(value: &str) -> Result<(), DomainError> {
    validate_text(value)?;
    let path = Path::new(value);
    if value.len() > 4096
        || !path.is_absolute()
        || value == "/"
        || value.contains("//")
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DomainError::ConfigInvalid);
    }
    const BLOCKED: [&str; 7] = [
        "/proc",
        "/sys",
        "/dev",
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/run",
        "/var/run",
    ];
    if BLOCKED
        .iter()
        .any(|blocked| path == Path::new(blocked) || path.starts_with(Path::new(blocked)))
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_ports(ports: &[PortInput]) -> Result<(), DomainError> {
    if ports.len() > 32 {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    let mut published = HashSet::new();
    let mut normalized = HashSet::new();
    for port in ports {
        if !matches!(port.host_ip.as_str(), "127.0.0.1" | "::1")
            || port.host_port == 0
            || port.container_port == 0
        {
            return Err(DomainError::ConfigInvalid);
        }
        if !published.insert((port.host_ip.clone(), port.host_port, port.protocol))
            || !normalized.insert((
                port.host_ip.clone(),
                port.host_port,
                port.container_port,
                port.protocol,
            ))
        {
            return Err(DomainError::PortConflict);
        }
    }
    Ok(())
}

fn validate_volumes(volumes: &[VolumeInput]) -> Result<(), DomainError> {
    if volumes.len() > 16 {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    let mut names = HashSet::new();
    for volume in volumes {
        validate_container_target(volume.target_path())?;
        let name = match volume {
            VolumeInput::Owned { logical_name, .. } => {
                validate_logical_name(logical_name)?;
                logical_name
            }
            VolumeInput::External { name, .. } => {
                validate_docker_name(name)?;
                name
            }
        };
        if !names.insert(name.clone()) {
            return Err(DomainError::ConfigInvalid);
        }
    }
    Ok(())
}

fn validate_docker_name(value: &str) -> Result<(), DomainError> {
    validate_text(value)?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

pub fn validate_binds(
    binds: &[BindMountInput],
    allowed_roots: &[PathBuf],
) -> Result<Vec<BindIdentity>, DomainError> {
    if binds.len() > 16 {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    if !binds.is_empty() && allowed_roots.is_empty() {
        return Err(DomainError::BindDisabled);
    }
    let mut result = Vec::with_capacity(binds.len());
    for bind in binds {
        validate_container_target(&bind.target_path)?;
        if !bind.readonly && !bind.acknowledge_non_rollbackable {
            return Err(DomainError::BindRwAckRequired);
        }
        result.push(validate_bind_source(
            Path::new(&bind.source),
            allowed_roots,
        )?);
    }
    Ok(result)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindIdentity {
    pub path: PathBuf,
    #[cfg(unix)]
    pub device: u64,
    #[cfg(unix)]
    pub inode: u64,
}

pub fn validate_bind_source(
    source: &Path,
    allowed_roots: &[PathBuf],
) -> Result<BindIdentity, DomainError> {
    if !source.is_absolute()
        || source
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DomainError::BindOutsideAllowedRoot);
    }
    const SENSITIVE: [&str; 8] = [
        "/",
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/var/run",
        "/var/lib/docker",
    ];
    if SENSITIVE.iter().any(|path| {
        let sensitive = Path::new(path);
        source == sensitive || (sensitive != Path::new("/") && source.starts_with(sensitive))
    }) {
        return Err(DomainError::BindOutsideAllowedRoot);
    }
    let root = allowed_roots
        .iter()
        .find(|root| source != root.as_path() && source.starts_with(root))
        .ok_or(DomainError::BindOutsideAllowedRoot)?;
    let mut current = PathBuf::from("/");
    for component in source.components().skip(1) {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|_| DomainError::ConfigInvalid)?;
        if metadata.file_type().is_symlink() {
            return Err(DomainError::BindSymlink);
        }
    }
    let canonical = fs::canonicalize(source).map_err(|_| DomainError::ConfigInvalid)?;
    let canonical_root = fs::canonicalize(root).map_err(|_| DomainError::ConfigInvalid)?;
    if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
        return Err(DomainError::BindOutsideAllowedRoot);
    }
    let metadata = fs::metadata(&canonical).map_err(|_| DomainError::ConfigInvalid)?;
    if !metadata.is_dir() {
        return Err(DomainError::ConfigInvalid);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(BindIdentity {
            path: canonical,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    Ok(BindIdentity { path: canonical })
}

pub fn revalidate_bind_identity(
    expected: &BindIdentity,
    allowed_roots: &[PathBuf],
) -> Result<(), DomainError> {
    let current = validate_bind_source(&expected.path, allowed_roots)?;
    if &current != expected {
        return Err(DomainError::BindChanged);
    }
    Ok(())
}

fn validate_health(health: &HealthPolicy) -> Result<(), DomainError> {
    match health {
        HealthPolicy::Healthy { http: None } | HealthPolicy::Completed => Ok(()),
        HealthPolicy::Running {
            stable_window_seconds,
        } if (5..=300).contains(stable_window_seconds) => Ok(()),
        HealthPolicy::Disabled {
            acknowledge_reduced_safety: true,
        } => Ok(()),
        HealthPolicy::Healthy { http: Some(http) } => {
            if http.scheme != "http"
                || !matches!(http.host.as_str(), "127.0.0.1" | "localhost" | "::1")
                || http.port == 0
                || !http.path.starts_with('/')
                || http.path.contains(char::is_control)
                || !(1..=300).contains(&http.interval_seconds)
                || !(1..=60).contains(&http.timeout_seconds)
                || !(1..=10).contains(&http.retries)
                || http.start_period_seconds > 300
            {
                return Err(DomainError::ConfigInvalid);
            }
            Ok(())
        }
        _ => Err(DomainError::ConfigInvalid),
    }
}

fn validate_mount_target_conflicts(
    files: &[ManagedFileMetadata],
    volumes: &[VolumeInput],
    binds: &[BindMountInput],
) -> Result<(), DomainError> {
    let targets: Vec<&str> = files
        .iter()
        .map(|file| file.target_path.as_str())
        .chain(volumes.iter().map(VolumeInput::target_path))
        .chain(binds.iter().map(|bind| bind.target_path.as_str()))
        .collect();
    for (index, left) in targets.iter().enumerate() {
        for right in targets.iter().skip(index + 1) {
            let left = Path::new(left);
            let right = Path::new(right);
            if left == right || left.starts_with(right) || right.starts_with(left) {
                return Err(DomainError::FileTargetConflict);
            }
        }
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), DomainError> {
    if value.chars().any(|character| character.is_control()) {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_content(value: &str) -> Result<(), DomainError> {
    if value.contains('\0') {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn canonical_non_secret(
    metadata: &ConfigMetadata,
    public_environment: &[PublicEnvInput],
    public_files: &BTreeMap<String, String>,
) -> Result<Vec<u8>, DomainError> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        metadata: &'a ConfigMetadata,
        public_environment: &'a [PublicEnvInput],
        public_files: &'a BTreeMap<String, String>,
    }
    serde_json::to_vec(&Canonical {
        metadata,
        public_environment,
        public_files,
    })
    .map_err(|_| DomainError::Internal)
}

fn hmac_hex(key: &[u8], value: &[u8]) -> Result<String, DomainError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| DomainError::Internal)?;
    mac.update(value);
    Ok(hex(&mac.finalize().into_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}

pub fn verify_config_integrity(
    metadata: &ConfigMetadata,
    public_environment: &[PublicEnvInput],
    public_files: &BTreeMap<String, String>,
    secrets: &ExistingSecrets,
    hmac_key: &[u8],
) -> Result<(), DomainError> {
    let expected_secret_env = secrets
        .environment
        .iter()
        .map(|(name, value)| Ok((name.clone(), hmac_hex(hmac_key, value.as_bytes())?)))
        .collect::<Result<BTreeMap<_, _>, DomainError>>()?;
    let expected_secret_files = secrets
        .files
        .iter()
        .map(|(name, value)| Ok((name.clone(), hmac_hex(hmac_key, value.as_bytes())?)))
        .collect::<Result<BTreeMap<_, _>, DomainError>>()?;
    let expected_public_files = public_files
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                hex(Sha256::digest(value.as_bytes()).as_slice()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if metadata.secret_hmacs != expected_secret_env
        || metadata.secret_file_hmacs != expected_secret_files
        || metadata.public_file_sha256s != expected_public_files
    {
        return Err(DomainError::ConfigInvalid);
    }
    let mut canonical_metadata = metadata.clone();
    canonical_metadata.config_sha256.clear();
    let expected_hash = hex(&Sha256::digest(canonical_non_secret(
        &canonical_metadata,
        public_environment,
        public_files,
    )?));
    if metadata.config_sha256 != expected_hash {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn volume_sort_key(value: &VolumeInput) -> String {
    match value {
        VolumeInput::Owned { logical_name, .. } => format!("0:{logical_name}"),
        VolumeInput::External { name, .. } => format!("1:{name}"),
    }
}

const fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("configuration is invalid")]
    ConfigInvalid,
    #[error("configuration quota exceeded")]
    ConfigQuotaExceeded,
    #[error("environment key is duplicated")]
    EnvDuplicate,
    #[error("secret operation is required")]
    SecretOperationRequired,
    #[error("managed file target conflicts with another mount")]
    FileTargetConflict,
    #[error("bind mounts are disabled")]
    BindDisabled,
    #[error("bind source is outside an allowed root")]
    BindOutsideAllowedRoot,
    #[error("bind source contains a symlink")]
    BindSymlink,
    #[error("bind source changed")]
    BindChanged,
    #[error("read-write bind requires acknowledgement")]
    BindRwAckRequired,
    #[error("published port conflicts with another port")]
    PortConflict,
    #[error("feature is not available")]
    FeatureNotAvailable,
    #[error("internal validation error")]
    Internal,
}

impl DomainError {
    pub const fn public_code(self) -> &'static str {
        match self {
            Self::ConfigQuotaExceeded => "CONFIG_QUOTA_EXCEEDED",
            Self::EnvDuplicate => "ENV_DUPLICATE",
            Self::SecretOperationRequired => "SECRET_OPERATION_REQUIRED",
            Self::FileTargetConflict => "FILE_TARGET_CONFLICT",
            Self::BindDisabled => "BIND_DISABLED",
            Self::BindOutsideAllowedRoot => "BIND_OUTSIDE_ALLOWED_ROOT",
            Self::BindSymlink => "BIND_SYMLINK",
            Self::BindChanged => "BIND_CHANGED",
            Self::BindRwAckRequired => "BIND_RW_ACK_REQUIRED",
            Self::PortConflict => "PORT_CONFLICT",
            Self::FeatureNotAvailable => "FEATURE_NOT_AVAILABLE",
            Self::ConfigInvalid => "CONFIG_INVALID",
            Self::Internal => "INTERNAL_ERROR",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> DraftInput {
        DraftInput {
            slug: "example".into(),
            display_name: "Example".into(),
            discovery_image_ref: "registry.example/app:latest".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: EnvironmentInput::default(),
            files: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            binds: Vec::new(),
            owned_default_network: true,
            networks: vec![NetworkInput::OwnedDefault],
            health: HealthPolicy::Running {
                stable_window_seconds: 15,
            },
        }
    }

    #[test]
    fn validates_core_bounds_and_secret_operations() {
        let normalized =
            normalize_draft(input(), &ExistingSecrets::default(), b"key", &[]).unwrap();
        assert_eq!(normalized.metadata.config_sha256.len(), 64);
        let mut invalid = input();
        invalid.discovery_image_ref = "registry.example/app@sha256:deadbeef".into();
        assert_eq!(
            normalize_draft(invalid, &ExistingSecrets::default(), b"key", &[])
                .err()
                .unwrap(),
            DomainError::ConfigInvalid
        );
        let mut secret = input();
        secret.environment.secrets.push(SecretEnvInput {
            key: "TOKEN".into(),
            operation: SecretOperation::Keep,
        });
        assert_eq!(
            normalize_draft(secret, &ExistingSecrets::default(), b"key", &[])
                .err()
                .unwrap(),
            DomainError::SecretOperationRequired
        );
    }

    #[test]
    fn detects_mount_ancestor_conflicts_and_port_duplicates() {
        let mut value = input();
        value.files.push(ManagedFileInput {
            logical_name: "config".into(),
            target_path: "/app/config".into(),
            sensitive: false,
            readonly: true,
            content: ManagedFileContent::Public(PublicFileContent {
                content: "x".into(),
            }),
        });
        value.volumes.push(VolumeInput::Owned {
            logical_name: "data".into(),
            target_path: "/app".into(),
        });
        assert_eq!(
            normalize_draft(value, &ExistingSecrets::default(), b"key", &[])
                .err()
                .unwrap(),
            DomainError::FileTargetConflict
        );
    }

    #[test]
    fn image_references_are_strict_and_canonical() {
        for value in [
            "registry.example/app",
            "registry.example/App:latest",
            "registry.example/app:one:two",
            "https://registry.example/app:latest",
            "user:password@registry.example/app:latest",
            "registry.example/app@sha256:deadbeef",
            "registry.example/app:${TAG}",
            "registry.example/app:bad tag",
        ] {
            assert!(validate_discovery_image(value).is_err(), "accepted {value}");
        }
        assert!(validate_discovery_image("registry.example:5000/team/app:v1.2").is_ok());
        assert!(
            validate_runnable_image(&format!(
                "registry.example/team/app@sha256:{}",
                "a".repeat(64)
            ))
            .is_ok()
        );
        assert!(validate_runnable_image("registry.example/app:latest").is_err());
    }

    #[test]
    fn environment_classification_changes_require_explicit_secret_operations() {
        let existing = ExistingSecrets {
            environment: BTreeMap::from([("TOKEN".into(), "old-secret".into())]),
            ..ExistingSecrets::default()
        };
        let mut to_public = input();
        to_public.environment.public.push(PublicEnvInput {
            key: "TOKEN".into(),
            value: "public".into(),
        });
        assert_eq!(
            normalize_draft(to_public.clone(), &existing, b"key", &[])
                .err()
                .unwrap(),
            DomainError::SecretOperationRequired
        );
        to_public.environment.secrets.push(SecretEnvInput {
            key: "TOKEN".into(),
            operation: SecretOperation::Delete,
        });
        let normalized = normalize_draft(to_public, &existing, b"key", &[]).unwrap();
        assert!(normalized.secret_environment.is_empty());
        assert_eq!(normalized.public_environment[0].value, "public");

        let mut duplicate_final = input();
        duplicate_final.environment.public.push(PublicEnvInput {
            key: "TOKEN".into(),
            value: "public".into(),
        });
        duplicate_final.environment.secrets.push(SecretEnvInput {
            key: "TOKEN".into(),
            operation: SecretOperation::Replace {
                value: "new-secret".into(),
            },
        });
        assert_eq!(
            normalize_draft(duplicate_final, &ExistingSecrets::default(), b"key", &[])
                .err()
                .unwrap(),
            DomainError::EnvDuplicate
        );
    }

    #[test]
    fn managed_file_sensitivity_changes_are_explicit_and_write_only() {
        let old = ManagedFileMetadata {
            logical_name: "config".into(),
            target_path: "/app/config".into(),
            sensitive: true,
            readonly: true,
        };
        let existing = ExistingSecrets {
            files: BTreeMap::from([("config".into(), "file-secret".into())]),
            file_metadata: BTreeMap::from([("config".into(), old)]),
            ..ExistingSecrets::default()
        };
        let mut value = input();
        value.files = vec![
            ManagedFileInput {
                logical_name: "config".into(),
                target_path: "/app/config".into(),
                sensitive: true,
                readonly: true,
                content: ManagedFileContent::Secret(SecretOperation::Delete),
            },
            ManagedFileInput {
                logical_name: "config".into(),
                target_path: "/app/config".into(),
                sensitive: false,
                readonly: true,
                content: ManagedFileContent::Public(PublicFileContent {
                    content: "public-file".into(),
                }),
            },
        ];
        let normalized = normalize_draft(value, &existing, b"key", &[]).unwrap();
        assert!(normalized.secret_files.is_empty());
        assert_eq!(normalized.public_files["config"], "public-file");
        let response = serde_json::to_string(&crate::domain::dto::DraftResponse {
            discovery_image_ref: normalized.discovery_image_ref,
            credential_ref: None,
            auto_deploy_enabled: false,
            poll_interval_seconds: normalized.poll_interval_seconds,
            public_environment: normalized.public_environment,
            secret_keys: normalized.metadata.secret_keys,
            files: vec![crate::domain::dto::ManagedFileResponse {
                metadata: normalized.files[0].clone(),
                content: normalized.public_files.get("config").cloned(),
            }],
            ports: normalized.ports,
            volumes: normalized.volumes,
            binds: normalized.binds,
            owned_default_network: normalized.owned_default_network,
            networks: normalized.networks,
            health: normalized.health,
        })
        .unwrap();
        assert!(!response.contains("file-secret"));
    }

    #[test]
    fn legacy_network_defaults_keep_canonical_config_bytes_and_hash() {
        let legacy_external = r#"{"kind":"external","name":"shared"}"#;
        let parsed: NetworkInput = serde_json::from_str(legacy_external).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), legacy_external);

        let mut value = input();
        value.networks.push(NetworkInput::External {
            name: "shared".into(),
            aliases: Vec::new(),
        });
        let normalized = normalize_draft(value, &ExistingSecrets::default(), b"key", &[]).unwrap();
        let metadata_json = serde_json::to_string(&normalized.metadata).unwrap();
        assert!(!metadata_json.contains("owned_default_network"));
        assert!(!metadata_json.contains("aliases"));
        let legacy_fixture = include_str!("../../tests/fixtures/legacy-config-v1.toml");
        assert_eq!(
            toml::to_string(&normalized.metadata).unwrap(),
            legacy_fixture
        );
        let loaded: ConfigMetadata = toml::from_str(legacy_fixture).unwrap();
        assert!(loaded.owned_default_network);
        assert!(matches!(
            &loaded.networks[1],
            NetworkInput::External { aliases, .. } if aliases.is_empty()
        ));
        verify_config_integrity(
            &loaded,
            &[],
            &BTreeMap::new(),
            &ExistingSecrets::default(),
            b"key",
        )
        .unwrap();
        assert_eq!(
            normalized.metadata.config_sha256,
            "739d53e83e08600fb6fa610333ffcd35e6e40e88ff97aa8bae406d43901d90c4"
        );
    }
}
