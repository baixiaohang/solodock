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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ValidationIssue {
    pub path: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub struct DraftValidationError {
    pub error: DomainError,
    pub issues: Vec<ValidationIssue>,
}

impl DraftValidationError {
    fn at(
        error: DomainError,
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            error,
            issues: vec![ValidationIssue {
                path: path.into(),
                code,
                message: message.into(),
            }],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
    pub schema_version: u32,
    #[serde(default = "default_stop_grace_period_seconds")]
    pub stop_grace_period_seconds: u16,
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
    #[serde(default)]
    pub service_discovery_enabled: bool,
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
    pub security_profile: Option<String>,
    pub display_name: String,
    pub discovery_image_ref: String,
    pub credential_ref: Option<uuid::Uuid>,
    pub auto_deploy_enabled: bool,
    pub auto_deploy_acknowledged: bool,
    pub poll_interval_seconds: u32,
    pub stop_grace_period_seconds: u16,
    pub public_environment: Vec<PublicEnvInput>,
    pub secret_environment: SecretMap,
    pub files: Vec<ManagedFileMetadata>,
    pub public_files: BTreeMap<String, String>,
    pub secret_files: SecretMap,
    pub ports: Vec<PortInput>,
    pub volumes: Vec<VolumeInput>,
    pub binds: Vec<BindMountInput>,
    pub owned_default_network: bool,
    pub service_discovery_enabled: bool,
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
    normalize_draft_with_issues(input, existing, hmac_key, allowed_bind_roots)
        .map_err(|error| error.error)
}

pub fn normalize_draft_with_issues(
    input: DraftInput,
    existing: &ExistingSecrets,
    hmac_key: &[u8],
    allowed_bind_roots: &[PathBuf],
) -> Result<NormalizedDraft, DraftValidationError> {
    normalize_draft_with_options(input, existing, hmac_key, allowed_bind_roots, true)
}

pub(crate) fn normalize_existing_draft(
    input: DraftInput,
    existing: &ExistingSecrets,
    hmac_key: &[u8],
    allowed_bind_roots: &[PathBuf],
) -> Result<NormalizedDraft, DomainError> {
    normalize_draft_with_options(input, existing, hmac_key, allowed_bind_roots, false)
        .map_err(|error| error.error)
}

fn normalize_draft_with_options(
    input: DraftInput,
    existing: &ExistingSecrets,
    hmac_key: &[u8],
    allowed_bind_roots: &[PathBuf],
    enforce_bind_plan: bool,
) -> Result<NormalizedDraft, DraftValidationError> {
    if let Some(profile) = &input.security_profile {
        if profile.is_empty()
            || profile.len() > 48
            || !profile.as_bytes()[0].is_ascii_lowercase()
            || !profile
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || profile == "unconfined"
        {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                "security_profile",
                "INVALID_VALUE",
                "Use a preinstalled profile name: 1 to 48 lowercase letters, digits or hyphens, starting with a letter",
            ));
        }
    }
    validate_display_name(&input.display_name).map_err(|error| {
        DraftValidationError::at(
            error,
            "display_name",
            "INVALID_VALUE",
            "Must be 1 to 80 characters without surrounding whitespace",
        )
    })?;
    validate_discovery_image(&input.discovery_image_ref).map_err(|error| {
        DraftValidationError::at(
            error,
            "discovery_image_ref",
            "INVALID_IMAGE_REFERENCE",
            "Must be a valid tagged OCI image reference",
        )
    })?;
    if !(60..=86_400).contains(&input.poll_interval_seconds) {
        return Err(DraftValidationError::at(
            DomainError::ConfigInvalid,
            "poll_interval_seconds",
            "OUT_OF_RANGE",
            "Must be between 60 and 86400",
        ));
    }
    let stop_grace = health_configuration_limits().stop_grace_period_seconds;
    if !(stop_grace.min..=stop_grace.max).contains(&input.stop_grace_period_seconds) {
        return Err(DraftValidationError::at(
            DomainError::ConfigInvalid,
            "stop_grace_period_seconds",
            "OUT_OF_RANGE",
            range_message(stop_grace.min, stop_grace.max),
        ));
    }

    let (public_environment, secret_environment) =
        normalize_environment(input.environment, existing, hmac_key)?;
    let (files, file_request_indexes, public_files, secret_files) =
        normalize_files(input.files, existing, hmac_key)?;
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
    let mut binds = input.binds;
    validate_binds_detailed_with_options(&binds, allowed_bind_roots, enforce_bind_plan)?;
    validate_mount_target_conflicts(&files, &file_request_indexes, &volumes, &binds)?;
    volumes.sort_by_key(volume_sort_key);
    binds.sort_by(|left, right| left.target_path.cmp(&right.target_path));
    let mut networks = input.networks;
    normalize_networks_with_issues(
        input.owned_default_network,
        input.service_discovery_enabled,
        &mut networks,
    )
    .map_err(|error| {
        DraftValidationError::at(error.error, error.path, error.code, error.message)
    })?;
    validate_health(&input.health)?;

    let public_env_keys = public_environment
        .iter()
        .map(|entry| entry.key.clone())
        .collect();
    let secret_keys = secret_environment.keys().cloned().collect();
    let secret_hmacs = secret_environment
        .iter()
        .map(|(key, value)| Ok((key.clone(), hmac_hex(hmac_key, value.expose().as_bytes())?)))
        .collect::<Result<_, DomainError>>()
        .map_err(|error| {
            DraftValidationError::at(
                error,
                "environment.secrets",
                "CONFIG_INVALID",
                "Secret integrity metadata could not be generated",
            )
        })?;
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
        .collect::<Result<_, DomainError>>()
        .map_err(|error| {
            DraftValidationError::at(
                error,
                "files",
                "CONFIG_INVALID",
                "Secret file integrity metadata could not be generated",
            )
        })?;
    let mut metadata = ConfigMetadata {
        security_profile: input.security_profile.clone(),
        schema_version: 4,
        stop_grace_period_seconds: input.stop_grace_period_seconds,
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
        service_discovery_enabled: input.service_discovery_enabled,
        networks: networks.clone(),
        health: input.health.clone(),
        config_sha256: String::new(),
    };
    let canonical =
        canonical_non_secret(&metadata, &public_environment, &public_files).map_err(|error| {
            DraftValidationError::at(
                error,
                "draft",
                "CONFIG_INVALID",
                "The normalized configuration cannot be serialized",
            )
        })?;
    let digest = Sha256::digest(canonical);
    metadata.config_sha256 = hex(&digest);
    Ok(NormalizedDraft {
        security_profile: input.security_profile.clone(),
        display_name: input.display_name.trim().to_owned(),
        discovery_image_ref: input.discovery_image_ref,
        credential_ref: input.credential_ref,
        auto_deploy_enabled: input.auto_deploy_enabled,
        auto_deploy_acknowledged: input.auto_deploy_acknowledged,
        poll_interval_seconds: input.poll_interval_seconds,
        stop_grace_period_seconds: input.stop_grace_period_seconds,
        public_environment,
        secret_environment,
        files,
        public_files,
        secret_files,
        ports,
        volumes,
        binds,
        owned_default_network: input.owned_default_network,
        service_discovery_enabled: input.service_discovery_enabled,
        networks,
        health: input.health,
        metadata,
    })
}

