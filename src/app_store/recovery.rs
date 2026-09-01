use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{StoreError, atomic::is_internal_temp_name};
use crate::{
    compose::{ComposeInput, generate},
    domain::{
        DesiredState, NetworkPlan, dto::DraftResponse, network_plan, validate_runnable_image,
    },
    security::permissions::{
        MANAGED_FILE_MODE, PermissionError, check_private, check_service_owned_file_mode,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredApp {
    pub app_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub resource_name_schema_version: u32,
    pub project_name: String,
    pub active_release_id: Option<Uuid>,
    pub active_image_ref: Option<String>,
    pub active_config_revision: Option<Uuid>,
    pub active_config_sha256: Option<String>,
    pub active_network_plan: Option<NetworkPlan>,
    pub pending_release_id: Option<Uuid>,
    pub pending_image_ref: Option<String>,
    pub pending_config_revision: Option<Uuid>,
    pub pending_network_plan: Option<NetworkPlan>,
    pub discovery_image_ref: Option<String>,
    pub draft_revision: Option<Uuid>,
    pub draft_config_sha256: Option<String>,
    pub desired_state: DesiredState,
    pub auto_deploy_enabled: bool,
    pub poll_interval_seconds: u32,
    pub last_operation_id: Option<Uuid>,
    pub draft: Option<DraftResponse>,
    pub source_updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryIssue {
    pub code: &'static str,
    pub app_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
    pub valid_apps: Vec<RecoveredApp>,
    pub issues: Vec<RecoveryIssue>,
}

type AppHeader = crate::domain::AppMetadata;

#[derive(Debug, Deserialize)]
struct AppSchemaHeader {
    schema_version: u32,
}

#[derive(Debug, Deserialize)]
struct ReleaseHeader {
    schema_version: u32,
    #[serde(default)]
    compose_schema_version: u32,
    id: Uuid,
    app_id: Uuid,
    runnable_image_ref: String,
    #[serde(default)]
    config_revision: Option<Uuid>,
    #[serde(default)]
    config_sha256: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

struct ValidatedReleaseLink {
    release_id: Uuid,
    release_directory: PathBuf,
}

pub fn scan(apps_directory: &Path) -> Result<RecoveryReport, StoreError> {
    scan_with_key(apps_directory, None)
}

pub fn scan_with_key(
    apps_directory: &Path,
    integrity_key: Option<&[u8]>,
) -> Result<RecoveryReport, StoreError> {
    scan_with_options(apps_directory, integrity_key, &[])
}

pub fn scan_with_options(
    apps_directory: &Path,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
) -> Result<RecoveryReport, StoreError> {
    scan_with_mode(
        apps_directory,
        integrity_key,
        allowed_bind_roots,
        ScanMode::StartupCleanup,
        None,
    )
}

pub fn scan_read_only_with_options(
    apps_directory: &Path,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
) -> Result<RecoveryReport, StoreError> {
    scan_with_mode(
        apps_directory,
        integrity_key,
        allowed_bind_roots,
        ScanMode::ReadOnly,
        None,
    )
}

pub fn scan_read_only_relocated(
    apps_directory: &Path,
    canonical_apps_directory: &Path,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
) -> Result<RecoveryReport, StoreError> {
    scan_with_mode(
        apps_directory,
        integrity_key,
        allowed_bind_roots,
        ScanMode::ReadOnly,
        Some(canonical_apps_directory),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    StartupCleanup,
    ReadOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedTreePolicy {
    Strict,
    DisposableAppTemp,
}

fn scan_with_mode(
    apps_directory: &Path,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
    mode: ScanMode,
    canonical_apps_directory: Option<&Path>,
) -> Result<RecoveryReport, StoreError> {
    check_private(apps_directory, true)?;
    let mut report = RecoveryReport::default();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(apps_directory)? {
        let entry = entry?;
        if entry.file_name() == std::ffi::OsStr::new(".trash") {
            check_private(&entry.path(), true)?;
            continue;
        }
        if is_app_temp_name(&entry.file_name()) {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            check_private(&entry.path(), true)?;
            validate_managed_tree(
                &entry.path(),
                &entry.path(),
                ManagedTreePolicy::DisposableAppTemp,
            )?;
            if mode == ScanMode::StartupCleanup {
                fs::remove_dir_all(entry.path())?;
                super::sync_directory(apps_directory)?;
            }
            report.issues.push(RecoveryIssue {
                code: if mode == ScanMode::StartupCleanup {
                    "TEMP_APP_REMOVED"
                } else {
                    "TEMP_ARTIFACT_IGNORED"
                },
                app_id: None,
            });
            continue;
        }
        if is_internal_temp_name(&entry.file_name()) {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(StoreError::SymlinkBoundary);
            }
            if file_type.is_dir() {
                check_private(&entry.path(), true)?;
                validate_managed_tree(
                    &entry.path(),
                    &entry.path(),
                    ManagedTreePolicy::DisposableAppTemp,
                )?;
            } else if file_type.is_file() {
                check_private(&entry.path(), false)?;
            } else {
                return Err(StoreError::SymlinkBoundary);
            }
            report.issues.push(RecoveryIssue {
                code: "TEMP_ARTIFACT_IGNORED",
                app_id: None,
            });
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(StoreError::SymlinkBoundary);
        }
        if !file_type.is_dir() {
            report.issues.push(RecoveryIssue {
                code: "UNRECOGNIZED_APP_ENTRY",
                app_id: None,
            });
            continue;
        }
        let directory_name = entry.file_name();
        let directory_id = match directory_name
            .to_str()
            .and_then(|name| name.parse::<Uuid>().ok())
        {
            Some(id) => id,
            None => {
                report.issues.push(RecoveryIssue {
                    code: "APP_DIRECTORY_ID_INVALID",
                    app_id: None,
                });
                continue;
            }
        };
        if directory_name.as_os_str() != std::ffi::OsStr::new(&directory_id.to_string()) {
            report.issues.push(RecoveryIssue {
                code: "APP_DIRECTORY_ID_NON_CANONICAL",
                app_id: Some(directory_id),
            });
            continue;
        }
        check_private(&entry.path(), true)?;
        if validate_managed_tree(&entry.path(), &entry.path(), ManagedTreePolicy::Strict)? > 0 {
            report.issues.push(RecoveryIssue {
                code: "TEMP_ARTIFACT_IGNORED",
                app_id: Some(directory_id),
            });
        }
        if mode == ScanMode::StartupCleanup {
            cleanup_revision_temps(&entry.path())?;
        }
        let active = validate_release_link_path(&entry.path(), "active")?;
        let pending = validate_release_link_path(&entry.path(), "pending")?;
        let active = match active {
            Ok(active) => active,
            Err(code) => {
                report.issues.push(RecoveryIssue {
                    code,
                    app_id: Some(directory_id),
                });
                continue;
            }
        };
        let pending = match pending {
            Ok(pending) => pending,
            Err(code) => {
                report.issues.push(RecoveryIssue {
                    code,
                    app_id: Some(directory_id),
                });
                continue;
            }
        };
        let recovered = scan_app(
            &entry.path(),
            &canonical_apps_directory
                .map(|directory| directory.join(directory_id.to_string()))
                .unwrap_or_else(|| entry.path()),
            directory_id,
            active.as_ref(),
            pending.as_ref(),
            integrity_key,
            allowed_bind_roots,
            mode,
        )?;
        match recovered {
            Ok(app) => {
                if let Some(key) = integrity_key {
                    match crate::webhook::store::validate_app_directory(
                        apps_directory,
                        &entry.path(),
                        directory_id,
                        key,
                    ) {
                        Ok(_) => {}
                        Err(StoreError::ContentInvalid) => {
                            report.issues.push(RecoveryIssue {
                                code: "WEBHOOK_CONFIG_INVALID",
                                app_id: Some(directory_id),
                            });
                        }
                        Err(StoreError::Permission(PermissionError::UnexpectedType(_))) => {
                            return Err(StoreError::SymlinkBoundary);
                        }
                        Err(StoreError::Permission(
                            error @ (PermissionError::Mode(_) | PermissionError::Owner(_)),
                        )) => return Err(StoreError::Permission(error)),
                        Err(StoreError::Permission(PermissionError::Io(error)))
                        | Err(StoreError::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
                            ) =>
                        {
                            report.issues.push(RecoveryIssue {
                                code: "WEBHOOK_CONFIG_INVALID",
                                app_id: Some(directory_id),
                            });
                        }
                        Err(error) => return Err(error),
                    }
                }
                candidates.push(app);
            }
            Err(code) => report.issues.push(RecoveryIssue {
                code,
                app_id: Some(directory_id),
            }),
        }
    }

    let mut slug_counts = HashMap::new();
    for app in &candidates {
        *slug_counts.entry(app.slug.clone()).or_insert(0usize) += 1;
    }
    let mut duplicate_reported = HashSet::new();
    for app in candidates {
        if slug_counts[&app.slug] > 1 {
            if duplicate_reported.insert(app.app_id) {
                report.issues.push(RecoveryIssue {
                    code: "APP_SLUG_DUPLICATE",
                    app_id: Some(app.app_id),
                });
            }
        } else {
            report.valid_apps.push(app);
        }
    }
    report.valid_apps.sort_by_key(|app| app.app_id);
    Ok(report)
}

fn is_app_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(value) = name.to_str() else {
        return false;
    };
    let Some(suffix) = value.strip_prefix(".solodock-tmp-app-") else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_revision_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(value) = name.to_str() else {
        return false;
    };
    let Some(suffix) = value.strip_prefix(".solodock-config-tmp-") else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn cleanup_revision_temps(app_directory: &Path) -> Result<(), StoreError> {
    let revisions = app_directory.join("config-revisions");
    let entries = match fs::read_dir(&revisions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !is_revision_temp_name(&entry.file_name()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(StoreError::SymlinkBoundary);
        }
        check_private(&entry.path(), true)?;
        fs::remove_dir_all(entry.path())?;
    }
    super::sync_directory(&revisions)
}

fn validate_managed_tree(
    tree_root: &Path,
    app_directory: &Path,
    policy: ManagedTreePolicy,
) -> Result<usize, StoreError> {
    let mut pending = vec![tree_root.to_owned()];
    let mut temporary_artifacts = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let is_release_link = directory == tree_root
                && matches!(entry.file_name().to_str(), Some("active" | "pending"));
            let is_webhook_artifact = directory == tree_root
                && matches!(
                    entry.file_name().to_str(),
                    Some("webhook.toml" | "webhook-secret-revisions")
                );
            if file_type.is_symlink() {
                if is_release_link {
                    continue;
                }
                return Err(StoreError::SymlinkBoundary);
            }
            // Webhook state has its own complete validator and is an optional
            // fail-closed substate. Deferring it keeps mode/HMAC/content
            // damage from removing the otherwise valid app from the catalog.
            if is_webhook_artifact {
                continue;
            }
            if file_type.is_dir() {
                check_private(&path, true)?;
                pending.push(path);
            } else if file_type.is_file() {
                match (
                    policy,
                    super::config_revision::managed_file_path_kind(app_directory, &path),
                ) {
                    (ManagedTreePolicy::DisposableAppTemp, Some(_)) => {
                        check_allowed_managed_file_modes(
                            &path,
                            &[MANAGED_FILE_MODE, 0o400, 0o600],
                        )?;
                    }
                    (
                        ManagedTreePolicy::Strict,
                        Some(super::config_revision::ManagedFilePathKind::Canonical),
                    ) => check_allowed_managed_file_modes(&path, &[MANAGED_FILE_MODE])?,
                    (
                        ManagedTreePolicy::Strict,
                        Some(super::config_revision::ManagedFilePathKind::Temporary),
                    ) => check_allowed_managed_file_modes(&path, &[MANAGED_FILE_MODE, 0o600])?,
                    (_, None) => check_private(&path, false)?,
                }
            } else {
                return Err(StoreError::SymlinkBoundary);
            }
            if is_internal_temp_name(&entry.file_name()) {
                temporary_artifacts += 1;
            }
        }
    }
    Ok(temporary_artifacts)
}

fn check_allowed_managed_file_modes(path: &Path, allowed: &[u32]) -> Result<(), StoreError> {
    if allowed
        .iter()
        .any(|mode| check_service_owned_file_mode(path, *mode).is_ok())
    {
        Ok(())
    } else {
        Err(StoreError::ManagedFilePermissionInvalid)
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_app(
    path: &Path,
    canonical_app_directory: &Path,
    directory_id: Uuid,
    active: Option<&ValidatedReleaseLink>,
    pending: Option<&ValidatedReleaseLink>,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
    mode: ScanMode,
) -> Result<Result<RecoveredApp, &'static str>, StoreError> {
    let app_path = path.join("app.toml");
    let contents = match read_toml_contents(&app_path, "APP_HEADER_MISSING", "APP_HEADER_INVALID")?
    {
        Ok(contents) => contents,
        Err(code) => return Ok(Err(code)),
    };
    let schema: AppSchemaHeader = match toml::from_str(&contents) {
        Ok(schema) => schema,
        Err(_) => return Ok(Err("APP_HEADER_INVALID")),
    };
    if !matches!(schema.schema_version, 2 | 3) {
        return Ok(Err("APP_SCHEMA_UNSUPPORTED"));
    }
    let header: AppHeader = match toml::from_str(&contents) {
        Ok(header) => header,
        Err(_) => return Ok(Err("APP_HEADER_INVALID")),
    };
    if header.id != directory_id {
        return Ok(Err("APP_ID_MISMATCH"));
    }
    if !super::valid_metadata_identity_versions(
        schema.schema_version,
        header.resource_name_schema_version,
    ) || crate::domain::validate_slug_for_resource_schema(
        &header.slug,
        header.resource_name_schema_version,
    )
    .is_err()
        || header.display_name.trim().is_empty()
        || !(60..=86_400).contains(&header.poll_interval_seconds)
        || header.created_at > header.updated_at
    {
        return Ok(Err("APP_HEADER_INVALID"));
    }
    if header.draft_revision.is_none() != header.draft_config_sha256.is_none()
        || (header.draft_revision.is_none()
            && (header.discovery_image_ref.is_some()
                || header.credential_ref.is_some()
                || header.desired_state != DesiredState::Stopped
                || header.auto_deploy_enabled
                || active.is_some()
                || pending.is_some()))
    {
        return Ok(Err("APP_HEADER_INVALID"));
    }
    let draft = match (
        header.draft_revision,
        header.draft_config_sha256.as_deref(),
        header.discovery_image_ref.as_deref(),
    ) {
        (None, None, None) => None,
        (Some(revision), Some(expected_hash), Some(image_ref)) => {
            match load_revision(path, revision, integrity_key) {
                Ok(loaded) if loaded.metadata.config_sha256 == expected_hash => {
                    if let Some(key) = integrity_key {
                        match loaded.normalize_verified(
                            header.display_name.clone(),
                            image_ref.to_owned(),
                            header.credential_ref,
                            header.auto_deploy_enabled,
                            header.poll_interval_seconds,
                            key,
                            allowed_bind_roots,
                        ) {
                            Ok(_) => {}
                            _ => return Ok(Err("CONFIG_REVISION_INVALID")),
                        }
                    }
                    Some(loaded.response(
                        image_ref.to_owned(),
                        header.credential_ref,
                        header.auto_deploy_enabled,
                        header.poll_interval_seconds,
                    ))
                }
                Ok(_) | Err(StoreError::ContentInvalid) => {
                    return Ok(Err("CONFIG_REVISION_INVALID"));
                }
                Err(StoreError::Permission(error)) => return Err(StoreError::Permission(error)),
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Err("CONFIG_REVISION_MISSING"));
                }
                Err(error) => return Err(error),
            }
        }
        _ => return Ok(Err("APP_HEADER_INVALID")),
    };
    let releases = path.join("releases");
    match fs::symlink_metadata(&releases) {
        Ok(metadata) if metadata.is_dir() => check_private(&releases, true)?,
        Ok(_) => return Ok(Err("RELEASES_DIRECTORY_INVALID")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("RELEASES_DIRECTORY_MISSING"));
        }
        Err(error) => return Err(error.into()),
    }
    let release_revisions = match collect_release_revisions(
        path,
        canonical_app_directory,
        directory_id,
        integrity_key,
        allowed_bind_roots,
        mode,
    )? {
        Ok(revisions) => revisions,
        Err(code) => return Ok(Err(code)),
    };
    for (revision, expected_hash) in &release_revisions {
        match load_revision(path, *revision, integrity_key) {
            Ok(loaded) if loaded.metadata.config_sha256 == *expected_hash => {}
            Ok(_) | Err(StoreError::ContentInvalid) => {
                return Ok(Err("RELEASE_CONFIG_REVISION_INVALID"));
            }
            Err(StoreError::Permission(error)) => return Err(StoreError::Permission(error)),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Err("RELEASE_CONFIG_REVISION_MISSING"));
            }
            Err(error) => return Err(error),
        }
    }
    let active = match active {
        Some(link) => match read_release_header(link, directory_id, integrity_key)? {
            Ok(release) => Some(release),
            Err(code) => return Ok(Err(code)),
        },
        None => None,
    };
    let pending = match pending {
        Some(link) => match read_release_header(link, directory_id, integrity_key)? {
            Ok(release) => Some(release),
            Err(code) => return Ok(Err(code)),
        },
        None => None,
    };
    let (
        active_release_id,
        active_image_ref,
        active_config_revision,
        active_config_sha256,
        active_compose_schema_version,
    ) = match active {
        Some(release) => (
            Some(release.id),
            Some(release.runnable_image_ref),
            release.config_revision,
            release.config_sha256,
            Some(release.compose_schema_version),
        ),
        None => (None, None, None, None, None),
    };
    let (pending_release_id, pending_image_ref, pending_config_revision) = match pending {
        Some(release) => (
            Some(release.id),
            Some(release.runnable_image_ref),
            release.config_revision,
        ),
        None => (None, None, None),
    };
    if let Some(revision) = active_config_revision {
        match load_revision(path, revision, integrity_key) {
            Ok(loaded)
                if active_config_sha256.as_deref()
                    == Some(loaded.metadata.config_sha256.as_str()) => {}
            Ok(_) | Err(StoreError::ContentInvalid) => {
                return Ok(Err("ACTIVE_CONFIG_REVISION_INVALID"));
            }
            Err(StoreError::Permission(error)) => return Err(StoreError::Permission(error)),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Err("ACTIVE_CONFIG_REVISION_MISSING"));
            }
            Err(error) => return Err(error),
        }
    };
    let load_network_plan = |revision: Option<Uuid>| -> Result<Option<NetworkPlan>, StoreError> {
        let Some(revision) = revision else {
            return Ok(None);
        };
        let loaded = load_revision(path, revision, integrity_key)?;
        network_plan(
            loaded.metadata.owned_default_network,
            loaded.metadata.service_discovery_enabled,
            &loaded.metadata.networks,
        )
        .map(Some)
        .map_err(|_| StoreError::ContentInvalid)
    };
    let active_network_plan = load_network_plan(active_config_revision)?;
    let pending_network_plan = load_network_plan(pending_config_revision)?;
    if let (Some(release_id), Some(image), Some(revision), Some(expected_hash)) = (
        active_release_id,
        active_image_ref.as_deref(),
        active_config_revision,
        active_config_sha256.as_deref(),
    ) {
        match validate_active_compose(
            path,
            canonical_app_directory,
            &header,
            release_id,
            image,
            revision,
            expected_hash,
            active_compose_schema_version.is_some_and(|version| version >= 3),
            integrity_key,
            allowed_bind_roots,
        )? {
            Ok(()) => {}
            Err(code) => return Ok(Err(code)),
        }
    }
    let mut referenced: HashSet<Uuid> = release_revisions.keys().copied().collect();
    if let Some(revision) = header.draft_revision {
        referenced.insert(revision);
    }
    if mode == ScanMode::StartupCleanup {
        cleanup_unreferenced_revisions(path, &referenced)?;
    }
    Ok(Ok(RecoveredApp {
        app_id: header.id,
        project_name: header.resource_names().project_name,
        slug: header.slug,
        display_name: header.display_name,
        resource_name_schema_version: header.resource_name_schema_version,
        active_release_id,
        active_image_ref,
        active_config_revision,
        active_config_sha256,
        active_network_plan,
        pending_release_id,
        pending_image_ref,
        pending_config_revision,
        pending_network_plan,
        discovery_image_ref: header.discovery_image_ref,
        draft_revision: header.draft_revision,
        draft_config_sha256: header.draft_config_sha256,
        desired_state: header.desired_state,
        auto_deploy_enabled: header.auto_deploy_enabled,
        poll_interval_seconds: header.poll_interval_seconds,
        last_operation_id: Some(header.last_operation_id),
        draft,
        source_updated_at: header.updated_at,
    }))
}

