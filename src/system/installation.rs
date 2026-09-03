use std::{fs, path::Path};

use serde::Serialize;

const MANAGED_BINARY: &str = "/usr/local/bin/solodock";
const MANAGED_DIRECTORY_PREFIX: &str = "/usr/local/lib/solodock/";
const MANIFEST_NAME: &str = "INSTALL_MANIFEST";
const MANIFEST_FORMAT: &str = "solodock-install-v1";
const MAX_MANIFEST_BYTES: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallationChannel {
    Stable,
    Main,
    Development,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallationIdentity {
    pub channel: InstallationChannel,
    pub version: Option<String>,
    pub source_sha: Option<String>,
    pub package_identity: Option<String>,
}

impl InstallationIdentity {
    fn development() -> Self {
        let source_sha = option_env!("SOLODOCK_SOURCE_SHA")
            .filter(|value| is_lower_hex(value, 40))
            .map(str::to_owned);
        Self {
            channel: InstallationChannel::Development,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            source_sha,
            package_identity: None,
        }
    }

    fn unknown() -> Self {
        Self {
            channel: InstallationChannel::Unknown,
            version: None,
            source_sha: None,
            package_identity: None,
        }
    }
}

pub fn read() -> InstallationIdentity {
    let executable = std::env::current_exe().unwrap_or_default();
    read_from(Path::new("/"), &executable)
}

fn read_from(root: &Path, executable: &Path) -> InstallationIdentity {
    let managed_bin_directory = rooted(root, "/usr/local/bin");
    let managed_lib_directory = rooted(root, "/usr/local/lib/solodock");
    let link_path = rooted(root, MANAGED_BINARY);
    let link_metadata = match fs::symlink_metadata(&link_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if executable.to_str().is_some_and(|path| {
                path == MANAGED_BINARY || path.starts_with(MANAGED_DIRECTORY_PREFIX)
            }) {
                InstallationIdentity::unknown()
            } else {
                InstallationIdentity::development()
            };
        }
        Err(_) => return InstallationIdentity::unknown(),
    };
    if !link_metadata.file_type().is_symlink() {
        return InstallationIdentity::unknown();
    }
    if !is_plain_directory(&managed_bin_directory) || !is_plain_directory(&managed_lib_directory) {
        return InstallationIdentity::unknown();
    }
    let Ok(target) = fs::read_link(&link_path) else {
        return InstallationIdentity::unknown();
    };
    let Some(target) = target.to_str() else {
        return InstallationIdentity::unknown();
    };
    let Some(relative_target) = target
        .strip_prefix(MANAGED_DIRECTORY_PREFIX)
        .and_then(|value| value.strip_suffix("/solodock"))
    else {
        return InstallationIdentity::unknown();
    };
    let (version_directory, generation) = if let Some(value) = relative_target
        .strip_prefix("generations/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        (value, true)
    } else if !relative_target.is_empty() && !relative_target.contains('/') {
        (relative_target, false)
    } else {
        return InstallationIdentity::unknown();
    };

    let managed_path = if generation {
        format!("{MANAGED_DIRECTORY_PREFIX}generations/{version_directory}")
    } else {
        format!("{MANAGED_DIRECTORY_PREFIX}{version_directory}")
    };
    let version_path = rooted(root, &managed_path);
    let binary_path = version_path.join("solodock");
    let manifest_path = version_path.join(MANIFEST_NAME);
    if !is_plain_directory(&version_path)
        || !is_plain_file(&binary_path)
        || !is_plain_file(&manifest_path)
    {
        return InstallationIdentity::unknown();
    }
    let Ok(metadata) = fs::metadata(&manifest_path) else {
        return InstallationIdentity::unknown();
    };
    if metadata.len() > MAX_MANIFEST_BYTES {
        return InstallationIdentity::unknown();
    }
    let Ok(contents) = fs::read_to_string(&manifest_path) else {
        return InstallationIdentity::unknown();
    };
    parse_manifest(&contents, version_directory, generation)
        .unwrap_or_else(InstallationIdentity::unknown)
}