fn normalize_environment(
    input: EnvironmentInput,
    existing: &ExistingSecrets,
    _hmac_key: &[u8],
) -> Result<(Vec<PublicEnvInput>, SecretMap), DraftValidationError> {
    if input.public.len() + input.secrets.len() > MAX_ENV_ENTRIES {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "environment",
            "CONFIG_QUOTA_EXCEEDED",
            format!("At most {MAX_ENV_ENTRIES} environment entries are allowed"),
        ));
    }
    let mut public_keys = HashSet::new();
    let mut total = 0usize;
    let mut public = Vec::with_capacity(input.public.len());
    for (index, entry) in input.public.into_iter().enumerate() {
        validate_env_key(&entry.key).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("environment.public[{index}].key"),
                "INVALID_ENV_KEY",
                "Must be a valid environment variable name",
            )
        })?;
        validate_text(&entry.value)
            .and_then(|()| validate_env_value(&entry.value))
            .map_err(|error| {
                DraftValidationError::at(
                    error,
                    format!("environment.public[{index}].value"),
                    "INVALID_ENV_VALUE",
                    "Must not contain control characters or newlines",
                )
            })?;
        if entry.value.len() > MAX_ENV_VALUE_BYTES {
            return Err(DraftValidationError::at(
                DomainError::ConfigQuotaExceeded,
                format!("environment.public[{index}].value"),
                "CONFIG_QUOTA_EXCEEDED",
                format!("Must not exceed {MAX_ENV_VALUE_BYTES} bytes"),
            ));
        }
        if !public_keys.insert(entry.key.clone()) {
            return Err(DraftValidationError::at(
                DomainError::EnvDuplicate,
                format!("environment.public[{index}].key"),
                "ENV_DUPLICATE",
                "Environment variable names must be unique",
            ));
        }
        total = total.saturating_add(entry.value.len());
        public.push(entry);
    }
    let mut secret = BTreeMap::new();
    let mut operations = HashSet::new();
    for (index, entry) in input.secrets.into_iter().enumerate() {
        let base = format!("environment.secrets[{index}]");
        validate_env_key(&entry.key).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("{base}.key"),
                "INVALID_ENV_KEY",
                "Must be a valid environment variable name",
            )
        })?;
        if !operations.insert(entry.key.clone()) {
            return Err(DraftValidationError::at(
                DomainError::EnvDuplicate,
                format!("{base}.key"),
                "ENV_DUPLICATE",
                "Secret operations must be unique by key",
            ));
        }
        if public_keys.contains(&entry.key) && !matches!(entry.operation, SecretOperation::Delete) {
            return Err(DraftValidationError::at(
                DomainError::EnvDuplicate,
                format!("{base}.key"),
                "ENV_DUPLICATE",
                "A key cannot be public and secret at the same time",
            ));
        }
        match entry.operation {
            SecretOperation::Keep => {
                let value = existing.environment.get(&entry.key).ok_or_else(|| {
                    DraftValidationError::at(
                        DomainError::SecretOperationRequired,
                        format!("{base}.operation"),
                        "SECRET_OPERATION_REQUIRED",
                        "The stored Secret no longer exists; provide a replacement",
                    )
                })?;
                total = total.saturating_add(value.len());
                secret.insert(entry.key, SecretMaterial::new(value.clone()));
            }
            SecretOperation::Replace { value } => {
                validate_text(&value)
                    .and_then(|()| validate_env_value(&value))
                    .map_err(|error| {
                        DraftValidationError::at(
                            error,
                            format!("{base}.value"),
                            "INVALID_ENV_VALUE",
                            "Must not contain control characters or newlines",
                        )
                    })?;
                if value.len() > MAX_ENV_VALUE_BYTES {
                    return Err(DraftValidationError::at(
                        DomainError::ConfigQuotaExceeded,
                        format!("{base}.value"),
                        "CONFIG_QUOTA_EXCEEDED",
                        format!("Must not exceed {MAX_ENV_VALUE_BYTES} bytes"),
                    ));
                }
                total = total.saturating_add(value.len());
                secret.insert(entry.key, SecretMaterial::new(value));
            }
            SecretOperation::Delete => {
                if !existing.environment.contains_key(&entry.key) {
                    return Err(DraftValidationError::at(
                        DomainError::SecretOperationRequired,
                        format!("{base}.operation"),
                        "SECRET_OPERATION_REQUIRED",
                        "The stored Secret no longer exists",
                    ));
                }
            }
        }
    }
    if existing
        .environment
        .keys()
        .any(|key| !operations.contains(key))
    {
        return Err(DraftValidationError::at(
            DomainError::SecretOperationRequired,
            "environment.secrets",
            "SECRET_OPERATION_REQUIRED",
            "Every stored Secret requires an explicit keep, replace, or delete operation",
        ));
    }
    if total > MAX_ENV_TOTAL_BYTES {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "environment",
            "CONFIG_QUOTA_EXCEEDED",
            format!("Environment values must not exceed {MAX_ENV_TOTAL_BYTES} bytes in total"),
        ));
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
    Vec<usize>,
    BTreeMap<String, String>,
    SecretMap,
);

