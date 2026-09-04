use reqwest::header::HeaderValue;
use url::Url;

use super::error::RegistryError;
use super::reference::ImageReference;

const DOCKER_HUB_TOKEN_ENDPOINT: &str = "https://auth.docker.io/token";

#[derive(Debug)]
pub struct BearerChallenge {
    pub realm: Url,
    pub service: Option<String>,
}

pub fn parse_bearer(
    value: &HeaderValue,
    allow_http_loopback: bool,
) -> Result<BearerChallenge, RegistryError> {
    let value = value.to_str().map_err(|_| RegistryError::Protocol)?;
    let parameters = value
        .strip_prefix("Bearer ")
        .ok_or(RegistryError::AuthUnsupported)?;
    if parameters.len() > 2048 {
        return Err(RegistryError::Protocol);
    }
    let mut realm = None;
    let mut service = None;
    for item in parameters.split(',') {
        let (key, raw) = item.trim().split_once('=').ok_or(RegistryError::Protocol)?;
        let raw = raw
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .ok_or(RegistryError::Protocol)?;
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err(RegistryError::Protocol);
        }
        match key {
            "realm" if realm.is_none() => realm = Some(raw),
            "service" if service.is_none() && raw.len() <= 255 => service = Some(raw.to_owned()),
            "scope" => {}
            _ => return Err(RegistryError::Protocol),
        }
    }
    let realm =
        Url::parse(realm.ok_or(RegistryError::Protocol)?).map_err(|_| RegistryError::Protocol)?;
    let loopback_http = allow_http_loopback
        && realm.scheme() == "http"
        && matches!(realm.host_str(), Some("127.0.0.1" | "localhost"));
    if (realm.scheme() != "https" && !loopback_http)
        || !realm.username().is_empty()
        || realm.password().is_some()
        || realm.query().is_some()
        || realm.fragment().is_some()
    {
        return Err(RegistryError::Protocol);
    }
    Ok(BearerChallenge { realm, service })
}

/// Verifies that a bearer token endpoint is authorized to receive credentials for this image.
///
/// Custom registries may only use their own origin. Docker Hub has one explicit cross-origin
/// exception for its official token endpoint.
pub fn validate_bearer_realm(
    image: &ImageReference,
    realm: &Url,
    allow_http_loopback: bool,
) -> Result<(), RegistryError> {
    let scheme = if allow_http_loopback
        && (image.transport_registry.starts_with("127.0.0.1:")
            || image.transport_registry.starts_with("localhost:"))
    {
        "http"
    } else {
        "https"
    };
    let registry_origin = Url::parse(&format!("{scheme}://{}/", image.transport_registry))
        .map_err(|_| RegistryError::Protocol)?;
    let same_registry_origin = realm.scheme() == registry_origin.scheme()
        && realm.host_str() == registry_origin.host_str()
        && realm.port_or_known_default() == registry_origin.port_or_known_default();
    let docker_hub_exception =
        image.logical_registry == "docker.io" && realm.as_str() == DOCKER_HUB_TOKEN_ENDPOINT;

    if same_registry_origin || docker_hub_exception {
        Ok(())
    } else {
        Err(RegistryError::AuthRealmUntrusted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_registry_realm_must_match_exact_origin() {
        let image = ImageReference::parse("registry.example:5443/team/app:v1").unwrap();
        assert!(
            validate_bearer_realm(
                &image,
                &Url::parse("https://registry.example:5443/oauth/token").unwrap(),
                false,
            )
            .is_ok()
        );
        for realm in [
            "https://registry.example/oauth/token",
            "https://registry.example.evil.test/oauth/token",
            "https://auth.registry.example:5443/oauth/token",
            "http://registry.example:5443/oauth/token",
        ] {
            assert!(matches!(
                validate_bearer_realm(&image, &Url::parse(realm).unwrap(), false),
                Err(RegistryError::AuthRealmUntrusted)
            ));
        }
    }

    #[test]
    fn docker_hub_exception_is_limited_to_the_exact_official_endpoint() {
        let image = ImageReference::parse("postgres:16").unwrap();
        assert!(
            validate_bearer_realm(
                &image,
                &Url::parse(DOCKER_HUB_TOKEN_ENDPOINT).unwrap(),
                false,
            )
            .is_ok()
        );
        for realm in [
            "https://auth.docker.io/token/",
            "https://auth.docker.io:444/token",
            "https://evil.docker.io/token",
            "https://auth.docker.io.evil.test/token",
        ] {
            assert!(matches!(
                validate_bearer_realm(&image, &Url::parse(realm).unwrap(), false),
                Err(RegistryError::AuthRealmUntrusted)
            ));
        }
    }

    #[test]
    fn test_http_realm_still_requires_the_registry_origin() {
        let image = ImageReference::parse("127.0.0.1:5000/team/app:v1").unwrap();
        assert!(
            validate_bearer_realm(
                &image,
                &Url::parse("http://127.0.0.1:5000/token").unwrap(),
                true,
            )
            .is_ok()
        );
        assert!(matches!(
            validate_bearer_realm(
                &image,
                &Url::parse("http://localhost:5000/token").unwrap(),
                true,
            ),
            Err(RegistryError::AuthRealmUntrusted)
        ));
    }
}
