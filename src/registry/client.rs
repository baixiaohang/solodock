use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{
    auth::parse_bearer,
    error::RegistryError,
    manifest::{self, DOCKER_LIST, DOCKER_MANIFEST, OCI_INDEX, OCI_MANIFEST},
    reference::ImageReference,
};
use crate::security::secret::SecretValue;

const MANIFEST_LIMIT: usize = 8 * 1024 * 1024;
const TOKEN_LIMIT: usize = 64 * 1024;

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
}