fn normalize_files(
    input: Vec<ManagedFileInput>,
    existing: &ExistingSecrets,
    _hmac_key: &[u8],
) -> Result<NormalizedFiles, DraftValidationError> {
    if input.len() > MAX_FILES {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "files",
            "CONFIG_QUOTA_EXCEEDED",
            format!("At most {MAX_FILES} managed files are allowed"),
        ));
    }
    let mut final_names = HashSet::new();
    let mut targets = HashSet::new();
    let mut secret_operations = HashSet::new();
    let mut public_names = HashSet::new();
    let mut metadata = Vec::new();
    let mut public = BTreeMap::new();
    let mut secret = BTreeMap::new();
    let mut total = 0usize;
    for (index, item) in input.into_iter().enumerate() {
        let base = format!("files[{index}]");
        validate_logical_name(&item.logical_name).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("{base}.logical_name"),
                "INVALID_FILE_NAME",
                "Must be a valid managed-file name",
            )
        })?;
        validate_container_target(&item.target_path).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("{base}.target_path"),
                "INVALID_MOUNT_TARGET",
                "Must be a safe absolute container path",
            )
        })?;
        if !item.readonly {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                format!("{base}.readonly"),
                "READONLY_REQUIRED",
                "Managed files must be read-only",
            ));
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
                {
                    return Err(DraftValidationError::at(
                        DomainError::FileTargetConflict,
                        format!("{base}.logical_name"),
                        "FILE_TARGET_CONFLICT",
                        "Managed-file names must be unique",
                    ));
                }
                if !targets.insert(descriptor.target_path.clone()) {
                    return Err(DraftValidationError::at(
                        DomainError::FileTargetConflict,
                        format!("{base}.target_path"),
                        "FILE_TARGET_CONFLICT",
                        "Managed-file targets must be unique",
                    ));
                }
                validate_content(&content).map_err(|error| {
                    DraftValidationError::at(
                        error,
                        format!("{base}.content"),
                        "INVALID_FILE_CONTENT",
                        "File content must not contain NUL bytes",
                    )
                })?;
                check_file_quota(&content, &mut total).map_err(|error| {
                    DraftValidationError::at(
                        error,
                        format!("{base}.content"),
                        "CONFIG_QUOTA_EXCEEDED",
                        format!("Managed files must not exceed {MAX_FILE_BYTES} bytes each"),
                    )
                })?;
                public.insert(item.logical_name, content);
                metadata.push((descriptor, index));
            }
            (true, ManagedFileContent::Secret(operation)) => {
                if !secret_operations.insert(item.logical_name.clone()) {
                    return Err(DraftValidationError::at(
                        DomainError::FileTargetConflict,
                        format!("{base}.logical_name"),
                        "FILE_TARGET_CONFLICT",
                        "Secret operations must be unique by file name",
                    ));
                }
                match operation {
                    SecretOperation::Keep => {
                        let value = existing.files.get(&item.logical_name).ok_or_else(|| {
                            DraftValidationError::at(
                                DomainError::SecretOperationRequired,
                                format!("{base}.content"),
                                "SECRET_OPERATION_REQUIRED",
                                "The stored Secret file no longer exists; provide a replacement",
                            )
                        })?;
                        if public_names.contains(&item.logical_name)
                            || !final_names.insert(item.logical_name.clone())
                        {
                            return Err(DraftValidationError::at(
                                DomainError::FileTargetConflict,
                                format!("{base}.logical_name"),
                                "FILE_TARGET_CONFLICT",
                                "Managed-file names must be unique",
                            ));
                        }
                        if !targets.insert(descriptor.target_path.clone()) {
                            return Err(DraftValidationError::at(
                                DomainError::FileTargetConflict,
                                format!("{base}.target_path"),
                                "FILE_TARGET_CONFLICT",
                                "Managed-file targets must be unique",
                            ));
                        }
                        check_file_quota(value, &mut total).map_err(|error| {
                            DraftValidationError::at(
                                error,
                                format!("{base}.content"),
                                "CONFIG_QUOTA_EXCEEDED",
                                format!(
                                    "Managed files must not exceed {MAX_FILE_BYTES} bytes each"
                                ),
                            )
                        })?;
                        secret.insert(item.logical_name, SecretMaterial::new(value.clone()));
                        metadata.push((descriptor, index));
                    }
                    SecretOperation::Replace { value } => {
                        if public_names.contains(&item.logical_name)
                            || !final_names.insert(item.logical_name.clone())
                        {
                            return Err(DraftValidationError::at(
                                DomainError::FileTargetConflict,
                                format!("{base}.logical_name"),
                                "FILE_TARGET_CONFLICT",
                                "Managed-file names must be unique",
                            ));
                        }
                        if !targets.insert(descriptor.target_path.clone()) {
                            return Err(DraftValidationError::at(
                                DomainError::FileTargetConflict,
                                format!("{base}.target_path"),
                                "FILE_TARGET_CONFLICT",
                                "Managed-file targets must be unique",
                            ));
                        }
                        validate_content(&value).map_err(|error| {
                            DraftValidationError::at(
                                error,
                                format!("{base}.content"),
                                "INVALID_FILE_CONTENT",
                                "File content must not contain NUL bytes",
                            )
                        })?;
                        check_file_quota(&value, &mut total).map_err(|error| {
                            DraftValidationError::at(
                                error,
                                format!("{base}.content"),
                                "CONFIG_QUOTA_EXCEEDED",
                                format!(
                                    "Managed files must not exceed {MAX_FILE_BYTES} bytes each"
                                ),
                            )
                        })?;
                        secret.insert(item.logical_name, SecretMaterial::new(value));
                        metadata.push((descriptor, index));
                    }
                    SecretOperation::Delete => {
                        if !existing.files.contains_key(&item.logical_name) {
                            return Err(DraftValidationError::at(
                                DomainError::SecretOperationRequired,
                                format!("{base}.content"),
                                "SECRET_OPERATION_REQUIRED",
                                "The stored Secret file no longer exists",
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    format!("{base}.sensitive"),
                    "FILE_SENSITIVITY_MISMATCH",
                    "File content must match its sensitivity classification",
                ));
            }
        }
    }
    if existing
        .files
        .keys()
        .any(|name| !secret_operations.contains(name))
    {
        return Err(DraftValidationError::at(
            DomainError::SecretOperationRequired,
            "files",
            "SECRET_OPERATION_REQUIRED",
            "Every stored Secret file requires an explicit keep, replace, or delete operation",
        ));
    }
    if total > MAX_FILE_TOTAL_BYTES {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "files",
            "CONFIG_QUOTA_EXCEEDED",
            format!("Managed files must not exceed {MAX_FILE_TOTAL_BYTES} bytes in total"),
        ));
    }
    metadata.sort_by(|(left, _), (right, _)| left.logical_name.cmp(&right.logical_name));
    let (metadata, request_indexes) = metadata.into_iter().unzip();
    Ok((metadata, request_indexes, public, secret))
}
fn check_file_quota(value: &str, total: &mut usize) -> Result<(), DomainError> {
    if value.len() > MAX_FILE_BYTES {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    *total = total.saturating_add(value.len());
    Ok(())
}

pub fn validate_slug(value: &str) -> Result<(), DomainError> {
    validate_slug_for_resource_schema(value, RESOURCE_NAME_SCHEMA_CURRENT)
}

pub fn validate_slug_for_resource_schema(
    value: &str,
    resource_name_schema_version: u32,
) -> Result<(), DomainError> {
    let max = match resource_name_schema_version {
        RESOURCE_NAME_SCHEMA_LEGACY => 12,
        RESOURCE_NAME_SCHEMA_CURRENT => 20,
        _ => return Err(DomainError::ConfigInvalid),
    };
    if value.is_empty()
        || value.len() > max
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

pub(crate) fn validate_logical_name(value: &str) -> Result<(), DomainError> {
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

fn validate_ports(ports: &[PortInput]) -> Result<(), DraftValidationError> {
    if ports.len() > 32 {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "ports",
            "CONFIG_QUOTA_EXCEEDED",
            "At most 32 published ports are allowed",
        ));
    }
    let mut published = HashSet::new();
    let mut normalized = HashSet::new();
    for (index, port) in ports.iter().enumerate() {
        let base = format!("ports[{index}]");
        if !matches!(port.host_ip.as_str(), "127.0.0.1" | "::1") {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                format!("{base}.host_ip"),
                "LOOPBACK_REQUIRED",
                "Host IP must be 127.0.0.1 or ::1",
            ));
        }
        if port.host_port == 0 {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                format!("{base}.host_port"),
                "OUT_OF_RANGE",
                "Host port must be between 1 and 65535",
            ));
        }
        if port.container_port == 0 {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                format!("{base}.container_port"),
                "OUT_OF_RANGE",
                "Container port must be between 1 and 65535",
            ));
        }
        if !published.insert((port.host_ip.clone(), port.host_port, port.protocol))
            || !normalized.insert((
                port.host_ip.clone(),
                port.host_port,
                port.container_port,
                port.protocol,
            ))
        {
            return Err(DraftValidationError::at(
                DomainError::PortConflict,
                format!("{base}.host_port"),
                "PORT_CONFLICT",
                "Published host IP, port, and protocol must be unique",
            ));
        }
    }
    Ok(())
}
fn validate_volumes(volumes: &[VolumeInput]) -> Result<(), DraftValidationError> {
    if volumes.len() > 16 {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "volumes",
            "CONFIG_QUOTA_EXCEEDED",
            "At most 16 volumes are allowed",
        ));
    }
    let mut names = HashSet::new();
    for (index, volume) in volumes.iter().enumerate() {
        let base = format!("volumes[{index}]");
        validate_container_target(volume.target_path()).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("{base}.target_path"),
                "INVALID_MOUNT_TARGET",
                "Must be a safe absolute container path",
            )
        })?;
        let (name, field) = match volume {
            VolumeInput::Owned { logical_name, .. } => {
                validate_logical_name(logical_name).map_err(|error| {
                    DraftValidationError::at(
                        error,
                        format!("{base}.logical_name"),
                        "INVALID_VALUE",
                        "Must be a valid managed-volume name",
                    )
                })?;
                (logical_name, "logical_name")
            }
            VolumeInput::External { name, .. } => {
                validate_docker_name(name).map_err(|error| {
                    DraftValidationError::at(
                        error,
                        format!("{base}.name"),
                        "INVALID_VALUE",
                        "Must be a valid Docker volume name",
                    )
                })?;
                (name, "name")
            }
        };
        if !names.insert(name.clone()) {
            return Err(DraftValidationError::at(
                DomainError::ConfigInvalid,
                format!("{base}.{field}"),
                "VOLUME_DUPLICATE",
                "Volume names must be unique",
            ));
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
    validate_binds_detailed(binds, allowed_roots).map_err(|error| error.error)
}