#[allow(clippy::too_many_arguments)]
fn validate_active_compose(
    app_directory: &Path,
    canonical_app_directory: &Path,
    app: &AppHeader,
    release_id: Uuid,
    image_ref: &str,
    revision_id: Uuid,
    expected_hash: &str,
    include_stop_grace_period: bool,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
) -> Result<Result<(), &'static str>, StoreError> {
    if validate_runnable_image(image_ref).is_err() {
        return Ok(Err("ACTIVE_RELEASE_IMAGE_INVALID"));
    }
    let Some(key) = integrity_key else {
        return Ok(Ok(()));
    };
    let loaded = match load_revision(app_directory, revision_id, integrity_key) {
        Ok(loaded) if loaded.metadata.config_sha256 == expected_hash => loaded,
        Ok(_) | Err(StoreError::ContentInvalid) => {
            return Ok(Err("ACTIVE_CONFIG_REVISION_INVALID"));
        }
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("ACTIVE_CONFIG_REVISION_MISSING"));
        }
        Err(error) => return Err(error),
    };
    let draft = match loaded.normalize_verified(
        app.display_name.clone(),
        app.discovery_image_ref
            .clone()
            .ok_or(StoreError::ContentInvalid)?,
        app.credential_ref,
        app.auto_deploy_enabled,
        app.poll_interval_seconds,
        key,
        allowed_bind_roots,
    ) {
        Ok(draft) => draft,
        _ => return Ok(Err("ACTIVE_CONFIG_REVISION_INVALID")),
    };
    let revision_directory = canonical_app_directory
        .join("config-revisions")
        .join(revision_id.to_string());
    let (expected, _) = generate(
        ComposeInput {
            resource_identity: app.resource_identity(),
            release_id,
            image_ref,
            revision_directory: &revision_directory,
            draft: &draft,
            include_stop_grace_period,
        },
        true,
    )
    .map_err(|_| StoreError::ContentInvalid)?;
    let compose_path = app_directory
        .join("releases")
        .join(release_id.to_string())
        .join("compose.yaml");
    match fs::read(compose_path) {
        Ok(actual) if actual == expected.as_bytes() => Ok(Ok(())),
        Ok(_) => Ok(Err("ACTIVE_COMPOSE_INVALID")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Err("ACTIVE_COMPOSE_MISSING"))
        }
        Err(error) => Err(error.into()),
    }
}

