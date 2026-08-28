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
    pub state_directory: PathBuf,
    pub runtime_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_address: SocketAddr,
    pub public_origin: String,
    pub state_directory: PathBuf,
    pub runtime_directory: PathBuf,
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
        validate_managed_path("state_directory", &raw.state_directory)?;
        validate_managed_path("runtime_directory", &raw.runtime_directory)?;
        Ok(Self {
            listen_address,
            public_origin,
            state_directory: raw.state_directory,
            runtime_directory: raw.runtime_directory,
        })
    }
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
    #[error("{0} must be an absolute normalized path")]
    ManagedPath(&'static str),
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
            state_directory: "/var/lib/solodock".into(),
            runtime_directory: "/run/solodock".into(),
        }
    }

    #[test]
    fn validates_and_normalizes_configuration() {
        let config = Config::try_from(raw()).unwrap();
        assert_eq!(config.public_origin, "https://example.com");
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
}
