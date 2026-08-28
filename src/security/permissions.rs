use std::{
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

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

pub fn set_private_file_mode(path: &Path) -> Result<(), PermissionError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    check_private(path, false)
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
}
