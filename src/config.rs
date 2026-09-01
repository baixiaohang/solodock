use std::{
    env, fs,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/solodock/config.toml";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub schema_version: u32,
    pub listen_address: String,
    pub public_origin: String,
    #[serde(default)]
    pub webhook_public_origin: Option<String>,
    pub state_directory: PathBuf,
    pub runtime_directory: PathBuf,
    #[serde(default)]
    pub allowed_bind_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_address: SocketAddr,
    pub public_origin: String,
    pub webhook_public_origin: Option<String>,
    pub state_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub allowed_bind_roots: Vec<PathBuf>,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let path = env::var_os("SOLODOCK_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        check_config_file(path)?;
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let raw: ConfigFile = toml::from_str(&contents).map_err(ConfigError::Parse)?;
        Self::try_from(raw)
    }

    pub fn database_path(&self) -> PathBuf {
        self.state_directory.join("state.sqlite3")
    }

    pub fn apps_directory(&self) -> PathBuf {
        self.state_directory.join("apps")
    }

    pub fn bootstrap_token_path(&self) -> PathBuf {
        self.runtime_directory.join("bootstrap.token")
    }

    pub fn validate_docker_root(&self, docker_root: Option<&str>) -> Result<(), ConfigError> {
        self.validate_docker_root_with_bind_roots(docker_root, &self.allowed_bind_roots)
    }

    pub fn validate_docker_root_with_bind_roots(
        &self,
        docker_root: Option<&str>,
        allowed_bind_roots: &[PathBuf],
    ) -> Result<(), ConfigError> {
        let Some(root) = docker_root else {
            return Ok(());
        };
        let root = Path::new(root);
        if !root.is_absolute()
            || allowed_bind_roots
                .iter()
                .any(|allowed| paths_overlap(allowed, root))
        {
            return Err(ConfigError::BindRootSensitive);
        }
        Ok(())
    }

    pub fn validate_bootstrap_bind_roots(
        &self,
        docker_root: Option<&str>,
    ) -> Result<Vec<PathBuf>, ConfigError> {
        validate_bind_roots(
            &self.allowed_bind_roots,
            &self.state_directory,
            &self.runtime_directory,
            docker_root,
        )
    }
}

impl TryFrom<ConfigFile> for Config {
    type Error = ConfigError;

    fn try_from(raw: ConfigFile) -> Result<Self, Self::Error> {
        if raw.schema_version != 1 {
            return Err(ConfigError::SchemaVersion(raw.schema_version));
        }
        let listen_address = raw
            .listen_address
            .parse::<SocketAddr>()
            .map_err(|_| ConfigError::ListenAddress(raw.listen_address.clone()))?;
        if !listen_address.ip().is_loopback() {
            return Err(ConfigError::NonLoopback(listen_address));
        }
        let public_origin = normalize_public_origin(&raw.public_origin)?;
        let webhook_public_origin = raw
            .webhook_public_origin
            .as_deref()
            .map(normalize_public_origin)
            .transpose()?;
        if webhook_public_origin.as_ref() == Some(&public_origin) {
            return Err(ConfigError::WebhookOriginConflict);
        }
        validate_managed_path("state_directory", &raw.state_directory)?;
        validate_managed_path("runtime_directory", &raw.runtime_directory)?;
        let mut allowed_bind_roots = Vec::with_capacity(raw.allowed_bind_roots.len());
        for root in &raw.allowed_bind_roots {
            // Deprecated TOML roots are bootstrap candidates. Filesystem and
            // Docker data-root checks run only after SQLite proves it has not
            // already imported them.
            validate_managed_path("allowed_bind_roots", root)?;
            allowed_bind_roots.push(root.clone());
        }
        allowed_bind_roots.sort();
        allowed_bind_roots.dedup();
        Ok(Self {
            listen_address,
            public_origin,
            webhook_public_origin,
            state_directory: raw.state_directory,
            runtime_directory: raw.runtime_directory,
            allowed_bind_roots,
        })
    }
}

pub(crate) fn validate_bind_roots(
    roots: &[PathBuf],
    state_directory: &Path,
    runtime_directory: &Path,
    docker_root: Option<&str>,
) -> Result<Vec<PathBuf>, ConfigError> {
    let mut validated = roots
        .iter()
        .map(|root| validate_bind_root(root, state_directory, runtime_directory))
        .collect::<Result<Vec<_>, _>>()?;
    validated.sort();
    validated.dedup();
    if let Some(root) = docker_root {
        let root = Path::new(root);
        if !root.is_absolute() || validated.iter().any(|allowed| paths_overlap(allowed, root)) {
            return Err(ConfigError::BindRootSensitive);
        }
    }
    Ok(validated)
}

