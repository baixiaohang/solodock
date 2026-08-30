use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::DomainError;

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
    External,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct NetworkPlan {
    pub owned_default_network: bool,
    pub mode: NetworkMode,
    pub external: Vec<ExternalNetworkAttachment>,
}

impl NetworkPlan {
    pub fn expected_networks(&self, app_id: Uuid) -> Vec<ExpectedNetwork> {
        let mut expected =
            Vec::with_capacity(self.external.len() + usize::from(self.owned_default_network));
        if self.owned_default_network {
            expected.push(ExpectedNetwork {
                name: format!("solodock-{}-default", app_id.simple()),
                kind: ExpectedNetworkKind::OwnedDefault,
                aliases: Vec::new(),
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
    networks: &mut [NetworkInput],
) -> Result<NetworkPlan, DomainError> {
    for network in networks.iter_mut() {
        if let NetworkInput::External { aliases, .. } = network {
            let mut seen = HashSet::new();
            for alias in aliases.iter() {
                validate_alias(alias)?;
                if !seen.insert(alias.clone()) {
                    return Err(DomainError::ConfigInvalid);
                }
            }
            if aliases.len() > MAX_NETWORK_ALIASES {
                return Err(DomainError::ConfigQuotaExceeded);
            }
            aliases.sort();
        }
    }
    networks.sort_by(|left, right| network_sort_key(left).cmp(&network_sort_key(right)));
    network_plan(owned_default_network, networks)
}

pub fn network_plan(
    owned_default_network: bool,
    networks: &[NetworkInput],
) -> Result<NetworkPlan, DomainError> {
    let mut names = HashSet::new();
    let mut owned_markers = 0usize;
    let mut external = Vec::new();
    for (source_index, network) in networks.iter().enumerate() {
        match network {
            NetworkInput::OwnedDefault => owned_markers += 1,
            NetworkInput::External { name, aliases } => {
                validate_docker_name(name)?;
                if !names.insert(name.clone()) {
                    return Err(DomainError::ConfigInvalid);
                }
                if aliases.len() > MAX_NETWORK_ALIASES {
                    return Err(DomainError::ConfigQuotaExceeded);
                }
                let mut previous: Option<&str> = None;
                for alias in aliases {
                    validate_alias(alias)?;
                    if previous == Some(alias) {
                        return Err(DomainError::ConfigInvalid);
                    }
                    previous = Some(alias);
                }
                external.push(ExternalNetworkAttachment {
                    name: name.clone(),
                    aliases: aliases.clone(),
                    source_index,
                });
            }
        }
    }
    if external.len() > MAX_EXTERNAL_NETWORKS {
        return Err(DomainError::ConfigQuotaExceeded);
    }
    if owned_markers > 1 || (!owned_default_network && owned_markers != 0) {
        return Err(DomainError::ConfigInvalid);
    }
    if !owned_default_network && external.is_empty() {
        return Err(DomainError::ConfigInvalid);
    }
    external.sort_by(|left, right| left.name.cmp(&right.name));
    let mode = match (owned_default_network, external.is_empty()) {
        (true, true) => NetworkMode::OwnedOnly,
        (true, false) => NetworkMode::OwnedAndExternal,
        (false, false) => NetworkMode::ExternalOnly,
        (false, true) => return Err(DomainError::ConfigInvalid),
    };
    Ok(NetworkPlan {
        owned_default_network,
        mode,
        external,
    })
}

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
        let plan = normalize_networks(true, &mut networks).unwrap();
        assert_eq!(plan.mode, NetworkMode::OwnedAndExternal);
        assert_eq!(plan.external[0].name, "alpha");
        assert_eq!(plan.external[0].source_index, 1);
        assert_eq!(plan.external[1].aliases, ["z-one", "z-two"]);
        assert_eq!(plan.external[1].source_index, 2);
    }

    #[test]
    fn rejects_invalid_external_only_and_aliases() {
        assert_eq!(
            normalize_networks(false, &mut []).unwrap_err(),
            DomainError::ConfigInvalid
        );
        for alias in ["UPPER", "-bad", "bad-", "bad.name", ""] {
            let mut networks = vec![NetworkInput::External {
                name: "shared".into(),
                aliases: vec![alias.into()],
            }];
            assert_eq!(
                normalize_networks(false, &mut networks).unwrap_err(),
                DomainError::ConfigInvalid
            );
        }
    }
}