fn collect_release_revisions(
    app_directory: &Path,
    canonical_app_directory: &Path,
    app_id: Uuid,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
    mode: ScanMode,
) -> Result<Result<HashMap<Uuid, String>, &'static str>, StoreError> {
    let releases = app_directory.join("releases");
    let mut references = HashMap::new();
    for entry in fs::read_dir(&releases)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if is_release_temp_name(&entry.file_name()) {
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(StoreError::SymlinkBoundary);
            }
            check_private(&entry.path(), true)?;
            validate_managed_tree(&entry.path(), app_directory, ManagedTreePolicy::Strict)?;
            if mode == ScanMode::StartupCleanup {
                fs::remove_dir_all(entry.path())?;
            }
            continue;
        }
        if file_type.is_symlink() {
            return Err(StoreError::SymlinkBoundary);
        }
        if !file_type.is_dir() {
            return Ok(Err("RELEASE_DIRECTORY_INVALID"));
        }
        let name = entry.file_name();
        let Some(release_id) = name.to_str().and_then(|value| value.parse::<Uuid>().ok()) else {
            return Ok(Err("RELEASE_DIRECTORY_ID_INVALID"));
        };
        if name.as_os_str() != std::ffi::OsStr::new(&release_id.to_string()) {
            return Ok(Err("RELEASE_DIRECTORY_ID_NON_CANONICAL"));
        }
        check_private(&entry.path(), true)?;
        validate_managed_tree(&entry.path(), app_directory, ManagedTreePolicy::Strict)?;
        let link = ValidatedReleaseLink {
            release_id,
            release_directory: entry.path(),
        };
        let release = match read_release_header(&link, app_id, integrity_key)? {
            Ok(release) => release,
            Err(code) => return Ok(Err(code)),
        };
        if !matches!(release.schema_version, 3..=5) {
            return Ok(Err("RELEASE_SCHEMA_UNSUPPORTED"));
        }
        match validate_v2_release(
            app_directory,
            canonical_app_directory,
            app_id,
            release_id,
            integrity_key,
            allowed_bind_roots,
        )? {
            Ok(()) => {}
            Err(code) => return Ok(Err(code)),
        }
        match (release.config_revision, release.config_sha256) {
            (Some(revision), Some(hash)) => {
                if let Some(previous) = references.insert(revision, hash.clone())
                    && previous != hash
                {
                    return Ok(Err("RELEASE_CONFIG_REVISION_INVALID"));
                }
            }
            (None, None) => {}
            _ => return Ok(Err("RELEASE_HEADER_INVALID")),
        }
    }
    if mode == ScanMode::StartupCleanup {
        super::sync_directory(&releases)?;
    }
    Ok(Ok(references))
}