pub(crate) fn validate_bind_root(
    root: &Path,
    state_directory: &Path,
    runtime_directory: &Path,
) -> Result<PathBuf, ConfigError> {
    validate_managed_path("allowed_bind_roots", root)?;
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
        root == sensitive || (sensitive != Path::new("/") && root.starts_with(sensitive))
    }) || paths_overlap(root, state_directory)
        || paths_overlap(root, runtime_directory)
    {
        return Err(ConfigError::BindRootSensitive);
    }
    let mut current = PathBuf::from("/");
    for component in root.components().skip(1) {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|source| ConfigError::BindRoot {
            path: root.to_owned(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ConfigError::BindRootSymlink(root.to_owned()));
        }
    }
    if !fs::metadata(root)
        .map_err(|source| ConfigError::BindRoot {
            path: root.to_owned(),
            source,
        })?
        .is_dir()
    {
        return Err(ConfigError::BindRootNotDirectory(root.to_owned()));
    }
    Ok(root.to_owned())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

pub fn normalize_public_origin(value: &str) -> Result<String, ConfigError> {
    let parsed = Url::parse(value).map_err(|_| ConfigError::PublicOrigin)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigError::PublicOrigin);
    }
    let host = parsed.host_str().ok_or(ConfigError::PublicOrigin)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_ascii_lowercase()
    };
    Ok(match parsed.port() {
        Some(port) if port != 443 => format!("https://{host}:{port}"),
        _ => format!("https://{host}"),
    })
}

pub fn origin_authority(origin: &str) -> Result<String, ConfigError> {
    let parsed = Url::parse(origin).map_err(|_| ConfigError::PublicOrigin)?;
    let host = parsed.host_str().ok_or(ConfigError::PublicOrigin)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_ascii_lowercase()
    };
    Ok(match parsed.port() {
        Some(port) if port != 443 => format!("{host}:{port}"),
        _ => host,
    })
}

fn validate_managed_path(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    if !path.is_absolute()
        || !is_lexically_normal(path)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(ConfigError::ManagedPath(field));
    }
    Ok(())
}

#[cfg(unix)]
fn is_lexically_normal(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    bytes.len() > 1
        && bytes[0] == b'/'
        && bytes[1..]
            .split(|byte| *byte == b'/')
            .all(|segment| !segment.is_empty() && segment != b"." && segment != b"..")
}

#[cfg(not(unix))]
fn is_lexically_normal(path: &Path) -> bool {
    let normalized: PathBuf = path.components().collect();
    normalized == path
}

