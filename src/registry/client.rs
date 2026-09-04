use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::Zeroizing;

use super::{
    auth::{parse_bearer, validate_bearer_realm},
    error::RegistryError,
    manifest::{self, DOCKER_LIST, DOCKER_MANIFEST, OCI_INDEX, OCI_MANIFEST},
    reference::ImageReference,
};
use crate::security::secret::SecretValue;

const MANIFEST_LIMIT: usize = 8 * 1024 * 1024;
const CONFIG_LIMIT: usize = 2 * 1024 * 1024;
const TOKEN_LIMIT: usize = 64 * 1024;
const BLOB_REDIRECT_LIMIT: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl Platform {
    pub fn canonical(
        os: &str,
        architecture: &str,
        variant: Option<&str>,
    ) -> Result<Self, RegistryError> {
        let os = os.to_ascii_lowercase();
        if os != "linux" {
            return Err(RegistryError::PlatformNotFound);
        }
        let raw_architecture = architecture.to_ascii_lowercase();
        let (architecture, inferred_variant) = match raw_architecture.as_str() {
            "amd64" | "x86_64" | "x86-64" => ("amd64", None),
            "arm64" | "aarch64" => ("arm64", Some("v8")),
            "armv7" | "armv7l" | "armhf" => ("arm", Some("v7")),
            "arm" => ("arm", None),
            _ => return Err(RegistryError::PlatformNotFound),
        };
        let variant = variant
            .filter(|value| !value.is_empty())
            .or(inferred_variant)
            .map(str::to_ascii_lowercase);
        let variant = match (architecture, variant.as_deref()) {
            ("amd64", None) => None,
            ("arm64", None | Some("v8")) => Some("v8".to_owned()),
            ("arm", Some("v7" | "7")) => Some("v7".to_owned()),
            ("arm", Some("v6" | "6")) => Some("v6".to_owned()),
            _ => return Err(RegistryError::PlatformNotFound),
        };
        Ok(Self {
            os,
            architecture: architecture.to_owned(),
            variant,
        })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResolvedImage {
    pub source_image_ref: String,
    pub logical_registry: String,
    pub repository: String,
    pub source_tag: String,
    pub source_descriptor_digest: String,
    pub index_digest: Option<String>,
    pub manifest_digest: String,
    pub runnable_image_ref: String,
    pub platform: Platform,
    pub local_image_id: String,
}

impl ResolvedImage {
    pub fn image_identity(&self) -> Result<super::ImageIdentity, RegistryError> {
        super::ImageIdentity::new(&self.manifest_digest, &self.local_image_id, &self.platform)
    }
}

#[derive(Clone, Debug)]
pub enum PollResolve {
    NotModified,
    Modified {
        image: Box<ResolvedImage>,
        etag: Option<String>,
    },
}

#[derive(Clone)]
pub struct RegistryResolver {
    client: Client,
    allow_http_loopback: bool,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ExposedPort {
    pub container_port: u16,
    pub protocol: String,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ImageConfigSuggestion {
    pub resolved_digest: String,
    pub exposed_ports: Vec<ExposedPort>,
    pub volume_targets: Vec<String>,
    pub has_healthcheck: bool,
    pub user: Option<String>,
    pub stop_signal: Option<String>,
    pub warnings: Vec<String>,
}

impl RegistryResolver {
    pub fn production() -> Result<Self, RegistryError> {
        let client = Client::builder()
            .user_agent("solodock/0.1")
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| RegistryError::Unavailable)?;
        Ok(Self {
            client,
            allow_http_loopback: false,
        })
    }

    #[cfg(any(test, feature = "docker-e2e"))]
    pub fn for_test_http() -> Result<Self, RegistryError> {
        let mut value = Self::production()?;
        value.allow_http_loopback = true;
        Ok(value)
    }

    pub async fn resolve(
        &self,
        image: &ImageReference,
        platform: &Platform,
        credential: Option<(&str, &SecretValue)>,
    ) -> Result<ResolvedImage, RegistryError> {
        let deadline = tokio::time::timeout(
            Duration::from_secs(30),
            self.resolve_inner(image, platform, credential, None),
        );
        match deadline.await.map_err(|_| RegistryError::Timeout)?? {
            PollResolve::Modified { image, .. } => Ok(*image),
            PollResolve::NotModified => Err(RegistryError::Protocol),
        }
    }

    pub async fn inspect_config(
        &self,
        image: &ImageReference,
        platform: &Platform,
        credential: Option<(&str, &SecretValue)>,
    ) -> Result<ImageConfigSuggestion, RegistryError> {
        let resolved = self.resolve(image, platform, credential).await?;
        let response = self
            .fetch_blob_authenticated(image, &resolved.local_image_id, credential)
            .await?;
        classify_status(response.status(), credential.is_some())?;
        let body = read_limited(response, CONFIG_LIMIT).await?;
        if digest(&body) != resolved.local_image_id {
            return Err(RegistryError::DigestMismatch);
        }
        parse_image_config(&body, resolved.manifest_digest)
    }

    pub async fn resolve_poll(
        &self,
        image: &ImageReference,
        platform: &Platform,
        credential: Option<(&str, &SecretValue)>,
        etag: Option<&str>,
    ) -> Result<PollResolve, RegistryError> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.resolve_inner(image, platform, credential, etag),
        )
        .await
        .map_err(|_| RegistryError::Timeout)?
    }

    async fn resolve_inner(
        &self,
        image: &ImageReference,
        platform: &Platform,
        credential: Option<(&str, &SecretValue)>,
        etag: Option<&str>,
    ) -> Result<PollResolve, RegistryError> {
        let platform = Platform::canonical(
            &platform.os,
            &platform.architecture,
            platform.variant.as_deref(),
        )?;
        let first = self.fetch(image, &image.tag, None, etag).await?;
        let mut bearer: Option<SecretValue> = None;
        let response = if first.status() == StatusCode::UNAUTHORIZED {
            let challenge = first
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .ok_or(RegistryError::CredentialRequired)?;
            if challenge
                .to_str()
                .is_ok_and(|value| value.starts_with("Basic "))
            {
                return Err(RegistryError::AuthUnsupported);
            } else {
                let challenge = parse_bearer(challenge, self.allow_http_loopback)?;
                validate_bearer_realm(image, &challenge.realm, self.allow_http_loopback)?;
                let mut url = challenge.realm;
                {
                    let mut pairs = url.query_pairs_mut();
                    if let Some(service) = challenge.service {
                        pairs.append_pair("service", &service);
                    }
                    pairs.append_pair("scope", &format!("repository:{}:pull", image.repository));
                }
                let mut request = self.client.get(url);
                if let Some((username, secret)) = credential {
                    request = request.basic_auth(username, Some(secret.expose()));
                }
                let token_response = request.send().await.map_err(classify_transport)?;
                classify_status(token_response.status(), credential.is_some())?;
                let token_body = Zeroizing::new(read_limited(token_response, TOKEN_LIMIT).await?);
                #[derive(Deserialize)]
                struct Token {
                    token: Option<SecretValue>,
                    access_token: Option<SecretValue>,
                }
                let token: Token =
                    serde_json::from_slice(&token_body).map_err(|_| RegistryError::Protocol)?;
                let token = token
                    .token
                    .or(token.access_token)
                    .filter(|v| !v.expose().is_empty() && v.expose().len() <= 16 * 1024)
                    .ok_or(RegistryError::CredentialInvalid)?;
                bearer = Some(token);
                self.fetch(
                    image,
                    &image.tag,
                    bearer.as_ref().map(SecretValue::expose),
                    etag,
                )
                .await?
            }
        } else {
            first
        };
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(PollResolve::NotModified);
        }
        classify_status(response.status(), credential.is_some())?;
        let etag = response.headers().get(header::ETAG).and_then(safe_etag);
        let media_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .ok_or(RegistryError::Protocol)?
            .to_owned();
        let declared = response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = read_limited(response, MANIFEST_LIMIT).await?;
        let source_digest = digest(&body);
        if declared.is_some_and(|value| value != source_digest) {
            return Err(RegistryError::DigestMismatch);
        }
        let (index_digest, manifest_digest, local_image_id, selected_platform) =
            match media_type.as_str() {
                OCI_MANIFEST | DOCKER_MANIFEST => {
                    let manifest = manifest::parse_manifest(&body, &media_type)?;
                    (
                        None,
                        source_digest.clone(),
                        manifest.config.digest,
                        platform.clone(),
                    )
                }
                OCI_INDEX | DOCKER_LIST => {
                    let index = manifest::parse_index(&body, &media_type)?;
                    let matching = index
                        .manifests
                        .into_iter()
                        .filter(|item| {
                            item.platform.as_ref().is_some_and(|value| {
                                Platform::canonical(
                                    &value.os,
                                    &value.architecture,
                                    value.variant.as_deref(),
                                )
                                .is_ok_and(|value| value == platform)
                            }) && matches!(item.media_type.as_str(), OCI_MANIFEST | DOCKER_MANIFEST)
                        })
                        .collect::<Vec<_>>();
                    if matching.len() != 1 {
                        return Err(RegistryError::PlatformNotFound);
                    }
                    let child = &matching[0];
                    let child_response = self
                        .fetch(
                            image,
                            &child.digest,
                            bearer.as_ref().map(SecretValue::expose),
                            None,
                        )
                        .await?;
                    classify_status(child_response.status(), credential.is_some())?;
                    let child_media = child_response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.split(';').next())
                        .ok_or(RegistryError::Protocol)?
                        .to_owned();
                    let child_declared = child_response
                        .headers()
                        .get("docker-content-digest")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let child_body = read_limited(child_response, MANIFEST_LIMIT).await?;
                    let child_digest = digest(&child_body);
                    if child_digest != child.digest
                        || child_declared.is_some_and(|value| value != child_digest)
                    {
                        return Err(RegistryError::DigestMismatch);
                    }
                    let manifest = manifest::parse_manifest(&child_body, &child_media)?;
                    let selected_platform = child
                        .platform
                        .as_ref()
                        .ok_or(RegistryError::PlatformNotFound)
                        .and_then(|value| {
                            Platform::canonical(
                                &value.os,
                                &value.architecture,
                                value.variant.as_deref(),
                            )
                        })?;
                    (
                        Some(source_digest.clone()),
                        child.digest.clone(),
                        manifest.config.digest,
                        selected_platform,
                    )
                }
                _ => return Err(RegistryError::ManifestUnsupported),
            };
        Ok(PollResolve::Modified {
            image: Box::new(ResolvedImage {
                source_image_ref: image.canonical_tagged_ref.clone(),
                logical_registry: image.logical_registry.clone(),
                repository: image.repository.clone(),
                source_tag: image.tag.clone(),
                source_descriptor_digest: source_digest,
                index_digest,
                runnable_image_ref: image.runnable(&manifest_digest)?,
                manifest_digest,
                platform: selected_platform,
                local_image_id,
            }),
            etag,
        })
    }

    async fn fetch(
        &self,
        image: &ImageReference,
        selector: &str,
        bearer: Option<&str>,
        etag: Option<&str>,
    ) -> Result<Response, RegistryError> {
        let scheme = if self.allow_http_loopback
            && (image.transport_registry.starts_with("127.0.0.1:")
                || image.transport_registry.starts_with("localhost:"))
        {
            "http"
        } else {
            "https"
        };
        let url = format!(
            "{scheme}://{}/v2/{}/manifests/{selector}",
            image.transport_registry, image.repository
        );
        let mut request = self
            .client
            .get(url)
            .header(header::ACCEPT, manifest::ACCEPT);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        request.send().await.map_err(classify_transport)
    }

    async fn fetch_blob_authenticated(
        &self,
        image: &ImageReference,
        digest: &str,
        credential: Option<(&str, &SecretValue)>,
    ) -> Result<Response, RegistryError> {
        let first = self.fetch_blob(image, digest, None).await?;
        if first.status() != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let challenge = first
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .ok_or(RegistryError::CredentialRequired)?;
        let challenge = parse_bearer(challenge, self.allow_http_loopback)?;
        validate_bearer_realm(image, &challenge.realm, self.allow_http_loopback)?;
        let mut url = challenge.realm;
        {
            let mut pairs = url.query_pairs_mut();
            if let Some(service) = challenge.service {
                pairs.append_pair("service", &service);
            }
            pairs.append_pair("scope", &format!("repository:{}:pull", image.repository));
        }
        let mut request = self.client.get(url);
        if let Some((username, secret)) = credential {
            request = request.basic_auth(username, Some(secret.expose()));
        }
        let token_response = request.send().await.map_err(classify_transport)?;
        classify_status(token_response.status(), credential.is_some())?;
        let token_body = Zeroizing::new(read_limited(token_response, TOKEN_LIMIT).await?);
        #[derive(Deserialize)]
        struct Token {
            token: Option<SecretValue>,
            access_token: Option<SecretValue>,
        }
        let token: Token =
            serde_json::from_slice(&token_body).map_err(|_| RegistryError::Protocol)?;
        let token = token
            .token
            .or(token.access_token)
            .filter(|value| !value.expose().is_empty() && value.expose().len() <= 16 * 1024)
            .ok_or(RegistryError::CredentialInvalid)?;
        self.fetch_blob(image, digest, Some(token.expose())).await
    }

    async fn fetch_blob(
        &self,
        image: &ImageReference,
        digest: &str,
        bearer: Option<&str>,
    ) -> Result<Response, RegistryError> {
        let scheme = if self.allow_http_loopback
            && (image.transport_registry.starts_with("127.0.0.1:")
                || image.transport_registry.starts_with("localhost:"))
        {
            "http"
        } else {
            "https"
        };
        let initial_url = Url::parse(&format!(
            "{scheme}://{}/v2/{}/blobs/{digest}",
            image.transport_registry, image.repository
        ))
        .map_err(|_| RegistryError::Protocol)?;
        let mut url = initial_url.clone();
        for redirects in 0..=BLOB_REDIRECT_LIMIT {
            let mut request = self.client.get(url.clone());
            if same_origin(&url, &initial_url)
                && let Some(token) = bearer
            {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(classify_transport)?;
            if !is_followable_redirect(response.status()) {
                return Ok(response);
            }
            if redirects == BLOB_REDIRECT_LIMIT {
                return Err(RegistryError::Protocol);
            }
            url = blob_redirect_target(&response, self.allow_http_loopback)?;
        }
        Err(RegistryError::Protocol)
    }
}