fn validate_v2_release(
    app_directory: &Path,
    canonical_app_directory: &Path,
    app_id: Uuid,
    release_id: Uuid,
    integrity_key: Option<&[u8]>,
    allowed_bind_roots: &[PathBuf],
) -> Result<Result<(), &'static str>, StoreError> {
    let Some(key) = integrity_key else {
        return Ok(Err("RELEASE_INTEGRITY_UNVERIFIED"));
    };
    let directory = app_directory.join("releases").join(release_id.to_string());
    let mut release: super::releases::ReleaseV2 = match read_toml(
        &directory.join("release.toml"),
        "RELEASE_HEADER_MISSING",
        "RELEASE_HEADER_INVALID",
    )? {
        Ok(value) => value,
        Err(code) => return Ok(Err(code)),
    };
    release.apply_schema_defaults();
    if !matches!(
        (release.schema_version, release.compose_schema_version),
        (3, 2) | (4, 3) | (5, 4)
    ) || release.app_id != app_id
        || release.id != release_id
        || super::releases::sign(&release, key) != release.integrity_hmac
    {
        return Ok(Err("RELEASE_INTEGRITY_INVALID"));
    }
    let loaded = match load_revision(app_directory, release.config_revision, integrity_key) {
        Ok(value) if value.metadata.config_sha256 == release.config_sha256 => value,
        Ok(_) | Err(StoreError::ContentInvalid) => {
            return Ok(Err("RELEASE_CONFIG_REVISION_INVALID"));
        }
        Err(error) => return Err(error),
    };
    let app: AppHeader = match read_toml(
        &app_directory.join("app.toml"),
        "APP_HEADER_MISSING",
        "APP_HEADER_INVALID",
    )? {
        Ok(value) => value,
        Err(code) => return Ok(Err(code)),
    };
    let identity = app.resource_identity();
    let draft = match loaded.normalize_verified(
        app.display_name.clone(),
        release.source_image_ref.clone(),
        release.credential_ref,
        app.auto_deploy_enabled,
        app.poll_interval_seconds,
        key,
        allowed_bind_roots,
    ) {
        Ok(value) => value,
        _ => return Ok(Err("RELEASE_CONFIG_REVISION_INVALID")),
    };
    if release.stop_grace_period_seconds != loaded.metadata.stop_grace_period_seconds {
        return Ok(Err("RELEASE_CONFIG_REVISION_INVALID"));
    }
    let (canonical, _) = generate(
        ComposeInput {
            resource_identity: identity,
            release_id,
            image_ref: &release.runnable_image_ref,
            revision_directory: &canonical_app_directory
                .join("config-revisions")
                .join(release.config_revision.to_string()),
            draft: &draft,
            include_stop_grace_period: release.compose_schema_version >= 3,
        },
        true,
    )
    .map_err(|_| StoreError::ContentInvalid)?;
    let compose = match fs::read(directory.join("compose.yaml")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("RELEASE_COMPOSE_MISSING"));
        }
        Err(error) => return Err(error.into()),
    };
    if compose != canonical.as_bytes()
        || format!("{:x}", Sha256::digest(&compose)) != release.compose_sha256
    {
        return Ok(Err("RELEASE_COMPOSE_INVALID"));
    }
    Ok(Ok(()))
}

