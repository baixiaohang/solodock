use reqwest::header::HeaderValue;
use url::Url;

use super::error::RegistryError;

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