fn parse_manifest(
    contents: &str,
    version_directory: &str,
    generation: bool,
) -> Option<InstallationIdentity> {
    if !contents.is_ascii() || contents.contains('\r') || contents.as_bytes().contains(&0) {
        return None;
    }
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.len() != 5 || lines[0] != format!("FORMAT={MANIFEST_FORMAT}") {
        return None;
    }
    let channel = match lines[1].strip_prefix("CHANNEL=")? {
        "stable" => InstallationChannel::Stable,
        "main" => InstallationChannel::Main,
        _ => return None,
    };
    let stored_version = lines[2].strip_prefix("VERSION=")?;
    let source_sha = lines[3].strip_prefix("SOURCE_SHA=")?;
    let package_identity = lines[4].strip_prefix("PACKAGE_IDENTITY=")?;
    if !is_lower_hex(source_sha, 40) || !is_lower_hex(package_identity, 64) {
        return None;
    }
    if generation {
        let prefix = format!("{stored_version}.{package_identity}.");
        let nonce = version_directory.strip_prefix(&prefix)?;
        if nonce.len() != 12 || !nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return None;
        }
    } else if stored_version != version_directory {
        return None;
    }
    let version = match channel {
        InstallationChannel::Stable if is_canonical_semver(stored_version) => {
            stored_version.to_owned()
        }
        InstallationChannel::Main if stored_version == format!("main-{}", &source_sha[..12]) => {
            "main".to_owned()
        }
        _ => return None,
    };
    Some(InstallationIdentity {
        channel,
        version: Some(version),
        source_sha: Some(source_sha.to_owned()),
        package_identity: Some(package_identity.to_owned()),
    })
}

fn rooted(root: &Path, absolute: &str) -> std::path::PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

