use serde::{Deserialize, Serialize};

use super::error::RegistryError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageReference {
    pub logical_registry: String,
    pub transport_registry: String,
    pub repository: String,
    pub tag: String,
    pub canonical_tagged_ref: String,
    pub docker_auth_key: String,
}

impl ImageReference {
    pub fn parse(value: &str) -> Result<Self, RegistryError> {
        if value.is_empty()
            || value.len() > 320
            || value.contains(['@', '$', '{', '}', '`', '\\', '"', '\'', '?', '#'])
            || value.contains("://")
            || value
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            return Err(RegistryError::ReferenceInvalid);
        }
        let slash = value.rfind('/');
        let colon = value.rfind(':').ok_or(RegistryError::ReferenceInvalid)?;
        if slash.is_some_and(|slash| colon < slash) {
            return Err(RegistryError::ReferenceInvalid);
        }
        let repository_part = &value[..colon];
        let tag = &value[colon + 1..];
        if tag.is_empty()
            || tag.len() > 128
            || !tag.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphanumeric()
                    || byte == b'_'
                    || (index > 0 && matches!(byte, b'.' | b'-'))
            })
        {
            return Err(RegistryError::ReferenceInvalid);
        }
        let mut parts = repository_part.split('/');
        let first = parts.next().ok_or(RegistryError::ReferenceInvalid)?;
        let first_is_registry = first.contains('.') || first.contains(':') || first == "localhost";
        let (logical_registry, transport_registry, repository) = if first_is_registry {
            validate_registry(first)?;
            let mut rest = parts.collect::<Vec<_>>().join("/");
            if matches!(first, "docker.io" | "index.docker.io") && !rest.contains('/') {
                rest = format!("library/{rest}");
            }
            validate_repository(&rest)?;
            if first == "registry-1.docker.io" {
                return Err(RegistryError::ReferenceInvalid);
            }
            if matches!(first, "docker.io" | "index.docker.io") {
                ("docker.io".into(), "registry-1.docker.io".into(), rest)
            } else {
                (first.to_owned(), first.to_owned(), rest)
            }
        } else {
            let rest = std::iter::once(first)
                .chain(parts)
                .collect::<Vec<_>>()
                .join("/");
            validate_repository(&rest)?;
            let repository = if rest.contains('/') {
                rest
            } else {
                format!("library/{rest}")
            };
            (
                "docker.io".into(),
                "registry-1.docker.io".into(),
                repository,
            )
        };
        let canonical_tagged_ref = if logical_registry == "docker.io" {
            format!("docker.io/{repository}:{tag}")
        } else {
            format!("{logical_registry}/{repository}:{tag}")
        };
        let docker_auth_key = if logical_registry == "docker.io" {
            "https://index.docker.io/v1/".into()
        } else {
            logical_registry.clone()
        };
        Ok(Self {
            logical_registry,
            transport_registry,
            repository,
            tag: tag.into(),
            canonical_tagged_ref,
            docker_auth_key,
        })
    }

    pub fn runnable(&self, digest: &str) -> Result<String, RegistryError> {
        validate_digest(digest)?;
        let prefix = if self.logical_registry == "docker.io" {
            format!("docker.io/{}", self.repository)
        } else {
            format!("{}/{}", self.logical_registry, self.repository)
        };
        Ok(format!("{prefix}@{digest}"))
    }
}

pub fn validate_digest(value: &str) -> Result<(), RegistryError> {
    let hex = value
        .strip_prefix("sha256:")
        .ok_or(RegistryError::DigestMismatch)?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(RegistryError::DigestMismatch);
    }
    Ok(())
}

pub fn validate_logical_registry(value: &str) -> Result<String, RegistryError> {
    if value == "registry-1.docker.io" {
        return Err(RegistryError::ReferenceInvalid);
    }
    validate_registry(value)?;
    Ok(if matches!(value, "index.docker.io" | "docker.io") {
        "docker.io".into()
    } else {
        value.into()
    })
}

fn validate_registry(value: &str) -> Result<(), RegistryError> {
    if value.is_empty() || value.len() > 255 || value.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(RegistryError::ReferenceInvalid);
    }
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(h, p)| (h, Some(p)));
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        || port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(RegistryError::ReferenceInvalid);
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<(), RegistryError> {
    if value.is_empty()
        || value.len() > 255
        || value.split('/').any(|part| {
            part.is_empty()
                || !part.as_bytes()[0].is_ascii_alphanumeric()
                || !part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                || !part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                })
        })
    {
        return Err(RegistryError::ReferenceInvalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_docker_hub_and_custom_references() {
        let image = ImageReference::parse("postgres:16-alpine").unwrap();
        assert_eq!(image.logical_registry, "docker.io");
        assert_eq!(image.transport_registry, "registry-1.docker.io");
        assert_eq!(image.repository, "library/postgres");
        assert_eq!(
            image.canonical_tagged_ref,
            "docker.io/library/postgres:16-alpine"
        );
        let custom = ImageReference::parse("registry.example:5443/team/app:v1").unwrap();
        assert_eq!(custom.repository, "team/app");
        assert!(ImageReference::parse("UPPER/app:v1").is_err());
        assert!(ImageReference::parse("app@sha256:deadbeef").is_err());
    }
}
