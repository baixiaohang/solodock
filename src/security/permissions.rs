use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

pub const MANAGED_FILE_MODE: u32 = 0o444;
pub const LEGACY_MANAGED_FILE_MODES: [u32; 2] = [0o400, 0o600];

pub fn ensure_private_directory(path: &Path) -> Result<(), PermissionError> {
    reject_symlink_components(path)?;
    if !path.exists() {
        #[cfg(unix)]
        {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700).create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
    }
    reject_symlink_components(path)?;
    check_private(path, true)
}

fn reject_symlink_components(path: &Path) -> Result<(), PermissionError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PermissionError::UnexpectedType(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn check_private(path: &Path, directory: bool) -> Result<(), PermissionError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(PermissionError::UnexpectedType(path.to_owned()));
    }
    #[cfg(unix)]
    {
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(PermissionError::Owner(path.to_owned()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(PermissionError::Mode(path.to_owned()));
        }
    }
    Ok(())
}

/// Validate every component below a trusted managed root without following a
/// symlink. This is deliberately repeated at read time because an attacker
/// with host access can replace an intermediate directory after startup.
pub fn check_private_tree(
    root: &Path,
    target: &Path,
    target_is_directory: bool,
) -> Result<(), PermissionError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| PermissionError::UnexpectedType(target.to_owned()))?;
    check_private(root, true)?;
    let mut current = root.to_owned();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        check_private(
            &current,
            index + 1 != components.len() || target_is_directory,
        )?;
    }
    Ok(())
}

pub fn set_private_file_mode(path: &Path) -> Result<(), PermissionError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    check_private(path, false)
}

pub fn check_service_owned_file_mode(
    path: &Path,
    expected_mode: u32,
) -> Result<(), PermissionError> {
    let metadata = fs::symlink_metadata(path)?;
    check_service_owned_regular_file(path, &metadata, expected_mode)
}

pub fn normalize_service_owned_file_mode(
    path: &Path,
    expected_mode: u32,
    legacy_modes: &[u32],
) -> Result<bool, PermissionError> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(PermissionError::UnexpectedType(path.to_owned()));
    }
    #[cfg(unix)]
    {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            return Err(PermissionError::UnexpectedType(path.to_owned()));
        }
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(PermissionError::Owner(path.to_owned()));
        }
        let mode = metadata.mode() & 0o7777;
        if mode == expected_mode {
            return Ok(false);
        }
        if !legacy_modes.contains(&mode) {
            return Err(PermissionError::Mode(path.to_owned()));
        }
        file.set_permissions(fs::Permissions::from_mode(expected_mode))?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        check_service_owned_regular_file(path, &metadata, expected_mode)?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = (expected_mode, legacy_modes);
        check_service_owned_file_mode(path, expected_mode)?;
        Ok(false)
    }
}

fn check_service_owned_regular_file(
    path: &Path,
    metadata: &fs::Metadata,
    expected_mode: u32,
) -> Result<(), PermissionError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PermissionError::UnexpectedType(path.to_owned()));
    }
    #[cfg(unix)]
    {
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(PermissionError::Owner(path.to_owned()));
        }
        if metadata.mode() & 0o7777 != expected_mode {
            return Err(PermissionError::Mode(path.to_owned()));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("failed to inspect managed path")]
    Io(#[from] std::io::Error),
    #[error("managed path has an unexpected type: {0}")]
    UnexpectedType(std::path::PathBuf),
    #[error("managed path is not owned by the service user: {0}")]
    Owner(std::path::PathBuf),
    #[error("managed path is accessible by group or other: {0}")]
    Mode(std::path::PathBuf),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn rejects_symlink_in_managed_directory_path() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(matches!(
            ensure_private_directory(&link.join("nested")),
            Err(PermissionError::UnexpectedType(_))
        ));
    }

    #[test]
    fn exact_file_mode_is_enforced_and_legacy_modes_are_normalized() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("managed");
        fs::write(&path, "value").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(normalize_service_owned_file_mode(
            &path,
            MANAGED_FILE_MODE,
            &LEGACY_MANAGED_FILE_MODES,
        )
        .unwrap());
        assert!(
            !normalize_service_owned_file_mode(
                &path,
                MANAGED_FILE_MODE,
                &LEGACY_MANAGED_FILE_MODES,
            )
            .unwrap()
        );
        check_service_owned_file_mode(&path, MANAGED_FILE_MODE).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            normalize_service_owned_file_mode(&path, MANAGED_FILE_MODE, &LEGACY_MANAGED_FILE_MODES,),
            Err(PermissionError::Mode(_))
        ));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn exact_file_mode_rejects_special_permission_bits() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("managed");
        fs::write(&path, "value").unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o1444)).unwrap();
        assert!(matches!(
            check_service_owned_file_mode(&path, MANAGED_FILE_MODE),
            Err(PermissionError::Mode(_))
        ));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o1600)).unwrap();
        assert!(matches!(
            normalize_service_owned_file_mode(&path, MANAGED_FILE_MODE, &LEGACY_MANAGED_FILE_MODES,),
            Err(PermissionError::Mode(_))
        ));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            0o1600
        );
    }
}