pub(crate) fn validate_existing_binds(
    binds: &[BindMountInput],
    allowed_roots: &[PathBuf],
) -> Result<Vec<BindIdentity>, DomainError> {
    validate_binds_detailed_with_options(binds, allowed_roots, false).map_err(|error| error.error)
}

fn validate_binds_detailed(
    binds: &[BindMountInput],
    allowed_roots: &[PathBuf],
) -> Result<Vec<BindIdentity>, DraftValidationError> {
    validate_binds_detailed_with_options(binds, allowed_roots, true)
}

fn validate_binds_detailed_with_options(
    binds: &[BindMountInput],
    allowed_roots: &[PathBuf],
    enforce_bind_plan: bool,
) -> Result<Vec<BindIdentity>, DraftValidationError> {
    if binds.len() > 16 {
        return Err(DraftValidationError::at(
            DomainError::ConfigQuotaExceeded,
            "binds",
            "CONFIG_QUOTA_EXCEEDED",
            "At most 16 bind mounts are allowed",
        ));
    }
    if !binds.is_empty() && allowed_roots.is_empty() {
        return Err(DraftValidationError::at(
            DomainError::BindDisabled,
            "binds",
            "BIND_DISABLED",
            "Bind mounts are disabled until an allowed root is configured",
        ));
    }
    let mut result = Vec::with_capacity(binds.len());
    for (index, bind) in binds.iter().enumerate() {
        let base = format!("binds[{index}]");
        validate_container_target(&bind.target_path).map_err(|error| {
            DraftValidationError::at(
                error,
                format!("{base}.target_path"),
                "INVALID_MOUNT_TARGET",
                "Must be a safe absolute container path",
            )
        })?;
        if !bind.readonly && !bind.acknowledge_non_rollbackable {
            return Err(DraftValidationError::at(
                DomainError::BindRwAckRequired,
                format!("{base}.acknowledge_non_rollbackable"),
                "BIND_RW_ACK_REQUIRED",
                "Confirm that read-write bind data is not rolled back with a release",
            ));
        }
        result.push(
            validate_bind_source(Path::new(&bind.source), allowed_roots).map_err(|error| {
                let (code, message) = match error {
                    DomainError::BindDisabled => (
                        "BIND_DISABLED",
                        "Bind mounts are disabled until an allowed root is configured",
                    ),
                    DomainError::BindOutsideAllowedRoot => (
                        "BIND_OUTSIDE_ALLOWED_ROOT",
                        "Must be an existing directory below an allowed bind root",
                    ),
                    DomainError::BindSymlink => (
                        "BIND_SYMLINK",
                        "Bind source path components must not be symbolic links",
                    ),
                    _ => (
                        "BIND_SOURCE_INVALID",
                        "Must be an existing accessible directory",
                    ),
                };
                DraftValidationError::at(error, format!("{base}.source"), code, message)
            })?,
        );
    }
    if enforce_bind_plan {
        let planned = result
            .iter()
            .zip(binds)
            .map(|(identity, bind)| BindSafetySource {
                identity: identity.clone(),
                readonly: bind.readonly,
            })
            .collect::<Vec<_>>();
        validate_bind_plan(&planned, &[]).map_err(|error| {
            DraftValidationError::at(
                error,
                "binds",
                "BIND_SOURCE_ANCESTOR_CONFLICT",
                "A read-write bind source must not be an ancestor of another bind source",
            )
        })?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindSafetySource {
    pub identity: BindIdentity,
    pub readonly: bool,
}

/// Validates target binds against themselves and a fresh inventory of other live applications.
/// Conflicts solely within the existing inventory do not block an unrelated target.
pub fn validate_bind_plan(
    target: &[BindSafetySource],
    existing: &[BindSafetySource],
) -> Result<(), DomainError> {
    for (index, left) in target.iter().enumerate() {
        if target[index + 1..]
            .iter()
            .any(|right| bind_sources_conflict(left, right))
            || existing
                .iter()
                .any(|right| bind_sources_conflict(left, right))
        {
            return Err(DomainError::BindSourceAncestorConflict);
        }
    }
    Ok(())
}

fn bind_sources_conflict(left: &BindSafetySource, right: &BindSafetySource) -> bool {
    (!left.readonly
        && left.identity.path != right.identity.path
        && right.identity.path.starts_with(&left.identity.path))
        || (!right.readonly
            && left.identity.path != right.identity.path
            && left.identity.path.starts_with(&right.identity.path))
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

fn validate_health(health: &HealthPolicy) -> Result<(), DraftValidationError> {
    let limits = health_configuration_limits();
    match health {
        HealthPolicy::Healthy { http: None } | HealthPolicy::Completed => Ok(()),
        HealthPolicy::Running {
            stable_window_seconds,
        } => {
            let limit = limits.running_stable_window_seconds;
            if !(limit.min..=limit.max).contains(stable_window_seconds) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.stable_window_seconds",
                    "OUT_OF_RANGE",
                    range_message(limit.min, limit.max),
                ));
            }
            Ok(())
        }
        HealthPolicy::Disabled {
            acknowledge_reduced_safety,
        } => {
            if !acknowledge_reduced_safety {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.acknowledge_reduced_safety",
                    "ACK_REQUIRED",
                    "Confirm reduced safety before disabling health checks",
                ));
            }
            Ok(())
        }
        HealthPolicy::Healthy { http: Some(http) } => {
            if http.scheme != "http" {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.scheme",
                    "INVALID_VALUE",
                    "Only http is supported",
                ));
            }
            if !matches!(http.host.as_str(), "127.0.0.1" | "localhost" | "::1") {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.host",
                    "INVALID_VALUE",
                    "Only loopback health hosts are allowed",
                ));
            }
            if http.port == 0 {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.port",
                    "OUT_OF_RANGE",
                    "Must be between 1 and 65535",
                ));
            }
            if !http.path.starts_with('/') || http.path.contains(char::is_control) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.path",
                    "INVALID_VALUE",
                    "Must be an absolute HTTP path without control characters",
                ));
            }
            let limit = limits.http_interval_seconds;
            if !(limit.min..=limit.max).contains(&http.interval_seconds) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.interval_seconds",
                    "OUT_OF_RANGE",
                    range_message(limit.min, limit.max),
                ));
            }
            let limit = limits.http_timeout_seconds;
            if !(limit.min..=limit.max).contains(&http.timeout_seconds) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.timeout_seconds",
                    "OUT_OF_RANGE",
                    range_message(limit.min, limit.max),
                ));
            }
            let limit = limits.http_retries;
            if !(limit.min..=limit.max).contains(&http.retries) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.retries",
                    "OUT_OF_RANGE",
                    range_message(limit.min, limit.max),
                ));
            }
            let limit = limits.http_start_period_seconds;
            if !(limit.min..=limit.max).contains(&http.start_period_seconds) {
                return Err(DraftValidationError::at(
                    DomainError::ConfigInvalid,
                    "health.http.start_period_seconds",
                    "OUT_OF_RANGE",
                    range_message(limit.min, limit.max),
                ));
            }
            Ok(())
        }
    }
}