#[cfg(unix)]
fn check_config_file(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != 0 && metadata.uid() != effective_uid {
        return Err(ConfigError::ConfigOwner);
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(ConfigError::ConfigMode);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_config_file(path: &Path) -> Result<(), ConfigError> {
    fs::metadata(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("unsupported configuration schema_version {0}")]
    SchemaVersion(u32),
    #[error("listen_address must be an explicit IP socket address")]
    ListenAddress(String),
    #[error("listen_address must use a loopback IP: {0}")]
    NonLoopback(SocketAddr),
    #[error("public_origin must be a canonical HTTPS origin")]
    PublicOrigin,
    #[error("webhook_public_origin must use a different authority from public_origin")]
    WebhookOriginConflict,
    #[error("{0} must be an absolute normalized path")]
    ManagedPath(&'static str),
    #[error("allowed bind root is sensitive or overlaps SoloDock state")]
    BindRootSensitive,
    #[error("allowed bind root contains a symlink: {0}")]
    BindRootSymlink(PathBuf),
    #[error("allowed bind root is not a directory: {0}")]
    BindRootNotDirectory(PathBuf),
    #[error("failed to inspect allowed bind root {path}: {source}")]
    BindRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration file must be owned by root or the current user")]
    ConfigOwner,
    #[error("configuration file must not be group or other writable")]
    ConfigMode,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn raw() -> ConfigFile {
        ConfigFile {
            schema_version: 1,
            listen_address: "127.0.0.1:8080".into(),
            public_origin: "https://Example.COM:443".into(),
            webhook_public_origin: None,
            state_directory: "/var/lib/solodock".into(),
            runtime_directory: "/run/solodock".into(),
            allowed_bind_roots: Vec::new(),
        }
    }

    #[test]
    fn validates_and_normalizes_configuration() {
        let config = Config::try_from(raw()).unwrap();
        assert_eq!(config.public_origin, "https://example.com");
    }

    #[test]
    fn webhook_origin_is_optional_canonical_and_separate() {
        let mut value = raw();
        value.webhook_public_origin = Some("https://Hooks.Example.COM:443".into());
        let config = Config::try_from(value).unwrap();
        assert_eq!(
            config.webhook_public_origin.as_deref(),
            Some("https://hooks.example.com")
        );
        assert_eq!(
            origin_authority(config.webhook_public_origin.as_deref().unwrap()).unwrap(),
            "hooks.example.com"
        );
        let mut value = raw();
        value.webhook_public_origin = Some("https://example.com".into());
        assert!(matches!(
            Config::try_from(value),
            Err(ConfigError::WebhookOriginConflict)
        ));
    }

    #[test]
    fn rejects_non_default_docker_root_overlapping_bind_allowlist() {
        let temporary = tempfile::tempdir().unwrap();
        let allowed = temporary.path().join("shared");
        fs::create_dir(&allowed).unwrap();
        let mut value = raw();
        value.allowed_bind_roots = vec![allowed.clone()];
        value.state_directory = temporary.path().join("state");
        value.runtime_directory = temporary.path().join("runtime");
        let config = Config::try_from(value).unwrap();
        assert!(matches!(
            config.validate_docker_root(allowed.join("docker-data").to_str()),
            Err(ConfigError::BindRootSensitive)
        ));
    }

    #[test]
    fn packaged_example_matches_the_strict_schema() {
        let raw: ConfigFile = toml::from_str(include_str!("../packaging/solodock.toml.example"))
            .expect("packaged example must parse");
        Config::try_from(raw).expect("packaged example must validate");
    }

    #[test]
    fn rejects_non_loopback_and_ambiguous_paths() {
        let mut value = raw();
        value.listen_address = "[::]:8080".into();
        assert!(matches!(
            Config::try_from(value),
            Err(ConfigError::NonLoopback(_))
        ));
        let mut value = raw();
        value.state_directory = "/var/lib/../tmp".into();
        assert!(matches!(
            Config::try_from(value),
            Err(ConfigError::ManagedPath("state_directory"))
        ));
        let mut value = raw();
        value.runtime_directory = "/run/./solodock".into();
        assert!(matches!(
            Config::try_from(value),
            Err(ConfigError::ManagedPath("runtime_directory"))
        ));
    }

    #[test]
    fn rejects_origin_path_and_unknown_field() {
        let mut value = raw();
        value.public_origin = "https://example.com/admin".into();
        assert!(matches!(
            Config::try_from(value),
            Err(ConfigError::PublicOrigin)
        ));

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "schema_version=1\nlisten_address='127.0.0.1:8080'\npublic_origin='https://example.com'\nstate_directory='/tmp/state'\nruntime_directory='/tmp/run'\ntypo=true"
        )
        .unwrap();
        assert!(matches!(
            Config::load_from(file.path()),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn rejects_group_writable_config_file() {
        use std::os::unix::fs::PermissionsExt;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "schema_version=1").unwrap();
        fs::set_permissions(file.path(), fs::Permissions::from_mode(0o620)).unwrap();
        assert!(matches!(
            Config::load_from(file.path()),
            Err(ConfigError::ConfigMode)
        ));
    }

    #[test]
    fn rejects_sensitive_descendants_and_bidirectional_state_overlap() {
        for root in ["/etc/ssh", "/var/lib/docker/volumes/example"] {
            let mut value = raw();
            value.allowed_bind_roots = vec![root.into()];
            let config = Config::try_from(value).unwrap();
            assert!(matches!(
                config.validate_bootstrap_bind_roots(None),
                Err(ConfigError::BindRootSensitive)
            ));
        }

        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state/solodock");
        let runtime = temporary.path().join("runtime/solodock");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        let mut value = raw();
        value.state_directory = state;
        value.runtime_directory = runtime;
        value.allowed_bind_roots = vec![temporary.path().to_owned()];
        let config = Config::try_from(value).unwrap();
        assert!(matches!(
            config.validate_bootstrap_bind_roots(None),
            Err(ConfigError::BindRootSensitive)
        ));
    }

    #[test]
    fn deprecated_bind_roots_are_parsed_without_touching_stale_host_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let stale = temporary.path().join("removed-after-sqlite-import");
        let mut value = raw();
        value.state_directory = temporary.path().join("state");
        value.runtime_directory = temporary.path().join("runtime");
        value.allowed_bind_roots = vec![stale.clone()];

        let config = Config::try_from(value).unwrap();
        assert_eq!(config.allowed_bind_roots, [stale]);
        assert!(matches!(
            config.validate_bootstrap_bind_roots(None),
            Err(ConfigError::BindRoot { .. })
        ));
    }
}