fn is_release_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(value) = name.to_str() else {
        return false;
    };
    let Some(suffix) = value.strip_prefix(".solodock-tmp-") else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn cleanup_unreferenced_revisions(
    app_directory: &Path,
    referenced: &HashSet<Uuid>,
) -> Result<(), StoreError> {
    let revisions = app_directory.join("config-revisions");
    for entry in fs::read_dir(&revisions)? {
        let entry = entry?;
        if is_revision_temp_name(&entry.file_name()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(StoreError::SymlinkBoundary);
        }
        if !file_type.is_dir() {
            return Err(StoreError::ContentInvalid);
        }
        let name = entry.file_name();
        let revision = name
            .to_str()
            .and_then(|value| value.parse::<Uuid>().ok())
            .ok_or(StoreError::ContentInvalid)?;
        if name.as_os_str() != std::ffi::OsStr::new(&revision.to_string()) {
            return Err(StoreError::ContentInvalid);
        }
        if !referenced.contains(&revision) {
            check_private(&entry.path(), true)?;
            validate_managed_tree(&entry.path(), app_directory, ManagedTreePolicy::Strict)?;
            fs::remove_dir_all(entry.path())?;
        }
    }
    super::sync_directory(&revisions)
}

fn load_revision(
    path: &Path,
    revision: Uuid,
    integrity_key: Option<&[u8]>,
) -> Result<super::config_revision::LoadedRevision, StoreError> {
    match integrity_key {
        Some(key) => super::config_revision::load_verified(path, revision, key),
        None => super::config_revision::load(path, revision),
    }
}

fn validate_release_link_path(
    app_directory: &Path,
    name: &str,
) -> Result<Result<Option<ValidatedReleaseLink>, &'static str>, StoreError> {
    let link = app_directory.join(name);
    let metadata = match fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Ok(None)),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_symlink() {
        return Err(StoreError::SymlinkBoundary);
    }
    let target = fs::read_link(&link)?;
    let parts: Vec<_> = target.components().collect();
    let release_id = match parts.as_slice() {
        [Component::Normal(releases), Component::Normal(id)]
            if *releases == std::ffi::OsStr::new("releases") =>
        {
            id.to_string_lossy()
                .parse::<Uuid>()
                .map_err(|_| StoreError::SymlinkBoundary)?
        }
        _ => return Err(StoreError::SymlinkBoundary),
    };
    let canonical_target = PathBuf::from("releases").join(release_id.to_string());
    if target.as_os_str() != canonical_target.as_os_str() {
        return Err(StoreError::SymlinkBoundary);
    }
    let release_directory = app_directory.join(&target);
    match fs::symlink_metadata(&release_directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(StoreError::SymlinkBoundary);
        }
        Ok(metadata) if metadata.is_dir() => check_private(&release_directory, true)?,
        Ok(_) => return Ok(Err("RELEASE_TARGET_INVALID")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("RELEASE_TARGET_MISSING"));
        }
        Err(error) => return Err(error.into()),
    }
    Ok(Ok(Some(ValidatedReleaseLink {
        release_id,
        release_directory,
    })))
}

fn read_release_header(
    link: &ValidatedReleaseLink,
    app_id: Uuid,
    integrity_key: Option<&[u8]>,
) -> Result<Result<ReleaseHeader, &'static str>, StoreError> {
    let release_path = link.release_directory.join("release.toml");
    let release: ReleaseHeader = match read_toml(
        &release_path,
        "RELEASE_HEADER_MISSING",
        "RELEASE_HEADER_INVALID",
    )? {
        Ok(release) => release,
        Err(code) => return Ok(Err(code)),
    };
    if release.id != link.release_id
        || release.app_id != app_id
        || !valid_runnable_image_ref(&release.runnable_image_ref)
    {
        return Ok(Err("RELEASE_HEADER_INVALID"));
    }
    match release.schema_version {
        3..=5 => {
            let Some(key) = integrity_key else {
                return Ok(Err("RELEASE_INTEGRITY_UNVERIFIED"));
            };
            let mut value: super::releases::ReleaseV2 = match read_toml(
                &release_path,
                "RELEASE_HEADER_MISSING",
                "RELEASE_HEADER_INVALID",
            )? {
                Ok(value) => value,
                Err(code) => return Ok(Err(code)),
            };
            value.apply_schema_defaults();
            if super::releases::sign(&value, key) != value.integrity_hmac {
                return Ok(Err("RELEASE_INTEGRITY_INVALID"));
            }
        }
        _ => return Ok(Err("RELEASE_SCHEMA_UNSUPPORTED")),
    }
    let _ = release.created_at;
    Ok(Ok(release))
}