fn range_message<T: std::fmt::Display>(min: T, max: T) -> String {
    format!("Must be between {min} and {max}")
}
fn validate_mount_target_conflicts(
    files: &[ManagedFileMetadata],
    file_request_indexes: &[usize],
    volumes: &[VolumeInput],
    binds: &[BindMountInput],
) -> Result<(), DraftValidationError> {
    let targets = files
        .iter()
        .zip(file_request_indexes)
        .map(|(file, request_index)| {
            (
                format!("files[{request_index}].target_path"),
                file.target_path.as_str(),
            )
        })
        .chain(volumes.iter().enumerate().map(|(index, volume)| {
            (
                format!("volumes[{index}].target_path"),
                volume.target_path(),
            )
        }))
        .chain(binds.iter().enumerate().map(|(index, bind)| {
            (
                format!("binds[{index}].target_path"),
                bind.target_path.as_str(),
            )
        }))
        .collect::<Vec<_>>();
    for (index, (_, left)) in targets.iter().enumerate() {
        for (path, right) in targets.iter().skip(index + 1) {
            let left = Path::new(left);
            let right = Path::new(right);
            if left == right || left.starts_with(right) || right.starts_with(left) {
                return Err(DraftValidationError::at(
                    DomainError::FileTargetConflict,
                    path.clone(),
                    "FILE_TARGET_CONFLICT",
                    "Mount targets must not overlap",
                ));
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
    if metadata.schema_version < 4 && metadata.security_profile.is_some() {
        return Err(DomainError::ConfigInvalid);
    }
    if metadata.schema_version == 1 {
        #[derive(Serialize)]
        struct LegacyConfigMetadata<'a> {
            schema_version: u32,
            public_env_keys: &'a [String],
            secret_keys: &'a [String],
            secret_hmacs: &'a BTreeMap<String, String>,
            files: &'a [ManagedFileMetadata],
            public_file_sha256s: &'a BTreeMap<String, String>,
            secret_file_hmacs: &'a BTreeMap<String, String>,
            ports: &'a [PortInput],
            volumes: &'a [VolumeInput],
            binds: &'a [BindMountInput],
            #[serde(skip_serializing_if = "is_true")]
            owned_default_network: &'a bool,
            networks: &'a [NetworkInput],
            health: &'a HealthPolicy,
            config_sha256: &'a str,
        }
        #[derive(Serialize)]
        struct LegacyCanonical<'a> {
            metadata: LegacyConfigMetadata<'a>,
            public_environment: &'a [PublicEnvInput],
            public_files: &'a BTreeMap<String, String>,
        }
        return serde_json::to_vec(&LegacyCanonical {
            metadata: LegacyConfigMetadata {
                schema_version: metadata.schema_version,
                public_env_keys: &metadata.public_env_keys,
                secret_keys: &metadata.secret_keys,
                secret_hmacs: &metadata.secret_hmacs,
                files: &metadata.files,
                public_file_sha256s: &metadata.public_file_sha256s,
                secret_file_hmacs: &metadata.secret_file_hmacs,
                ports: &metadata.ports,
                volumes: &metadata.volumes,
                binds: &metadata.binds,
                owned_default_network: &metadata.owned_default_network,
                networks: &metadata.networks,
                health: &metadata.health,
                config_sha256: &metadata.config_sha256,
            },
            public_environment,
            public_files,
        })
        .map_err(|_| DomainError::Internal);
    }
    if metadata.schema_version == 2 {
        #[derive(Serialize)]
        struct Schema2Metadata<'a> {
            schema_version: u32,
            stop_grace_period_seconds: u16,
            public_env_keys: &'a [String],
            secret_keys: &'a [String],
            secret_hmacs: &'a BTreeMap<String, String>,
            files: &'a [ManagedFileMetadata],
            public_file_sha256s: &'a BTreeMap<String, String>,
            secret_file_hmacs: &'a BTreeMap<String, String>,
            ports: &'a [PortInput],
            volumes: &'a [VolumeInput],
            binds: &'a [BindMountInput],
            #[serde(skip_serializing_if = "is_true")]
            owned_default_network: &'a bool,
            networks: &'a [NetworkInput],
            health: &'a HealthPolicy,
            config_sha256: &'a str,
        }
        #[derive(Serialize)]
        struct Schema2Canonical<'a> {
            metadata: Schema2Metadata<'a>,
            public_environment: &'a [PublicEnvInput],
            public_files: &'a BTreeMap<String, String>,
        }
        return serde_json::to_vec(&Schema2Canonical {
            metadata: Schema2Metadata {
                schema_version: metadata.schema_version,
                stop_grace_period_seconds: metadata.stop_grace_period_seconds,
                public_env_keys: &metadata.public_env_keys,
                secret_keys: &metadata.secret_keys,
                secret_hmacs: &metadata.secret_hmacs,
                files: &metadata.files,
                public_file_sha256s: &metadata.public_file_sha256s,
                secret_file_hmacs: &metadata.secret_file_hmacs,
                ports: &metadata.ports,
                volumes: &metadata.volumes,
                binds: &metadata.binds,
                owned_default_network: &metadata.owned_default_network,
                networks: &metadata.networks,
                health: &metadata.health,
                config_sha256: &metadata.config_sha256,
            },
            public_environment,
            public_files,
        })
        .map_err(|_| DomainError::Internal);
    }
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