fn is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_semver(value: &str) -> bool {
    let components = value.split('.').collect::<Vec<_>>();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (*component == "0" || !component.starts_with('0'))
        })
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::symlink, path::Path};

    use tempfile::tempdir;

    use super::{InstallationChannel, read_from};

    fn managed(
        root: &Path,
        version: &str,
        identity: &str,
        manifest: Option<&str>,
    ) -> std::path::PathBuf {
        let generation = format!("{version}.{identity}.abcdefghijkl");
        let directory = root
            .join("usr/local/lib/solodock/generations")
            .join(&generation);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("solodock"), b"binary").unwrap();
        if let Some(manifest) = manifest {
            std::fs::write(directory.join("INSTALL_MANIFEST"), manifest).unwrap();
        }
        std::fs::create_dir_all(root.join("usr/local/bin")).unwrap();
        symlink(
            format!("/usr/local/lib/solodock/generations/{generation}/solodock"),
            root.join("usr/local/bin/solodock"),
        )
        .unwrap();
        directory
    }

    #[test]
    fn reads_stable_and_main_manifests() {
        let stable_root = tempdir().unwrap();
        let stable_directory = managed(
            stable_root.path(),
            "0.2.0",
            &"b".repeat(64),
            Some(&format!(
                "FORMAT=solodock-install-v1\nCHANNEL=stable\nVERSION=0.2.0\nSOURCE_SHA={}\nPACKAGE_IDENTITY={}\n",
                "a".repeat(40),
                "b".repeat(64)
            )),
        );
        let stable = read_from(stable_root.path(), Path::new("/tmp/solodock"));
        assert_eq!(stable.channel, InstallationChannel::Stable);
        assert_eq!(stable.version.as_deref(), Some("0.2.0"));
        assert_eq!(
            stable.source_sha.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let refreshed_identity = "e".repeat(64);
        let refreshed_directory = stable_root
            .path()
            .join("usr/local/lib/solodock/generations")
            .join(format!("0.2.0.{refreshed_identity}.mnopqrstuvwx"));
        std::fs::create_dir_all(&refreshed_directory).unwrap();
        std::fs::write(refreshed_directory.join("solodock"), b"binary").unwrap();
        std::fs::write(
            refreshed_directory.join("INSTALL_MANIFEST"),
            format!(
                "FORMAT=solodock-install-v1\nCHANNEL=stable\nVERSION=0.2.0\nSOURCE_SHA={}\nPACKAGE_IDENTITY={}\n",
                "a".repeat(40),
                refreshed_identity
            ),
        )
        .unwrap();
        std::fs::remove_file(stable_root.path().join("usr/local/bin/solodock")).unwrap();
        symlink(
            format!(
                "/usr/local/lib/solodock/generations/0.2.0.{}.mnopqrstuvwx/solodock",
                "e".repeat(64)
            ),
            stable_root.path().join("usr/local/bin/solodock"),
        )
        .unwrap();
        let refreshed = read_from(stable_root.path(), Path::new("/tmp/solodock"));
        assert_eq!(
            refreshed.package_identity.as_deref(),
            Some(refreshed_identity.as_str())
        );
        assert!(stable_directory.exists());

        let main_root = tempdir().unwrap();
        let _ = managed(
            main_root.path(),
            "main-cccccccccccc",
            &"d".repeat(64),
            Some(&format!(
                "FORMAT=solodock-install-v1\nCHANNEL=main\nVERSION=main-cccccccccccc\nSOURCE_SHA={}\nPACKAGE_IDENTITY={}\n",
                "c".repeat(40),
                "d".repeat(64)
            )),
        );
        let main = read_from(main_root.path(), Path::new("/tmp/solodock"));
        assert_eq!(main.channel, InstallationChannel::Main);
        assert_eq!(main.version.as_deref(), Some("main"));
        assert_eq!(
            main.package_identity.as_deref(),
            Some("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
        );
    }

    #[test]
    fn distinguishes_development_from_damaged_managed_installs() {
        let development_root = tempdir().unwrap();
        let development = read_from(
            development_root.path(),
            Path::new("/workspace/target/debug/solodock"),
        );
        assert_eq!(development.channel, InstallationChannel::Development);
        assert_eq!(
            development.version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );

        let missing_manifest_root = tempdir().unwrap();
        let _ = managed(missing_manifest_root.path(), "0.2.0", &"b".repeat(64), None);
        assert_eq!(
            read_from(missing_manifest_root.path(), Path::new("/tmp/solodock")).channel,
            InstallationChannel::Unknown
        );

        let invalid_root = tempdir().unwrap();
        let _ = managed(
            invalid_root.path(),
            "0.2.0",
            &"b".repeat(64),
            Some(
                "FORMAT=solodock-install-v1\nCHANNEL=stable\nVERSION=0.2.0\nSOURCE_SHA=untrusted\nPACKAGE_IDENTITY=untrusted\n",
            ),
        );
        assert_eq!(
            read_from(invalid_root.path(), Path::new("/tmp/solodock")).channel,
            InstallationChannel::Unknown
        );
    }

    #[test]
    fn rejects_noncanonical_managed_paths_and_manifest_versions() {
        let root = tempdir().unwrap();
        let _ = managed(
            root.path(),
            "01.2.3",
            &"b".repeat(64),
            Some(&format!(
                "FORMAT=solodock-install-v1\nCHANNEL=stable\nVERSION=01.2.3\nSOURCE_SHA={}\nPACKAGE_IDENTITY={}\n",
                "a".repeat(40),
                "b".repeat(64)
            )),
        );
        assert_eq!(
            read_from(root.path(), Path::new("/tmp/solodock")).channel,
            InstallationChannel::Unknown
        );

        let mismatched_identity_root = tempdir().unwrap();
        let _ = managed(
            mismatched_identity_root.path(),
            "0.2.0",
            &"b".repeat(64),
            Some(&format!(
                "FORMAT=solodock-install-v1\nCHANNEL=stable\nVERSION=0.2.0\nSOURCE_SHA={}\nPACKAGE_IDENTITY={}\n",
                "a".repeat(40),
                "c".repeat(64)
            )),
        );
        assert_eq!(
            read_from(mismatched_identity_root.path(), Path::new("/tmp/solodock")).channel,
            InstallationChannel::Unknown
        );
    }
}
