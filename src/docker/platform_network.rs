use std::{collections::HashMap, sync::LazyLock};

use tokio::sync::Mutex;

use super::models::{DockerNetworkResource, DockerReadApi};
use crate::domain::{PLATFORM_BRIDGE_NAME, PLATFORM_NETWORK_NAME};

const MANAGED: &str = "io.solodock.managed";
const SCOPE: &str = "io.solodock.resource-scope";
const SCHEMA: &str = "io.solodock.network-schema";
static GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub fn expected_labels() -> HashMap<String, String> {
    HashMap::from([
        (MANAGED.to_owned(), "true".to_owned()),
        (SCOPE.to_owned(), "platform".to_owned()),
        (SCHEMA.to_owned(), "1".to_owned()),
    ])
}

pub async fn ensure(api: &dyn DockerReadApi) -> Result<(), PlatformNetworkError> {
    let _guard = GUARD.lock().await;
    if let Some(existing) = api
        .inspect_network(PLATFORM_NETWORK_NAME)
        .await
        .map_err(|_| PlatformNetworkError::Unavailable)?
    {
        return validate_existing(&existing);
    }
    if api.create_platform_network().await.is_err() {
        // A concurrent creator can make create return a conflict. Inspect the
        // resulting resource before deciding whether the operation failed.
        let Some(existing) = api
            .inspect_network(PLATFORM_NETWORK_NAME)
            .await
            .map_err(|_| PlatformNetworkError::Unavailable)?
        else {
            return Err(PlatformNetworkError::Unavailable);
        };
        return validate_existing(&existing);
    }
    let existing = api
        .inspect_network(PLATFORM_NETWORK_NAME)
        .await
        .map_err(|_| PlatformNetworkError::Unavailable)?
        .ok_or(PlatformNetworkError::Unavailable)?;
    validate_existing(&existing)
}

pub async fn ensure_for_app(
    api: &dyn DockerReadApi,
    alias: &str,
    allowed_container_ids: &[&str],
) -> Result<(), PlatformNetworkError> {
    ensure(api).await?;
    validate_alias(api, alias, allowed_container_ids, false).await
}

pub async fn validate_for_app_if_present(
    api: &dyn DockerReadApi,
    alias: &str,
    allowed_container_ids: &[&str],
) -> Result<(), PlatformNetworkError> {
    validate_alias(api, alias, allowed_container_ids, true).await
}

pub async fn validate_if_present(api: &dyn DockerReadApi) -> Result<(), PlatformNetworkError> {
    let resource = api
        .inspect_network(PLATFORM_NETWORK_NAME)
        .await
        .map_err(|_| PlatformNetworkError::Unavailable)?;
    resource.as_ref().map_or(Ok(()), validate_existing)
}

async fn validate_alias(
    api: &dyn DockerReadApi,
    alias: &str,
    allowed_container_ids: &[&str],
    allow_missing: bool,
) -> Result<(), PlatformNetworkError> {
    let resource = api
        .inspect_network(PLATFORM_NETWORK_NAME)
        .await
        .map_err(|_| PlatformNetworkError::Unavailable)?;
    let Some(resource) = resource else {
        return if allow_missing {
            Ok(())
        } else {
            Err(PlatformNetworkError::Unavailable)
        };
    };
    validate_existing(&resource)?;
    let snapshot = api
        .inspect_network_snapshot(PLATFORM_NETWORK_NAME)
        .await
        .map_err(|_| PlatformNetworkError::Unavailable)?
        .ok_or(PlatformNetworkError::Unavailable)?;
    if snapshot.name != PLATFORM_NETWORK_NAME {
        return Err(PlatformNetworkError::Unavailable);
    }
    if snapshot.members.iter().any(|member| {
        !allowed_container_ids.contains(&member.container_id.as_str())
            && member.dns_names.iter().any(|name| name == alias)
    }) {
        return Err(PlatformNetworkError::AliasConflict);
    }
    Ok(())
}