fn is_followable_redirect(status: StatusCode) -> bool {
    matches!(status, StatusCode::FOUND | StatusCode::TEMPORARY_REDIRECT)
}

fn blob_redirect_target(
    response: &Response,
    allow_http_loopback: bool,
) -> Result<Url, RegistryError> {
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 8 * 1024)
        .ok_or(RegistryError::Protocol)?;
    let target = response
        .url()
        .join(location)
        .map_err(|_| RegistryError::Protocol)?;
    let loopback_http = allow_http_loopback
        && target.scheme() == "http"
        && matches!(target.host_str(), Some("127.0.0.1" | "localhost"));
    if (target.scheme() != "https" && !loopback_http)
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
    {
        return Err(RegistryError::Protocol);
    }
    Ok(target)
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn parse_image_config(
    body: &[u8],
    resolved_digest: String,
) -> Result<ImageConfigSuggestion, RegistryError> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        config: Config,
    }
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Config {
        #[serde(default)]
        exposed_ports: std::collections::BTreeMap<String, serde_json::Value>,
        #[serde(default)]
        volumes: std::collections::BTreeMap<String, serde_json::Value>,
        healthcheck: Option<serde_json::Value>,
        user: Option<String>,
        stop_signal: Option<String>,
    }
    let root: Root = serde_json::from_slice(body).map_err(|_| RegistryError::Protocol)?;
    let mut exposed_ports = root
        .config
        .exposed_ports
        .keys()
        .map(|value| {
            let (port, protocol) = value.split_once('/').ok_or(RegistryError::Protocol)?;
            let container_port = port.parse::<u16>().map_err(|_| RegistryError::Protocol)?;
            if container_port == 0 || !matches!(protocol, "tcp" | "udp") {
                return Err(RegistryError::Protocol);
            }
            Ok(ExposedPort {
                container_port,
                protocol: protocol.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, RegistryError>>()?;
    exposed_ports.sort_by_key(|value| (value.container_port, value.protocol.clone()));
    if exposed_ports.len() > 64 || root.config.volumes.len() > 64 {
        return Err(RegistryError::Protocol);
    }
    let mut volume_targets = root.config.volumes.into_keys().collect::<Vec<_>>();
    if volume_targets
        .iter()
        .any(|target| !target.starts_with('/') || target.len() > 4096)
    {
        return Err(RegistryError::Protocol);
    }
    volume_targets.sort();
    Ok(ImageConfigSuggestion {
        resolved_digest,
        exposed_ports,
        volume_targets,
        has_healthcheck: root.config.healthcheck.is_some(),
        user: root
            .config
            .user
            .filter(|value| !value.is_empty() && value.len() <= 256),
        stop_signal: root
            .config
            .stop_signal
            .filter(|value| !value.is_empty() && value.len() <= 64),
        warnings: Vec::new(),
    })
}

fn safe_etag(value: &header::HeaderValue) -> Option<String> {
    let value = value.to_str().ok()?;
    if value.is_empty() || value.len() > 256 || value.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    Some(value.to_owned())
}

fn digest(body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(body))
}

async fn read_limited(response: Response, limit: usize) -> Result<Vec<u8>, RegistryError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(RegistryError::Protocol);
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(classify_transport)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(RegistryError::Protocol);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_status(status: StatusCode, authenticated: bool) -> Result<(), RegistryError> {
    match status {
        value if value.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(if authenticated {
            RegistryError::CredentialInvalid
        } else {
            RegistryError::CredentialRequired
        }),
        StatusCode::FORBIDDEN => Err(RegistryError::Forbidden),
        StatusCode::NOT_FOUND => Err(RegistryError::TagNotFound),
        StatusCode::TOO_MANY_REQUESTS => Err(RegistryError::RateLimited),
        value if value.is_server_error() => Err(RegistryError::Unavailable),
        _ => Err(RegistryError::Protocol),
    }
}

fn classify_transport(error: reqwest::Error) -> RegistryError {
    if error.is_timeout() {
        RegistryError::Timeout
    } else if error
        .to_string()
        .to_ascii_lowercase()
        .contains("certificate")
        || error.to_string().to_ascii_lowercase().contains("tls")
    {
        RegistryError::Tls
    } else if error.is_connect() {
        RegistryError::Unavailable
    } else {
        RegistryError::Protocol
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    #[tokio::test]
    async fn private_index_reuses_bearer_for_platform_manifest_and_verifies_digests() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": format!("sha256:{}", "c".repeat(64)) },
            "layers": []
        }).to_string();
        let manifest_digest = digest(manifest.as_bytes());
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [{ "mediaType": OCI_MANIFEST, "digest": manifest_digest, "platform": {"os":"linux","architecture":"amd64"}}]
        }).to_string();
        let index_digest = digest(index.as_bytes());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = seen.clone();
        let server_manifest_digest = manifest_digest.clone();
        let server = tokio::spawn(async move {
            for sequence in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let read = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..read]).to_string();
                server_seen.lock().await.push(request.clone());
                let response = match sequence {
                    0 => format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{address}/token\",service=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    1 => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"token\":\"fixture-token\"}".into(),
                    2 => format!("HTTP/1.1 200 OK\r\nContent-Type: {OCI_INDEX}\r\nDocker-Content-Digest: {index_digest}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{index}", index.len()),
                    _ => format!("HTTP/1.1 200 OK\r\nContent-Type: {OCI_MANIFEST}\r\nDocker-Content-Digest: {server_manifest_digest}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{manifest}", manifest.len()),
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let image = ImageReference::parse(&format!("{address}/team/app:latest")).unwrap();
        let credential = SecretValue::new("canary-password".into());
        let resolved = RegistryResolver::for_test_http()
            .unwrap()
            .resolve(
                &image,
                &Platform {
                    os: "linux".into(),
                    architecture: "amd64".into(),
                    variant: None,
                },
                Some(("fixture-user", &credential)),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(resolved.manifest_digest, manifest_digest);
        assert!(resolved.runnable_image_ref.ends_with(&manifest_digest));
        let requests = seen.lock().await;
        assert!(
            requests[1].contains("authorization: Basic Zml4dHVyZS11c2VyOmNhbmFyeS1wYXNzd29yZA==")
        );
        assert!(requests[2].contains("authorization: Bearer fixture-token"));
        assert!(requests[3].contains("authorization: Bearer fixture-token"));
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("canary-password"))
        );
    }

    #[test]
    fn canonical_platform_normalizes_common_docker_architectures() {
        assert_eq!(
            Platform::canonical("linux", "x86_64", None).unwrap(),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            }
        );
        assert_eq!(
            Platform::canonical("linux", "arm64", None).unwrap(),
            Platform::canonical("linux", "aarch64", Some("v8")).unwrap()
        );
        assert_eq!(
            Platform::canonical("linux", "armv7l", None).unwrap(),
            Platform {
                os: "linux".into(),
                architecture: "arm".into(),
                variant: Some("v7".into()),
            }
        );
    }

    #[tokio::test]
    async fn poll_uses_safe_etag_and_accepts_not_modified_only_as_observation() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": format!("sha256:{}", "c".repeat(64)) },
            "layers": []
        })
        .to_string();
        let manifest_digest = digest(manifest.as_bytes());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_manifest = manifest.clone();
        let server_digest = manifest_digest.clone();
        let server = tokio::spawn(async move {
            for sequence in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 4096];
                let read = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..read]);
                let response = if sequence == 0 {
                    assert!(!request.to_ascii_lowercase().contains("if-none-match"));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {OCI_MANIFEST}\r\nDocker-Content-Digest: {server_digest}\r\nETag: \"fixture-v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{server_manifest}",
                        server_manifest.len()
                    )
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("if-none-match: \"fixture-v1\"")
                    );
                    "HTTP/1.1 304 Not Modified\r\nETag: \"fixture-v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let resolver = RegistryResolver::for_test_http().unwrap();
        let image = ImageReference::parse(&format!("{address}/team/app:latest")).unwrap();
        let platform = Platform::canonical("linux", "amd64", None).unwrap();
        let first = resolver
            .resolve_poll(&image, &platform, None, None)
            .await
            .unwrap();
        let PollResolve::Modified { image, etag } = first else {
            panic!("initial poll must resolve a complete manifest")
        };
        assert_eq!(image.manifest_digest, manifest_digest);
        assert_eq!(etag.as_deref(), Some("\"fixture-v1\""));
        assert!(matches!(
            resolver
                .resolve_poll(
                    &ImageReference::parse(&format!("{address}/team/app:latest")).unwrap(),
                    &platform,
                    None,
                    etag.as_deref()
                )
                .await
                .unwrap(),
            PollResolve::NotModified
        ));
        server.await.unwrap();
    }

    #[test]
    fn unsafe_etag_is_not_reused() {
        assert!(safe_etag(&header::HeaderValue::from_static("\"safe\"")).is_some());
        assert!(safe_etag(&header::HeaderValue::from_static("")).is_none());
        let oversized = header::HeaderValue::from_str(&format!("\"{}\"", "a".repeat(255))).unwrap();
        assert!(safe_etag(&oversized).is_none());
    }

    #[test]
    fn image_config_projection_is_allowlisted_and_canonical() {
        let body = serde_json::json!({
            "config": {
                "ExposedPorts": {"3000/tcp": {}, "5353/udp": {}},
                "Volumes": {"/var/lib/data": {}, "/cache": {}},
                "Healthcheck": {"Test": ["CMD", "curl", "http://localhost/healthz"]},
                "User": "10001:10001",
                "StopSignal": "SIGTERM",
                "Env": ["SECRET_CANARY=must-not-project"],
                "Labels": {"secret": "must-not-project"},
                "Entrypoint": ["/bin/private"]
            }
        });
        let suggestion = parse_image_config(
            serde_json::to_vec(&body).unwrap().as_slice(),
            format!("sha256:{}", "a".repeat(64)),
        )
        .unwrap();
        assert_eq!(suggestion.exposed_ports.len(), 2);
        assert_eq!(suggestion.volume_targets, vec!["/cache", "/var/lib/data"]);
        assert!(suggestion.has_healthcheck);
        let serialized = serde_json::to_string(&suggestion).unwrap();
        assert!(!serialized.contains("SECRET_CANARY"));
        assert!(!serialized.contains("must-not-project"));
        assert!(!serialized.contains("/bin/private"));
    }

    #[tokio::test]
    async fn image_config_follows_blob_redirect_without_forwarding_bearer() {
        let config = serde_json::json!({
            "config": {
                "ExposedPorts": {"3000/tcp": {}},
                "Volumes": {"/var/lib/data": {}},
                "User": "10001:10001",
                "StopSignal": "SIGTERM"
            }
        })
        .to_string();
        let config_digest = digest(config.as_bytes());
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest },
            "layers": []
        })
        .to_string();
        let manifest_digest = digest(manifest.as_bytes());
        let registry = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_address = registry.local_addr().unwrap();
        let storage = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let storage_address = storage.local_addr().unwrap();
        let registry_seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = registry_seen.clone();
        let server_manifest = manifest.clone();
        let server_manifest_digest = manifest_digest.clone();
        let registry_server = tokio::spawn(async move {
            for sequence in 0..6 {
                let (mut stream, _) = registry.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let read = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..read]).to_string();
                server_seen.lock().await.push(request);
                let response = match sequence {
                    0 | 3 => format!(
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{registry_address}/token\",service=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    1 | 4 => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"token\":\"fixture-token\"}".into(),
                    2 => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {OCI_MANIFEST}\r\nDocker-Content-Digest: {server_manifest_digest}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{server_manifest}",
                        server_manifest.len()
                    ),
                    _ => format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{storage_address}/signed/config?key=fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let storage_seen = Arc::new(Mutex::new(String::new()));
        let server_storage_seen = storage_seen.clone();
        let storage_server = tokio::spawn(async move {
            let (mut stream, _) = storage.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let read = stream.read(&mut bytes).await.unwrap();
            *server_storage_seen.lock().await = String::from_utf8_lossy(&bytes[..read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{config}",
                config.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let image = ImageReference::parse(&format!("{registry_address}/team/app:latest")).unwrap();
        let credential = SecretValue::new("canary-password".into());
        let suggestion = RegistryResolver::for_test_http()
            .unwrap()
            .inspect_config(
                &image,
                &Platform::canonical("linux", "amd64", None).unwrap(),
                Some(("fixture-user", &credential)),
            )
            .await
            .unwrap();
        registry_server.await.unwrap();
        storage_server.await.unwrap();

        assert_eq!(suggestion.resolved_digest, manifest_digest);
        assert_eq!(suggestion.exposed_ports[0].container_port, 3000);
        assert_eq!(suggestion.volume_targets, vec!["/var/lib/data"]);
        let registry_requests = registry_seen.lock().await;
        assert!(registry_requests[5].contains("authorization: Bearer fixture-token"));
        let storage_request = storage_seen.lock().await;
        assert!(storage_request.starts_with("GET /signed/config?key=fixture "));
        assert!(
            !storage_request
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        assert!(!storage_request.contains("fixture-token"));
        assert!(!storage_request.contains("canary-password"));
    }

    #[tokio::test]
    async fn blob_redirect_rejects_cleartext_non_loopback_target() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://registry.example/blob\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let image = ImageReference::parse(&format!("{address}/team/app:latest")).unwrap();
        let error = RegistryResolver::for_test_http()
            .unwrap()
            .fetch_blob(&image, &format!("sha256:{}", "a".repeat(64)), None)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(error, RegistryError::Protocol));
    }

    #[tokio::test]
    async fn basic_only_registry_auth_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let image = ImageReference::parse(&format!("{address}/team/app:latest")).unwrap();
        let credential = SecretValue::new("not-used".into());
        let error = RegistryResolver::for_test_http()
            .unwrap()
            .resolve(
                &image,
                &Platform::canonical("linux", "amd64", None).unwrap(),
                Some(("fixture", &credential)),
            )
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(error, RegistryError::AuthUnsupported));
    }

    #[tokio::test]
    async fn manifest_auth_rejects_cross_origin_realm_before_any_token_request() {
        let registry = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_address = registry.local_addr().unwrap();
        let attacker = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let attacker_address = attacker.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = registry.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{attacker_address}/token\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let image = ImageReference::parse(&format!("{registry_address}/team/app:latest")).unwrap();
        let credential = SecretValue::new("must-not-leave-process".into());
        let error = RegistryResolver::for_test_http()
            .unwrap()
            .resolve(
                &image,
                &Platform::canonical("linux", "amd64", None).unwrap(),
                Some(("fixture-user", &credential)),
            )
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(error, RegistryError::AuthRealmUntrusted));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), attacker.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn blob_auth_rejects_cross_origin_realm_before_any_token_request() {
        let registry = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_address = registry.local_addr().unwrap();
        let attacker = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let attacker_address = attacker.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = registry.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{attacker_address}/token\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let image = ImageReference::parse(&format!("{registry_address}/team/app:latest")).unwrap();
        let credential = SecretValue::new("must-not-leave-process".into());
        let error = RegistryResolver::for_test_http()
            .unwrap()
            .fetch_blob_authenticated(
                &image,
                &format!("sha256:{}", "a".repeat(64)),
                Some(("fixture-user", &credential)),
            )
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(error, RegistryError::AuthRealmUntrusted));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), attacker.accept())
                .await
                .is_err()
        );
    }
}