#[cfg(test)]
pub(crate) fn recompute_config_hash_for_test(
    metadata: &ConfigMetadata,
    public_environment: &[PublicEnvInput],
    public_files: &BTreeMap<String, String>,
) -> String {
    let mut metadata = metadata.clone();
    metadata.config_sha256.clear();
    hex(&Sha256::digest(
        canonical_non_secret(&metadata, public_environment, public_files).unwrap(),
    ))
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
    #[error("read-write bind source is an ancestor of another bind source")]
    BindSourceAncestorConflict,
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
            Self::BindSourceAncestorConflict => "BIND_SOURCE_ANCESTOR_CONFLICT",
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

    #[test]
    fn security_profile_is_optional_validated_and_integrity_protected() {
        let base = normalize_draft(input(), &ExistingSecrets::default(), b"key", &[]).unwrap();
        let mut selected = input();
        selected.security_profile = Some("codex-v1".into());
        let normalized =
            normalize_draft(selected, &ExistingSecrets::default(), b"key", &[]).unwrap();
        assert_eq!(
            normalized.metadata.security_profile.as_deref(),
            Some("codex-v1")
        );
        assert_ne!(
            normalized.metadata.config_sha256,
            base.metadata.config_sha256
        );
        for invalid in [
            "",
            "../codex",
            "unconfined",
            "a/b",
            "a=b",
            "a\nprivileged",
            "-a",
            "A",
        ] {
            let mut draft = input();
            draft.security_profile = Some(invalid.into());
            assert!(normalize_draft(draft, &ExistingSecrets::default(), b"key", &[]).is_err());
        }
        for version in 1..=3 {
            let mut legacy = normalized.metadata.clone();
            legacy.schema_version = version;
            assert!(canonical_non_secret(&legacy, &[], &BTreeMap::new()).is_err());
        }
    }

    fn input() -> DraftInput {
        DraftInput {
            security_profile: None,
            display_name: "Example".into(),
            discovery_image_ref: "registry.example/app:latest".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            stop_grace_period_seconds: default_stop_grace_period_seconds(),
            environment: EnvironmentInput::default(),
            files: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            binds: Vec::new(),
            owned_default_network: true,
            service_discovery_enabled: true,
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
        assert_eq!(normalized.stop_grace_period_seconds, 10);
        for stop_grace_period_seconds in [0, 601] {
            let mut invalid = input();
            invalid.stop_grace_period_seconds = stop_grace_period_seconds;
            assert!(matches!(
                normalize_draft(invalid, &ExistingSecrets::default(), b"key", &[]),
                Err(DomainError::ConfigInvalid)
            ));
        }
        for stop_grace_period_seconds in [1, 600] {
            let mut valid = input();
            valid.stop_grace_period_seconds = stop_grace_period_seconds;
            assert_eq!(
                normalize_draft(valid, &ExistingSecrets::default(), b"key", &[])
                    .unwrap()
                    .stop_grace_period_seconds,
                stop_grace_period_seconds
            );
        }
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
    fn health_capabilities_are_the_validation_source_of_truth() {
        let limits = health_configuration_limits();
        assert_eq!(limits.running_stable_window_seconds.min, 5);
        assert_eq!(limits.running_stable_window_seconds.max, 300);
        assert_eq!(limits.http_interval_seconds.max, 300);
        assert_eq!(limits.http_timeout_seconds.max, 60);
        assert_eq!(limits.http_retries.max, 10);
        assert_eq!(limits.http_start_period_seconds.max, 300);
        assert_eq!(limits.stop_grace_period_seconds.max, 600);

        let mut invalid = input();
        invalid.health = HealthPolicy::Healthy {
            http: Some(HttpHealthcheck {
                client: HttpClient::Curl,
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port: 3000,
                path: "/readyz".into(),
                interval_seconds: limits.http_interval_seconds.default,
                timeout_seconds: limits.http_timeout_seconds.default,
                retries: limits.http_retries.max + 1,
                start_period_seconds: limits.http_start_period_seconds.default,
            }),
        };
        let error = normalize_draft_with_issues(invalid, &ExistingSecrets::default(), b"key", &[])
            .err()
            .unwrap();
        assert_eq!(error.error, DomainError::ConfigInvalid);
        assert!(error.issues.iter().any(|issue| {
            issue.path == "health.http.retries"
                && issue.code == "OUT_OF_RANGE"
                && issue.message == "Must be between 1 and 10"
        }));
    }

    #[test]
    fn detailed_validation_locates_safe_field_issues() {
        let directory = tempfile::tempdir().unwrap();
        let bind_root = directory.path().join("allowed");
        let bind_source = bind_root.join("app");
        std::fs::create_dir_all(&bind_source).unwrap();

        let mut invalid = input();
        invalid.environment.public = vec![
            PublicEnvInput {
                key: "TOKEN".into(),
                value: "public-canary-one".into(),
            },
            PublicEnvInput {
                key: "TOKEN".into(),
                value: "public-canary-two".into(),
            },
        ];
        invalid.ports = vec![
            PortInput {
                host_ip: "127.0.0.1".into(),
                host_port: 3000,
                container_port: 3000,
                protocol: PortProtocol::Tcp,
            },
            PortInput {
                host_ip: "127.0.0.1".into(),
                host_port: 3000,
                container_port: 4000,
                protocol: PortProtocol::Tcp,
            },
        ];
        invalid.volumes.push(VolumeInput::Owned {
            logical_name: "data".into(),
            target_path: "/app/data".into(),
        });
        invalid.binds.push(BindMountInput {
            source: bind_source.to_string_lossy().into_owned(),
            target_path: "/app/data/cache".into(),
            readonly: false,
            acknowledge_non_rollbackable: false,
        });
        let error = normalize_draft_with_issues(
            invalid,
            &ExistingSecrets::default(),
            b"key",
            std::slice::from_ref(&bind_root),
        )
        .err()
        .unwrap();
        let paths = error
            .issues
            .iter()
            .map(|issue| issue.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"environment.public[1].key"));
        assert_eq!(paths, ["environment.public[1].key"]);
        let serialized = serde_json::to_string(&error.issues).unwrap();
        assert!(!serialized.contains("public-canary"));
        assert!(!serialized.contains(bind_source.to_string_lossy().as_ref()));
    }

    #[test]
    fn bind_plan_rejects_only_strict_read_write_ancestors() {
        let identity = |path: &str| BindIdentity {
            path: PathBuf::from(path),
            #[cfg(unix)]
            device: 1,
            #[cfg(unix)]
            inode: 1,
        };
        let source = |path: &str, readonly: bool| BindSafetySource {
            identity: identity(path),
            readonly,
        };

        assert_eq!(
            validate_bind_plan(
                &[source("/srv/data", false), source("/srv/data/child", true)],
                &[],
            ),
            Err(DomainError::BindSourceAncestorConflict)
        );
        assert_eq!(
            validate_bind_plan(
                &[source("/srv/data/child", true)],
                &[source("/srv/data", false)],
            ),
            Err(DomainError::BindSourceAncestorConflict)
        );
        assert!(
            validate_bind_plan(
                &[source("/srv/data", true), source("/srv/data/child", false)],
                &[],
            )
            .is_ok()
        );
        assert!(
            validate_bind_plan(
                &[source("/srv/data", false), source("/srv/data", true)],
                &[],
            )
            .is_ok()
        );
        assert!(
            validate_bind_plan(
                &[source("/srv/data/left", false)],
                &[source("/srv/data/right", true)],
            )
            .is_ok()
        );
    }

    #[test]
    fn bind_plan_uses_canonical_sources_and_rejects_symlink_aliases() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let parent = root.path().join("parent");
        let child = parent.join("child");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
        let binds = vec![
            BindMountInput {
                source: parent.display().to_string(),
                target_path: "/parent".into(),
                readonly: false,
                acknowledge_non_rollbackable: true,
            },
            BindMountInput {
                source: child.display().to_string(),
                target_path: "/child".into(),
                readonly: true,
                acknowledge_non_rollbackable: false,
            },
        ];
        assert_eq!(
            validate_binds(&binds, &[root.path().to_path_buf()]).unwrap_err(),
            DomainError::BindSourceAncestorConflict
        );

        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&child, &alias).unwrap();
        let aliased = [BindMountInput {
            source: alias.display().to_string(),
            target_path: "/alias".into(),
            readonly: true,
            acknowledge_non_rollbackable: false,
        }];
        assert_eq!(
            validate_binds(&aliased, &[root.path().to_path_buf()]).unwrap_err(),
            DomainError::BindSymlink
        );
    }

    #[test]
    fn detailed_validation_uses_variant_and_row_specific_paths() {
        let mut owned = input();
        owned.volumes = vec![VolumeInput::Owned {
            logical_name: "INVALID!".into(),
            target_path: "/data".into(),
        }];
        let error =
            match normalize_draft_with_issues(owned, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("invalid managed volume was accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "volumes[0].logical_name");

        let mut external = input();
        external.volumes = vec![VolumeInput::External {
            name: "INVALID!".into(),
            target_path: "/data".into(),
        }];
        let error =
            match normalize_draft_with_issues(external, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("invalid external volume was accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "volumes[0].name");

        let mut network = input();
        network.networks = vec![NetworkInput::External {
            name: "INVALID!".into(),
            aliases: Vec::new(),
        }];
        let error =
            match normalize_draft_with_issues(network, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("invalid external network was accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "networks[0].name");

        let mut alias = input();
        alias.networks = vec![NetworkInput::External {
            name: "shared".into(),
            aliases: vec!["INVALID!".into()],
        }];
        let error =
            match normalize_draft_with_issues(alias, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("invalid network alias was accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "networks[0].aliases[0]");
    }

    #[test]
    fn mount_conflict_paths_use_the_original_request_index() {
        let mut value = input();
        value.files.push(ManagedFileInput {
            logical_name: "config".into(),
            target_path: "/app/config".into(),
            sensitive: false,
            readonly: true,
            content: ManagedFileContent::Public(PublicFileContent {
                content: "public-file".into(),
            }),
        });
        value.volumes = vec![
            VolumeInput::Owned {
                logical_name: "z-data".into(),
                target_path: "/safe".into(),
            },
            VolumeInput::Owned {
                logical_name: "a-data".into(),
                target_path: "/app".into(),
            },
        ];

        let error =
            match normalize_draft_with_issues(value, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("overlapping mount target was accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "volumes[1].target_path");
    }

    #[test]
    fn managed_file_overlap_uses_the_exact_request_row() {
        let mut value = input();
        value.files = vec![
            ManagedFileInput {
                logical_name: "config".into(),
                target_path: "/etc/app".into(),
                sensitive: false,
                readonly: true,
                content: ManagedFileContent::Public(PublicFileContent {
                    content: "root".into(),
                }),
            },
            ManagedFileInput {
                logical_name: "settings".into(),
                target_path: "/etc/app/config.json".into(),
                sensitive: false,
                readonly: true,
                content: ManagedFileContent::Public(PublicFileContent {
                    content: "nested".into(),
                }),
            },
        ];

        let error =
            match normalize_draft_with_issues(value, &ExistingSecrets::default(), b"key", &[]) {
                Ok(_) => panic!("overlapping managed-file targets were accepted"),
                Err(error) => error,
            };
        assert_eq!(error.issues[0].path, "files[1].target_path");
    }

    #[test]
    fn slug_is_short_ascii_and_stable_resource_safe() {
        for valid in ["a", "app-1", "abcdefghijkl", "abcdefghijklmnopqrst"] {
            assert!(validate_slug(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "-app",
            "app-",
            "App",
            "app_name",
            "abcdefghijklmnopqrstu",
        ] {
            assert_eq!(validate_slug(invalid), Err(DomainError::ConfigInvalid));
        }
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
            security_profile: None,
            discovery_image_ref: normalized.discovery_image_ref,
            credential_ref: None,
            auto_deploy_enabled: false,
            poll_interval_seconds: normalized.poll_interval_seconds,
            stop_grace_period_seconds: normalized.stop_grace_period_seconds,
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
            service_discovery_enabled: normalized.service_discovery_enabled,
            networks: normalized.networks,
            health: normalized.health,
        })
        .unwrap();
        assert!(!response.contains("file-secret"));
    }

    #[test]
    fn valid_secret_to_public_file_conversion_does_not_create_false_issues() {
        let existing = ExistingSecrets {
            files: BTreeMap::from([("config".into(), "file-secret".into())]),
            file_metadata: BTreeMap::from([(
                "config".into(),
                ManagedFileMetadata {
                    logical_name: "config".into(),
                    target_path: "/app/config".into(),
                    sensitive: true,
                    readonly: true,
                },
            )]),
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
        value.health = HealthPolicy::Healthy {
            http: Some(HttpHealthcheck {
                client: HttpClient::Curl,
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port: 3000,
                path: "/readyz".into(),
                interval_seconds: 10,
                timeout_seconds: 5,
                retries: 11,
                start_period_seconds: 30,
            }),
        };

        let error = match normalize_draft_with_issues(value, &existing, b"key", &[]) {
            Ok(_) => panic!("invalid health configuration was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.issues.len(), 1);
        assert_eq!(error.issues[0].path, "health.http.retries");
    }

    #[test]
    fn legacy_network_defaults_keep_existing_hash_while_new_revisions_record_stop_grace() {
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
        assert!(metadata_json.contains("stop_grace_period_seconds"));
        let legacy_fixture = include_str!("../../tests/fixtures/legacy-config-v1.toml");
        let loaded: ConfigMetadata = toml::from_str(legacy_fixture).unwrap();
        assert_eq!(loaded.stop_grace_period_seconds, 10);
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
        assert_ne!(
            normalized.metadata.config_sha256,
            "739d53e83e08600fb6fa610333ffcd35e6e40e88ff97aa8bae406d43901d90c4"
        );
    }
}
