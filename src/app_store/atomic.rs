use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, fs::symlink},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{StoreError, sync_directory};
use crate::security::permissions::check_private;

const TEMP_PREFIX: &str = ".solodock-tmp-";

pub fn is_internal_temp_name(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(TEMP_PREFIX)
}

pub struct AtomicWriter;

impl AtomicWriter {
    pub fn write(path: &Path, contents: &[u8], mode: u32) -> Result<(), StoreError> {
        Self::write_with_before_rename(path, contents, mode, || Ok(()))
    }

    fn write_with_before_rename(
        path: &Path,
        contents: &[u8],
        mode: u32,
        before_rename: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let parent = path.parent().ok_or_else(|| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed file must have a parent",
            ))
        })?;
        let temp = parent.join(format!("{TEMP_PREFIX}{}", Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp)?;
            file.write_all(contents)?;
            file.flush()?;
            file.sync_all()?;
            before_rename()?;
            fs::rename(&temp, path)?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub fn publish_release(
        releases: &Path,
        release_id: Uuid,
        release_toml: &[u8],
        compose_yaml: &[u8],
    ) -> Result<PathBuf, StoreError> {
        let temp = releases.join(format!("{TEMP_PREFIX}{}", Uuid::new_v4().simple()));
        let target = releases.join(release_id.to_string());
        let result = (|| {
            let mut builder = fs::DirBuilder::new();
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700).create(&temp)?;
            Self::write(&temp.join("release.toml"), release_toml, 0o600)?;
            Self::write(&temp.join("compose.yaml"), compose_yaml, 0o600)?;
            sync_directory(&temp)?;
            rename_no_replace(&temp, &target)?;
            sync_directory(releases)?;
            Ok(target.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temp);
        }
        result
    }

    pub fn switch_release_link(
        app_directory: &Path,
        link_name: &str,
        release_id: Uuid,
    ) -> Result<(), StoreError> {
        if link_name != "active" && link_name != "pending" {
            return Err(StoreError::SymlinkBoundary);
        }
        check_private(app_directory, true)?;
        let releases_directory = app_directory.join("releases");
        check_private(&releases_directory, true)?;
        let target = PathBuf::from("releases").join(release_id.to_string());
        check_private(&app_directory.join(&target), true)?;
        let temporary = app_directory.join(format!("{TEMP_PREFIX}{}", Uuid::new_v4().simple()));
        let final_path = app_directory.join(link_name);
        let result = (|| {
            symlink(&target, &temporary)?;
            fs::rename(&temporary, &final_path)?;
            sync_directory(app_directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn remove_release_link(app_directory: &Path, link_name: &str) -> Result<(), StoreError> {
        if link_name != "active" && link_name != "pending" {
            return Err(StoreError::SymlinkBoundary);
        }
        let path = app_directory.join(link_name);
        match fs::remove_file(path) {
            Ok(()) => sync_directory(app_directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn rename_no_replace(source: &Path, target: &Path) -> Result<(), StoreError> {
    let source =
        CString::new(source.as_os_str().as_bytes()).map_err(|_| StoreError::SymlinkBoundary)?;
    let target =
        CString::new(target.as_os_str().as_bytes()).map_err(|_| StoreError::SymlinkBoundary)?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        Err(StoreError::ReleaseConflict)
    } else {
        Err(StoreError::Io(error))
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn rename_no_replace(source: &Path, target: &Path) -> Result<(), StoreError> {
    if target.exists() {
        return Err(StoreError::ReleaseConflict);
    }
    fs::rename(source, target).map_err(StoreError::Io)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn atomic_write_replaces_target_and_ignores_unrelated_temp() {
        let root = tempdir().unwrap();
        let target = root.path().join("app.toml");
        fs::write(&target, b"old").unwrap();
        fs::write(root.path().join(".solodock-tmp-interrupted"), b"partial").unwrap();
        AtomicWriter::write(&target, b"new", 0o600).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new");
        assert!(root.path().join(".solodock-tmp-interrupted").exists());
    }

    #[test]
    fn interrupted_write_preserves_existing_target() {
        let root = tempdir().unwrap();
        let target = root.path().join("app.toml");
        fs::write(&target, b"old").unwrap();
        let result = AtomicWriter::write_with_before_rename(&target, b"new", 0o600, || {
            Err(StoreError::Io(std::io::Error::other("injected failure")))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter(|entry| is_internal_temp_name(&entry.as_ref().unwrap().file_name()))
                .count(),
            0
        );
    }

    #[test]
    fn release_is_immutable() {
        let root = tempdir().unwrap();
        let id = Uuid::new_v4();
        AtomicWriter::publish_release(root.path(), id, b"one", b"compose").unwrap();
        assert!(matches!(
            AtomicWriter::publish_release(root.path(), id, b"two", b"compose"),
            Err(StoreError::ReleaseConflict)
        ));
        assert_eq!(
            fs::read(root.path().join(id.to_string()).join("release.toml")).unwrap(),
            b"one"
        );
    }

    #[test]
    fn switches_and_removes_only_managed_release_links() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        let releases = root.path().join("releases");
        let release = releases.join(id.to_string());
        fs::create_dir_all(&release).unwrap();
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&release, fs::Permissions::from_mode(0o700)).unwrap();
        AtomicWriter::switch_release_link(root.path(), "active", id).unwrap();
        assert_eq!(
            fs::read_link(root.path().join("active")).unwrap(),
            PathBuf::from("releases").join(id.to_string())
        );
        AtomicWriter::remove_release_link(root.path(), "active").unwrap();
        assert!(!root.path().join("active").exists());
        assert!(AtomicWriter::switch_release_link(root.path(), "other", id).is_err());
    }

    #[test]
    fn refuses_release_directory_symlink_as_link_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let releases = root.path().join("releases");
        fs::create_dir(&releases).unwrap();
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        symlink(&outside, releases.join(id.to_string())).unwrap();
        assert!(AtomicWriter::switch_release_link(root.path(), "active", id).is_err());
        assert!(!root.path().join("active").exists());
    }
}
