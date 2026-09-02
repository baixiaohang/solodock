use std::collections::HashSet;

use super::{AppResourceNames, DomainError};
use serde::{Deserialize, Serialize};

pub const MAX_EXTERNAL_NETWORKS: usize = 8;
pub const MAX_NETWORK_ALIASES: usize = 8;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkInput {
    /// 只用于读取旧 revision；新写入由 `owned_default_network` 表达。
    OwnedDefault,
    External {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        aliases: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    OwnedOnly,
    OwnedAndExternal,
    ExternalOnly,
    OwnedAndPlatform,
    OwnedPlatformAndExternal,
    PlatformAndExternal,
    PlatformOnly,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ExternalNetworkAttachment {
    pub name: String,
    pub aliases: Vec<String>,
    #[serde(skip)]
    pub source_index: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ExpectedNetwork {
    pub name: String,
    pub kind: ExpectedNetworkKind,
    pub aliases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedNetworkKind {
    OwnedDefault,
    Platform,
    External,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct NetworkPlan {
    pub owned_default_network: bool,
    pub service_discovery_enabled: bool,
    pub mode: NetworkMode,
    pub external: Vec<ExternalNetworkAttachment>,
}

#[derive(Debug)]
pub struct NetworkValidationError {
    pub error: DomainError,
    pub path: String,
    pub code: &'static str,
    pub message: String,
}

impl NetworkPlan {
    pub fn expected_networks(
        &self,
        names: &AppResourceNames,
        service_alias: &str,
    ) -> Vec<ExpectedNetwork> {
        let mut expected = Vec::with_capacity(
            self.external.len()
                + usize::from(self.owned_default_network)
                + usize::from(self.service_discovery_enabled),
        );
        if self.owned_default_network {
            expected.push(ExpectedNetwork {
                name: names.owned_default_network_name.clone(),
                kind: ExpectedNetworkKind::OwnedDefault,
                aliases: Vec::new(),
            });
        }
        if self.service_discovery_enabled {
            expected.push(ExpectedNetwork {
                name: PLATFORM_NETWORK_NAME.to_owned(),
                kind: ExpectedNetworkKind::Platform,
                aliases: vec![service_alias.to_owned()],
            });
        }
        expected.extend(self.external.iter().map(|attachment| ExpectedNetwork {
            name: attachment.name.clone(),
            kind: ExpectedNetworkKind::External,
            aliases: attachment.aliases.clone(),
        }));
        expected.sort_by(|left, right| left.name.cmp(&right.name));
        expected
    }
}

pub fn normalize_networks(
    owned_default_network: bool,
    service_discovery_enabled: bool,
    networks: &mut [NetworkInput],
) -> Result<NetworkPlan, DomainError> {
    normalize_networks_with_issues(owned_default_network, service_discovery_enabled, networks)
        .map_err(|error| error.error)
}

pub fn normalize_networks_with_issues(
    owned_default_network: bool,
    service_discovery_enabled: bool,
    networks: &mut [NetworkInput],
) -> Result<NetworkPlan, NetworkValidationError> {
    validate_network_inputs(
        owned_default_network,
        service_discovery_enabled,
        networks,
        true,
    )?;
    for network in networks.iter_mut() {
        if let NetworkInput::External { aliases, .. } = network {
            aliases.sort();
        }
    }
    networks.sort_by(|left, right| network_sort_key(left).cmp(&network_sort_key(right)));
    Ok(build_network_plan(
        owned_default_network,
        service_discovery_enabled,
        networks,
    ))
}

pub fn network_plan(
    owned_default_network: bool,
    service_discovery_enabled: bool,
    networks: &[NetworkInput],
) -> Result<NetworkPlan, DomainError> {
    validate_network_inputs(
        owned_default_network,
        service_discovery_enabled,
        networks,
        false,
    )
    .map_err(|error| error.error)?;
    Ok(build_network_plan(
        owned_default_network,
        service_discovery_enabled,
        networks,
    ))
}

fn validate_network_inputs(
    owned_default_network: bool,
    service_discovery_enabled: bool,
    networks: &[NetworkInput],
    reserve_platform_name: bool,
) -> Result<(), NetworkValidationError> {
    let mut names = HashSet::new();
    let mut owned_markers = 0usize;
    for (source_index, network) in networks.iter().enumerate() {
        match network {
            NetworkInput::OwnedDefault => owned_markers += 1,
            NetworkInput::External { name, aliases } => {
                if reserve_platform_name && name == PLATFORM_NETWORK_NAME {
                    return Err(network_error(
                        DomainError::ConfigInvalid,
                        format!("networks[{source_index}].name"),
                        "NETWORK_NAME_RESERVED",
                        "The platform service-discovery network name is reserved",
                    ));
                }
                validate_docker_name(name).map_err(|error| {
                    network_error(
                        error,
                        format!("networks[{source_index}].name"),
                        "INVALID_VALUE",
                        "Must be a valid Docker network name",
                    )
                })?;
                if !names.insert(name.clone()) {
                    return Err(network_error(
                        DomainError::ConfigInvalid,
                        format!("networks[{source_index}].name"),
                        "NETWORK_DUPLICATE",
                        "External network names must be unique",
                    ));
                }
                if aliases.len() > MAX_NETWORK_ALIASES {
                    return Err(network_error(
                        DomainError::ConfigQuotaExceeded,
                        format!("networks[{source_index}].aliases"),
                        "CONFIG_QUOTA_EXCEEDED",
                        format!("At most {MAX_NETWORK_ALIASES} aliases are allowed"),
                    ));
                }
                let mut seen_aliases = HashSet::new();
                for (alias_index, alias) in aliases.iter().enumerate() {
                    validate_alias(alias).map_err(|error| {
                        network_error(
                            error,
                            format!("networks[{source_index}].aliases[{alias_index}]"),
                            "INVALID_VALUE",
                            "Must be a valid lowercase DNS alias",
                        )
                    })?;
                    if !seen_aliases.insert(alias) {
                        return Err(network_error(
                            DomainError::ConfigInvalid,
                            format!("networks[{source_index}].aliases[{alias_index}]"),
                            "NETWORK_ALIAS_DUPLICATE",
                            "Aliases must be unique within a network",
                        ));
                    }
                }
            }
        }
    }
    let external_count = networks.len().saturating_sub(owned_markers);
    if external_count > MAX_EXTERNAL_NETWORKS {
        return Err(network_error(
            DomainError::ConfigQuotaExceeded,
            "networks",
            "CONFIG_QUOTA_EXCEEDED",
            format!("At most {MAX_EXTERNAL_NETWORKS} external networks are allowed"),
        ));
    }
    if owned_markers > 1 || (!owned_default_network && owned_markers != 0) {
        return Err(network_error(
            DomainError::ConfigInvalid,
            "networks",
            "NETWORK_INVALID",
            "The legacy owned-network marker is inconsistent",
        ));
    }
    if !owned_default_network && !service_discovery_enabled && external_count == 0 {
        return Err(network_error(
            DomainError::ConfigInvalid,
            "networks",
            "NETWORK_REQUIRED",
            "At least one network must be enabled",
        ));
    }
    Ok(())
}

fn build_network_plan(
    owned_default_network: bool,
    service_discovery_enabled: bool,
    networks: &[NetworkInput],
) -> NetworkPlan {
    let mut external = networks
        .iter()
        .enumerate()
        .filter_map(|(source_index, network)| match network {
            NetworkInput::OwnedDefault => None,
            NetworkInput::External { name, aliases } => Some(ExternalNetworkAttachment {
                name: name.clone(),
                aliases: aliases.clone(),
                source_index,
            }),
        })
        .collect::<Vec<_>>();
    external.sort_by(|left, right| left.name.cmp(&right.name));
    let mode = match (
        owned_default_network,
        service_discovery_enabled,
        external.is_empty(),
    ) {
        (true, false, true) => NetworkMode::OwnedOnly,
        (true, false, false) => NetworkMode::OwnedAndExternal,
        (false, false, false) => NetworkMode::ExternalOnly,
        (true, true, true) => NetworkMode::OwnedAndPlatform,
        (true, true, false) => NetworkMode::OwnedPlatformAndExternal,
        (false, true, false) => NetworkMode::PlatformAndExternal,
        (false, true, true) => NetworkMode::PlatformOnly,
        (false, false, true) => unreachable!("network inputs were validated"),
    };
    NetworkPlan {
        owned_default_network,
        service_discovery_enabled,
        mode,
        external,
    }
}

fn network_error(
    error: DomainError,
    path: impl Into<String>,
    code: &'static str,
    message: impl Into<String>,
) -> NetworkValidationError {
    NetworkValidationError {
        error,
        path: path.into(),
        code,
        message: message.into(),
    }
}

pub const PLATFORM_NETWORK_NAME: &str = "solodock-services";
pub const PLATFORM_BRIDGE_NAME: &str = "sd-services";

fn network_sort_key(value: &NetworkInput) -> (u8, &str) {
    match value {
        NetworkInput::OwnedDefault => (0, ""),
        NetworkInput::External { name, .. } => (1, name),
    }
}

fn validate_docker_name(value: &str) -> Result<(), DomainError> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(char::is_control)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

fn validate_alias(value: &str) -> Result<(), DomainError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 63
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(DomainError::ConfigInvalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_aliases_and_preserves_normalized_source_index() {
        let mut networks = vec![
            NetworkInput::External {
                name: "zeta".into(),
                aliases: vec!["z-two".into(), "z-one".into()],
            },
            NetworkInput::OwnedDefault,
            NetworkInput::External {
                name: "alpha".into(),
                aliases: vec![],
            },
        ];
        let plan = normalize_networks(true, false, &mut networks).unwrap();
        assert_eq!(plan.mode, NetworkMode::OwnedAndExternal);
        assert_eq!(plan.external[0].name, "alpha");
        assert_eq!(plan.external[0].source_index, 1);
        assert_eq!(plan.external[1].aliases, ["z-one", "z-two"]);
        assert_eq!(plan.external[1].source_index, 2);
    }

    #[test]
    fn rejects_invalid_external_only_and_aliases() {
        assert_eq!(
            normalize_networks(false, false, &mut []).unwrap_err(),
            DomainError::ConfigInvalid
        );
        for alias in ["UPPER", "-bad", "bad-", "bad.name", ""] {
            let mut networks = vec![NetworkInput::External {
                name: "shared".into(),
                aliases: vec![alias.into()],
            }];
            assert_eq!(
                normalize_networks(false, false, &mut networks).unwrap_err(),
                DomainError::ConfigInvalid
            );
        }
    }

    #[test]
    fn current_writes_reserve_the_platform_network_name_but_legacy_plans_remain_readable() {
        for service_discovery_enabled in [false, true] {
            let mut networks = vec![NetworkInput::External {
                name: PLATFORM_NETWORK_NAME.into(),
                aliases: vec!["legacy-alias".into()],
            }];
            assert_eq!(
                normalize_networks(true, service_discovery_enabled, &mut networks).unwrap_err(),
                DomainError::ConfigInvalid
            );
        }
        let legacy = network_plan(
            true,
            false,
            &[NetworkInput::External {
                name: PLATFORM_NETWORK_NAME.into(),
                aliases: vec!["legacy-alias".into()],
            }],
        )
        .unwrap();
        assert_eq!(legacy.external[0].name, PLATFORM_NETWORK_NAME);
    }
}