pub fn validate_existing(resource: &DockerNetworkResource) -> Result<(), PlatformNetworkError> {
    let expected = expected_labels();
    let bridge = resource
        .options
        .get("com.docker.network.bridge.name")
        .map(String::as_str);
    if resource.name != PLATFORM_NETWORK_NAME
        || resource.driver.as_deref() != Some("bridge")
        || !resource.internal
        || bridge != Some(PLATFORM_BRIDGE_NAME)
        || expected
            .iter()
            .any(|(key, value)| resource.labels.get(key) != Some(value))
    {
        return Err(PlatformNetworkError::IdentityConflict);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformNetworkError {
    #[error("platform network is unavailable")]
    Unavailable,
    #[error("platform network identity conflicts with an existing resource")]
    IdentityConflict,
    #[error("platform network alias is already owned by another container")]
    AliasConflict,
}

impl PlatformNetworkError {
    pub const fn public_code(&self) -> &'static str {
        match self {
            Self::Unavailable => "PLATFORM_NETWORK_UNAVAILABLE",
            Self::IdentityConflict => "PLATFORM_NETWORK_IDENTITY_CONFLICT",
            Self::AliasConflict => "NETWORK_ALIAS_CONFLICT",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::models::{
        ContainerRecord, DockerError, DockerStream, LogChunk, LogRequest, NetworkMember,
        NetworkSnapshot, ProbeSnapshot, RawDockerEvent, RawStats,
    };

    fn valid() -> DockerNetworkResource {
        DockerNetworkResource {
            name: PLATFORM_NETWORK_NAME.into(),
            labels: expected_labels(),
            driver: Some("bridge".into()),
            internal: true,
            options: HashMap::from([(
                "com.docker.network.bridge.name".into(),
                PLATFORM_BRIDGE_NAME.into(),
            )]),
        }
    }

    #[test]
    fn platform_identity_is_exact_and_never_accepts_an_unmanaged_namesake() {
        assert!(validate_existing(&valid()).is_ok());
        for mutate in [
            |value: &mut DockerNetworkResource| value.internal = false,
            |value: &mut DockerNetworkResource| value.driver = Some("overlay".into()),
            |value: &mut DockerNetworkResource| {
                value.labels.remove("io.solodock.resource-scope");
            },
            |value: &mut DockerNetworkResource| {
                value.options.insert(
                    "com.docker.network.bridge.name".into(),
                    "not-solodock".into(),
                );
            },
        ] {
            let mut resource = valid();
            mutate(&mut resource);
            assert!(matches!(
                validate_existing(&resource),
                Err(PlatformNetworkError::IdentityConflict)
            ));
        }
    }

    struct AliasDocker {
        snapshot: NetworkSnapshot,
    }

    #[async_trait::async_trait]
    impl DockerReadApi for AliasDocker {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            unreachable!()
        }
        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            unreachable!()
        }
        async fn inspect_container(&self, _: &str) -> Result<ContainerRecord, DockerError> {
            unreachable!()
        }
        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            unreachable!()
        }
        async fn logs(
            &self,
            _: &str,
            _: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            unreachable!()
        }
        async fn stats(&self, _: &str) -> Result<DockerStream<RawStats>, DockerError> {
            unreachable!()
        }
        async fn inspect_network(
            &self,
            _: &str,
        ) -> Result<Option<DockerNetworkResource>, DockerError> {
            Ok(Some(valid()))
        }
        async fn inspect_network_snapshot(
            &self,
            _: &str,
        ) -> Result<Option<NetworkSnapshot>, DockerError> {
            Ok(Some(self.snapshot.clone()))
        }
    }

    #[tokio::test]
    async fn service_alias_is_unique_except_for_the_exact_replaced_container() {
        let occupied = "a".repeat(64);
        let api = AliasDocker {
            snapshot: NetworkSnapshot {
                name: PLATFORM_NETWORK_NAME.into(),
                labels: expected_labels(),
                members: vec![NetworkMember {
                    container_id: occupied.clone(),
                    dns_names: vec!["postgres".into()],
                }],
            },
        };
        assert!(matches!(
            validate_for_app_if_present(&api, "postgres", &[]).await,
            Err(PlatformNetworkError::AliasConflict)
        ));
        assert!(
            ensure_for_app(&api, "postgres", &[occupied.as_str()])
                .await
                .is_ok()
        );
    }
}