fn read_toml<T: DeserializeOwned>(
    path: &Path,
    missing_code: &'static str,
    invalid_code: &'static str,
) -> Result<Result<T, &'static str>, StoreError> {
    let contents = match read_toml_contents(path, missing_code, invalid_code)? {
        Ok(contents) => contents,
        Err(code) => return Ok(Err(code)),
    };
    match toml::from_str(&contents) {
        Ok(value) => Ok(Ok(value)),
        Err(_) => Ok(Err(invalid_code)),
    }
}

fn read_toml_contents(
    path: &Path,
    missing_code: &'static str,
    invalid_code: &'static str,
) -> Result<Result<String, &'static str>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => check_private(path, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(missing_code));
        }
        Err(error) => return Err(error.into()),
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(missing_code));
        }
        Err(error) => return Err(error.into()),
    };
    let contents = match std::str::from_utf8(&bytes) {
        Ok(contents) => contents,
        Err(_) => return Ok(Err(invalid_code)),
    };
    Ok(Ok(contents.to_owned()))
}

fn valid_runnable_image_ref(value: &str) -> bool {
    validate_runnable_image(value).is_ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    fn managed_file_draft(key: &[u8], public_value: &str) -> crate::domain::NormalizedDraft {
        crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Managed files".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: crate::domain::EnvironmentInput::default(),
                files: vec![
                    crate::domain::ManagedFileInput {
                        logical_name: "public".into(),
                        target_path: "/app/public".into(),
                        sensitive: false,
                        readonly: true,
                        content: crate::domain::ManagedFileContent::Public(
                            crate::domain::PublicFileContent {
                                content: public_value.into(),
                            },
                        ),
                    },
                    crate::domain::ManagedFileInput {
                        logical_name: "secret".into(),
                        target_path: "/app/secret".into(),
                        sensitive: true,
                        readonly: true,
                        content: crate::domain::ManagedFileContent::Secret(
                            crate::domain::SecretOperation::Replace {
                                value: format!("secret-{public_value}"),
                            },
                        ),
                    },
                ],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![],
                health: crate::domain::HealthPolicy::default(),
            },
            &crate::domain::ExistingSecrets::default(),
            key,
            &[],
        )
        .unwrap()
    }

    #[test]
    fn startup_normalizes_all_canonical_managed_revisions_but_runtime_scan_never_mutates() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"normalization-integrity-key".to_vec();
        let store = crate::app_store::AppStore::initialize_managed(
            root.path().join("apps"),
            key.clone(),
            vec![],
        )
        .unwrap();
        let app_id = Uuid::new_v4();
        let first_revision = Uuid::new_v4();
        let first = managed_file_draft(&key, "first");
        let now = OffsetDateTime::now_utc();
        let metadata = store
            .create_app(
                app_id,
                "managed",
                Uuid::new_v4(),
                Some((first_revision, &first)),
                now,
            )
            .unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let release_id = Uuid::new_v4();
        store
            .publish_v2_release(
                &metadata,
                release_id,
                &crate::registry::ResolvedImage {
                    source_image_ref: "registry.example/app:stable".into(),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: format!("registry.example/app@{digest}"),
                    platform: crate::registry::Platform::canonical("linux", "amd64", None).unwrap(),
                    local_image_id: digest,
                },
                crate::app_store::releases::ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        crate::app_store::atomic::AtomicWriter::switch_release_link(
            &store.app_directory(app_id),
            "active",
            release_id,
        )
        .unwrap();
        let second_revision = Uuid::new_v4();
        store
            .update_draft(
                app_id,
                Some(first_revision),
                second_revision,
                Uuid::new_v4(),
                &managed_file_draft(&key, "second"),
                now,
            )
            .unwrap();
        let leaf = |revision: Uuid, kind: &str, name: &str| {
            store
                .app_directory(app_id)
                .join("config-revisions")
                .join(revision.to_string())
                .join("files")
                .join(kind)
                .join(name)
        };
        let first_public = leaf(first_revision, "public", "public");
        let first_secret = leaf(first_revision, "secret", "secret");
        let second_public = leaf(second_revision, "public", "public");
        fs::set_permissions(&first_public, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&first_secret, fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(&second_public, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            store.scan_read_only(),
            Err(StoreError::ManagedFilePermissionInvalid)
        ));
        assert_eq!(
            fs::metadata(&first_public).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let report = store.scan().unwrap();
        assert_eq!(report.valid_apps.len(), 1, "{:?}", report.issues);
        for path in [&first_public, &first_secret, &second_public] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                MANAGED_FILE_MODE
            );
        }
        let first_scan = store.scan().unwrap();
        assert_eq!(first_scan.valid_apps.len(), 1, "{:?}", first_scan.issues);

        fs::set_permissions(&first_public, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.scan(),
            Err(StoreError::ManagedFilePermissionInvalid)
        ));
        assert_eq!(
            fs::metadata(first_public).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn verified_m3_scan_rejects_tampered_active_compose() {
        let root = tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let apps = root.path().join("apps");
        let key = b"recovery-integrity-key".to_vec();
        let store =
            crate::app_store::AppStore::initialize_managed(apps, key.clone(), vec![]).unwrap();
        let app_id = Uuid::new_v4();
        let revision_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let draft = crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Canonical".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: crate::domain::EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![],
                health: crate::domain::HealthPolicy::default(),
            },
            &crate::domain::ExistingSecrets::default(),
            &key,
            &[],
        )
        .unwrap();
        let metadata = store
            .create_app(
                app_id,
                "canonical",
                revision_id,
                Some((revision_id, &draft)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        store
            .publish_v2_release(
                &metadata,
                release_id,
                &crate::registry::ResolvedImage {
                    source_image_ref: "registry.example/app:stable".into(),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: digest.clone(),
                    index_digest: None,
                    manifest_digest: digest.clone(),
                    runnable_image_ref: format!("registry.example/app@{digest}"),
                    platform: crate::registry::Platform::canonical("linux", "amd64", None).unwrap(),
                    local_image_id: format!("sha256:{}", "b".repeat(64)),
                },
                crate::app_store::releases::ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        crate::app_store::atomic::AtomicWriter::switch_release_link(
            &store.app_directory(app_id),
            "active",
            release_id,
        )
        .unwrap();
        assert_eq!(store.scan().unwrap().valid_apps.len(), 1);
        crate::app_store::atomic::AtomicWriter::write(
            &store
                .app_directory(app_id)
                .join("releases")
                .join(release_id.to_string())
                .join("compose.yaml"),
            b"services:\n  app:\n    image: latest\n    privileged: true\n",
            0o600,
        )
        .unwrap();
        let report = store.scan().unwrap();
        assert!(report.valid_apps.is_empty());
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "RELEASE_COMPOSE_INVALID")
        );
    }

    fn fixture(root: &Path, app_id: Uuid, release_id: Uuid, slug: &str) {
        fixture_named(root, &app_id.to_string(), app_id, release_id, slug);
    }

    fn fixture_named(
        root: &Path,
        directory_name: &str,
        app_id: Uuid,
        release_id: Uuid,
        slug: &str,
    ) {
        empty_fixture(root, app_id, slug);
        let canonical = root.join(app_id.to_string());
        let app = root.join(directory_name);
        if canonical != app {
            fs::rename(&canonical, &app).unwrap();
        }
        let release = app.join("releases").join(release_id.to_string());
        fs::create_dir_all(&release).unwrap();
        fs::write(
            release.join("release.toml"),
            format!("schema_version=1\nid='{release_id}'\napp_id='{app_id}'\nrunnable_image_ref='registry.example/image@sha256:{}'\ncreated_at='2026-08-28T00:00:00Z'\n", "a".repeat(64)),
        ).unwrap();
        symlink(format!("releases/{release_id}"), app.join("active")).unwrap();
        for path in [&app, &app.join("releases"), &release] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        for path in [app.join("app.toml"), release.join("release.toml")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn empty_fixture(root: &Path, app_id: Uuid, slug: &str) {
        let store = crate::app_store::AppStore::initialize(root.to_path_buf()).unwrap();
        let draft = crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Example".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: Default::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![crate::domain::NetworkInput::OwnedDefault],
                health: Default::default(),
            },
            &Default::default(),
            b"fixture-key",
            &[],
        )
        .unwrap();
        let revision_id = Uuid::new_v4();
        store
            .create_app(
                app_id,
                slug,
                revision_id,
                Some((revision_id, &draft)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
    }

    #[test]
    fn rejects_legacy_release_schema_from_filesystem() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        fixture(root.path(), app_id, release_id, "example");
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "RELEASE_SCHEMA_UNSUPPORTED");
    }

    #[test]
    fn recovery_and_runtime_reader_share_the_strict_app_header_schema() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        empty_fixture(root.path(), app_id, "example");
        let app_path = root.path().join(app_id.to_string()).join("app.toml");

        let report = scan(root.path()).unwrap();
        assert_eq!(report.valid_apps.len(), 1);
        let store = crate::app_store::AppStore::initialize(root.path().to_path_buf()).unwrap();
        assert_eq!(store.read_metadata(app_id).unwrap().slug, "example");

        let valid_header = fs::read_to_string(&app_path).unwrap();
        let mut schema3_legacy_identity: toml::Value = toml::from_str(&valid_header).unwrap();
        schema3_legacy_identity.as_table_mut().unwrap().insert(
            "resource_name_schema_version".into(),
            toml::Value::Integer(1),
        );
        fs::write(
            &app_path,
            toml::to_string(&schema3_legacy_identity).unwrap(),
        )
        .unwrap();
        assert_eq!(scan(root.path()).unwrap().valid_apps.len(), 1);
        assert_eq!(
            store
                .read_metadata(app_id)
                .unwrap()
                .resource_names()
                .bridge_name,
            "sd-example"
        );

        let mut schema2_current_identity: toml::Value = toml::from_str(&valid_header).unwrap();
        let table = schema2_current_identity.as_table_mut().unwrap();
        table.insert("schema_version".into(), toml::Value::Integer(2));
        table.insert(
            "resource_name_schema_version".into(),
            toml::Value::Integer(2),
        );
        fs::write(
            &app_path,
            toml::to_string(&schema2_current_identity).unwrap(),
        )
        .unwrap();
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "APP_HEADER_INVALID");
        assert!(matches!(
            store.read_metadata(app_id),
            Err(crate::app_store::StoreError::ContentInvalid)
        ));

        fs::write(
            &app_path,
            format!("{valid_header}\nfuture_field = 'rejected'\n"),
        )
        .unwrap();
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "APP_HEADER_INVALID");
        assert!(matches!(
            store.read_metadata(app_id),
            Err(crate::app_store::StoreError::ContentInvalid)
        ));
    }

    #[test]
    fn legacy_app_header_reports_schema_incompatibility_before_strict_parsing() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        empty_fixture(root.path(), app_id, "example");
        let app_path = root.path().join(app_id.to_string()).join("app.toml");
        let mut legacy_header: toml::Value =
            toml::from_str(&fs::read_to_string(&app_path).unwrap()).unwrap();
        let table = legacy_header.as_table_mut().unwrap();
        table.insert("schema_version".into(), toml::Value::Integer(1));
        table.insert(
            "project_name".into(),
            toml::Value::String(format!("solodock-{}", app_id.simple())),
        );
        fs::write(&app_path, toml::to_string(&legacy_header).unwrap()).unwrap();

        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "APP_SCHEMA_UNSUPPORTED");
    }

    #[test]
    fn runtime_scan_never_removes_a_concurrent_writer_temporary_app() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let temporary = root
            .path()
            .join(format!(".solodock-tmp-app-{}", Uuid::new_v4().simple()));
        fs::create_dir(&temporary).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).unwrap();
        let revision = temporary
            .join("config-revisions")
            .join(Uuid::new_v4().to_string());
        let public_directory = revision.join("files/public");
        let secret_directory = revision.join("files/secret");
        fs::create_dir_all(&public_directory).unwrap();
        fs::create_dir(&secret_directory).unwrap();
        for directory in [
            temporary.join("config-revisions"),
            revision.clone(),
            revision.join("files"),
            public_directory.clone(),
            secret_directory.clone(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let public = public_directory.join("public");
        let secret = secret_directory.join("secret");
        fs::write(&public, "public").unwrap();
        fs::write(&secret, "secret").unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o400)).unwrap();

        let report = scan_read_only_with_options(root.path(), None, &[]).unwrap();
        assert!(temporary.exists());
        assert_eq!(
            fs::metadata(&public).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o7777,
            0o400
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "TEMP_ARTIFACT_IGNORED")
        );

        let report = scan_with_options(root.path(), None, &[]).unwrap();
        assert!(!temporary.exists());
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "TEMP_APP_REMOVED")
        );
    }

    #[test]
    fn runtime_scan_never_collects_a_published_revision_before_metadata_commit() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = b"runtime-read-only-integrity-key".to_vec();
        let store = crate::app_store::AppStore::initialize_managed(
            root.path().join("apps"),
            key.clone(),
            vec![],
        )
        .unwrap();
        let draft = crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Read only".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                stop_grace_period_seconds: 10,
                environment: crate::domain::EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                service_discovery_enabled: true,
                networks: vec![],
                health: crate::domain::HealthPolicy::default(),
            },
            &crate::domain::ExistingSecrets::default(),
            &key,
            &[],
        )
        .unwrap();
        let app_id = Uuid::new_v4();
        let referenced = Uuid::new_v4();
        store
            .create_app(
                app_id,
                "read-only",
                referenced,
                Some((referenced, &draft)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let publishing = Uuid::new_v4();
        let path = crate::app_store::config_revision::publish(
            &store.app_directory(app_id),
            publishing,
            &draft,
        )
        .unwrap();

        store.scan_read_only().unwrap();
        assert!(path.exists());
        store.scan().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn rejects_escaping_symlink() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        fixture(root.path(), app_id, release_id, "example");
        let active = root.path().join(app_id.to_string()).join("active");
        fs::remove_file(&active).unwrap();
        symlink("../../outside", active).unwrap();
        assert!(matches!(
            scan(root.path()),
            Err(StoreError::SymlinkBoundary)
        ));
    }

    #[test]
    fn rejects_duplicate_slugs_without_mutating_input() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        empty_fixture(root.path(), Uuid::new_v4(), "same");
        empty_fixture(root.path(), Uuid::new_v4(), "same");
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues.len(), 2);
    }

    #[test]
    fn reports_corrupt_release_content_without_aborting_scan() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        fixture(root.path(), app_id, release_id, "example");
        fs::write(
            root.path()
                .join(app_id.to_string())
                .join("releases")
                .join(release_id.to_string())
                .join("release.toml"),
            "not valid toml =",
        )
        .unwrap();
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "RELEASE_HEADER_INVALID");
    }

    #[test]
    fn ignores_private_temp_artifacts_and_rejects_unsafe_managed_files() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        empty_fixture(root.path(), app_id, "example");
        let app = root.path().join(app_id.to_string());
        let temporary = app.join(".solodock-tmp-interrupted");
        fs::write(&temporary, "partial").unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
        let report = scan(root.path()).unwrap();
        assert_eq!(report.valid_apps.len(), 1);
        assert_eq!(report.issues[0].code, "TEMP_ARTIFACT_IGNORED");

        let secret_directory = app.join("secrets");
        fs::create_dir(&secret_directory).unwrap();
        fs::set_permissions(&secret_directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(scan(root.path()), Err(StoreError::Permission(_))));
    }

    #[test]
    fn escaping_link_is_a_hard_failure_even_when_app_header_is_missing() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        fixture(root.path(), app_id, Uuid::new_v4(), "example");
        let app = root.path().join(app_id.to_string());
        fs::remove_file(app.join("app.toml")).unwrap();
        fs::remove_file(app.join("active")).unwrap();
        symlink("../../outside", app.join("active")).unwrap();
        assert!(matches!(
            scan(root.path()),
            Err(StoreError::SymlinkBoundary)
        ));
    }

    #[test]
    fn missing_releases_directory_is_a_degraded_issue() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        fixture(root.path(), app_id, Uuid::new_v4(), "example");
        let app = root.path().join(app_id.to_string());
        fs::remove_file(app.join("active")).unwrap();
        fs::remove_dir_all(app.join("releases")).unwrap();
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "RELEASES_DIRECTORY_MISSING");
    }

    #[test]
    fn dangling_release_target_is_a_degraded_issue() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        fixture(root.path(), app_id, release_id, "example");
        fs::remove_dir_all(
            root.path()
                .join(app_id.to_string())
                .join("releases")
                .join(release_id.to_string()),
        )
        .unwrap();
        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        assert_eq!(report.issues[0].code, "RELEASE_TARGET_MISSING");
    }

    #[test]
    fn missing_and_non_utf8_headers_are_degraded_issues() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let missing_app = Uuid::new_v4();
        let missing_release = Uuid::new_v4();
        fixture(root.path(), missing_app, missing_release, "missing");
        fs::remove_file(
            root.path()
                .join(missing_app.to_string())
                .join("releases")
                .join(missing_release.to_string())
                .join("release.toml"),
        )
        .unwrap();

        let invalid_app = Uuid::new_v4();
        fixture(root.path(), invalid_app, Uuid::new_v4(), "invalid-app");
        fs::write(
            root.path().join(invalid_app.to_string()).join("app.toml"),
            [0xff, 0xfe],
        )
        .unwrap();

        let invalid_release_app = Uuid::new_v4();
        let invalid_release = Uuid::new_v4();
        fixture(
            root.path(),
            invalid_release_app,
            invalid_release,
            "invalid-rel",
        );
        fs::write(
            root.path()
                .join(invalid_release_app.to_string())
                .join("releases")
                .join(invalid_release.to_string())
                .join("release.toml"),
            [0xff, 0xfe],
        )
        .unwrap();

        let report = scan(root.path()).unwrap();
        assert!(report.valid_apps.is_empty());
        let codes: HashSet<_> = report.issues.iter().map(|issue| issue.code).collect();
        assert!(codes.contains("RELEASE_HEADER_MISSING"));
        assert!(codes.contains("APP_HEADER_INVALID"));
        assert!(codes.contains("RELEASE_HEADER_INVALID"));
    }

    #[test]
    fn non_canonical_app_uuid_directory_never_enters_valid_apps() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
        empty_fixture(root.path(), app_id, "canonical");
        let canonical = root.path().join(app_id.to_string());
        let non_canonical = root.path().join(app_id.simple().to_string());
        fs::rename(&canonical, &non_canonical).unwrap();
        empty_fixture(root.path(), app_id, "canonical");
        let report = scan(root.path()).unwrap();
        assert_eq!(report.valid_apps.len(), 1);
        assert_eq!(report.valid_apps[0].slug, "canonical");
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "APP_DIRECTORY_ID_NON_CANONICAL")
        );
    }

    #[test]
    fn release_link_target_requires_exact_canonical_text() {
        let release_id = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
        let invalid_targets = [
            format!("releases//{release_id}"),
            format!("releases/./{release_id}"),
            format!("releases/{}", release_id.simple()),
            format!("releases/{}", release_id.to_string().to_uppercase()),
        ];
        for target in invalid_targets {
            let root = tempdir().unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let app_id = Uuid::new_v4();
            fixture(root.path(), app_id, release_id, "example");
            let active = root.path().join(app_id.to_string()).join("active");
            fs::remove_file(&active).unwrap();
            symlink(target, active).unwrap();
            assert!(matches!(
                scan(root.path()),
                Err(StoreError::SymlinkBoundary)
            ));
        }
    }
}
