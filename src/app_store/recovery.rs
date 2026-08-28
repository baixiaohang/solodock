use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, de::DeserializeOwned};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{StoreError, atomic::is_internal_temp_name};
use crate::security::permissions::check_private;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredApp {
    pub app_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub project_name: String,
    pub active_release_id: Option<Uuid>,
    pub active_image_ref: Option<String>,
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

#[derive(Debug, Deserialize)]
struct AppHeader {
    schema_version: u32,
    id: Uuid,
    slug: String,
    display_name: String,
    project_name: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
struct ReleaseHeader {
    schema_version: u32,
    id: Uuid,
    app_id: Uuid,
    runnable_image_ref: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

struct ValidatedReleaseLink {
    release_id: Uuid,
    release_directory: PathBuf,
}

pub fn scan(apps_directory: &Path) -> Result<RecoveryReport, StoreError> {
    check_private(apps_directory, true)?;
    let mut report = RecoveryReport::default();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(apps_directory)? {
        let entry = entry?;
        if is_internal_temp_name(&entry.file_name()) {
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
        if validate_managed_tree(&entry.path())? > 0 {
            report.issues.push(RecoveryIssue {
                code: "TEMP_ARTIFACT_IGNORED",
                app_id: Some(directory_id),
            });
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
        match scan_app(
            &entry.path(),
            directory_id,
            active.as_ref(),
            pending.as_ref(),
        )? {
            Ok(app) => candidates.push(app),
            Err(code) => report.issues.push(RecoveryIssue {
                code,
                app_id: Some(directory_id),
            }),
        }
    }

    let mut slug_counts = HashMap::new();
    let mut project_counts = HashMap::new();
    for app in &candidates {
        *slug_counts.entry(app.slug.clone()).or_insert(0usize) += 1;
        *project_counts
            .entry(app.project_name.clone())
            .or_insert(0usize) += 1;
    }
    let mut duplicate_reported = HashSet::new();
    for app in candidates {
        if slug_counts[&app.slug] > 1 || project_counts[&app.project_name] > 1 {
            if duplicate_reported.insert(app.app_id) {
                report.issues.push(RecoveryIssue {
                    code: if slug_counts[&app.slug] > 1 {
                        "APP_SLUG_DUPLICATE"
                    } else {
                        "APP_PROJECT_NAME_DUPLICATE"
                    },
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

fn validate_managed_tree(app_directory: &Path) -> Result<usize, StoreError> {
    let mut pending = vec![app_directory.to_owned()];
    let mut temporary_artifacts = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let is_release_link = directory == app_directory
                && matches!(entry.file_name().to_str(), Some("active" | "pending"));
            if file_type.is_symlink() {
                if is_release_link {
                    continue;
                }
                return Err(StoreError::SymlinkBoundary);
            }
            if file_type.is_dir() {
                check_private(&path, true)?;
                pending.push(path);
            } else if file_type.is_file() {
                check_private(&path, false)?;
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

fn scan_app(
    path: &Path,
    directory_id: Uuid,
    active: Option<&ValidatedReleaseLink>,
    pending: Option<&ValidatedReleaseLink>,
) -> Result<Result<RecoveredApp, &'static str>, StoreError> {
    let app_path = path.join("app.toml");
    let header: AppHeader = match read_toml(&app_path, "APP_HEADER_MISSING", "APP_HEADER_INVALID")?
    {
        Ok(header) => header,
        Err(code) => return Ok(Err(code)),
    };
    if header.schema_version != 1 {
        return Ok(Err("APP_SCHEMA_UNSUPPORTED"));
    }
    if header.id != directory_id {
        return Ok(Err("APP_ID_MISMATCH"));
    }
    if header.slug.trim().is_empty()
        || header.display_name.trim().is_empty()
        || header.project_name.trim().is_empty()
        || header.created_at > header.updated_at
    {
        return Ok(Err("APP_HEADER_INVALID"));
    }
    let releases = path.join("releases");
    match fs::symlink_metadata(&releases) {
        Ok(metadata) if metadata.is_dir() => check_private(&releases, true)?,
        Ok(_) => return Ok(Err("RELEASES_DIRECTORY_INVALID")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("RELEASES_DIRECTORY_MISSING"));
        }
        Err(error) => return Err(error.into()),
    }
    let active = match active {
        Some(link) => match read_release_header(link, directory_id)? {
            Ok(release) => Some(release),
            Err(code) => return Ok(Err(code)),
        },
        None => None,
    };
    if let Some(link) = pending
        && let Err(code) = read_release_header(link, directory_id)?
    {
        return Ok(Err(code));
    }
    let (active_release_id, active_image_ref) = match active {
        Some(release) => (Some(release.id), Some(release.runnable_image_ref)),
        None => (None, None),
    };
    Ok(Ok(RecoveredApp {
        app_id: header.id,
        slug: header.slug,
        display_name: header.display_name,
        project_name: header.project_name,
        active_release_id,
        active_image_ref,
        source_updated_at: header.updated_at,
    }))
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
    if release.schema_version != 1
        || release.id != link.release_id
        || release.app_id != app_id
        || !valid_runnable_image_ref(&release.runnable_image_ref)
    {
        return Ok(Err("RELEASE_HEADER_INVALID"));
    }
    let _ = release.created_at;
    Ok(Ok(release))
}

fn read_toml<T: DeserializeOwned>(
    path: &Path,
    missing_code: &'static str,
    invalid_code: &'static str,
) -> Result<Result<T, &'static str>, StoreError> {
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
    match toml::from_str(contents) {
        Ok(value) => Ok(Ok(value)),
        Err(_) => Ok(Err(invalid_code)),
    }
}

fn valid_runnable_image_ref(value: &str) -> bool {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    !name.is_empty() && digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

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
        let app = root.join(directory_name);
        let release = app.join("releases").join(release_id.to_string());
        fs::create_dir_all(&release).unwrap();
        fs::write(
            app.join("app.toml"),
            format!("schema_version=1\nid='{app_id}'\nslug='{slug}'\ndisplay_name='Example'\nproject_name='solodock-{}'\ncreated_at='2026-08-28T00:00:00Z'\nupdated_at='2026-08-28T00:00:00Z'\nfuture_field='ignored'\n", app_id.simple()),
        ).unwrap();
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

    #[test]
    fn recovers_active_release_from_filesystem() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        fixture(root.path(), app_id, release_id, "example");
        let report = scan(root.path()).unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(report.valid_apps[0].active_release_id, Some(release_id));
        assert!(
            report.valid_apps[0]
                .active_image_ref
                .as_ref()
                .unwrap()
                .ends_with(&"a".repeat(64))
        );
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
        fixture(root.path(), Uuid::new_v4(), Uuid::new_v4(), "same");
        fixture(root.path(), Uuid::new_v4(), Uuid::new_v4(), "same");
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
        fixture(root.path(), app_id, Uuid::new_v4(), "example");
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
            "invalid-release",
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
        fixture(root.path(), app_id, Uuid::new_v4(), "canonical");
        fixture_named(
            root.path(),
            &app_id.simple().to_string(),
            app_id,
            Uuid::new_v4(),
            "non-canonical",
        );
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
