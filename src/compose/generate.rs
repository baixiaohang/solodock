use std::{collections::BTreeMap, path::Path};

use serde::Serialize;
use uuid::Uuid;

use super::model::*;
use crate::{
    docker::ownership::{APP_ID_LABEL, MANAGED_LABEL, RELEASE_ID_LABEL, SCHEMA_LABEL},
    domain::{
        ExternalNetworkAttachment, HealthPolicy, HttpClient, NetworkMode, NormalizedDraft,
        PortProtocol, VolumeInput, app_resource_names, network_plan, owned_volume_name,
    },
};

pub struct ComposeInput<'a> {
    pub app_id: Uuid,
    pub slug: &'a str,
    pub release_id: Uuid,
    pub image_ref: &'a str,
    pub revision_directory: &'a Path,
    pub draft: &'a NormalizedDraft,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ComposePlan {
    pub service: &'static str,
    pub image_ref: String,
    pub runnable: bool,
    pub ports: usize,
    pub mounts: usize,
    pub networks: usize,
    pub network_mode: NetworkMode,
    pub owned_default_network: Option<OwnedDefaultNetworkIdentity>,
    pub external_networks: Vec<ExternalNetworkAttachment>,
    pub warnings: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OwnedDefaultNetworkIdentity {
    pub docker_name: String,
    pub bridge_name: String,
}

pub fn generate(
    input: ComposeInput<'_>,
    runnable: bool,
) -> Result<(String, ComposePlan), serde_yml::Error> {
    let resource_names = app_resource_names(input.slug);
    let ownership = service_labels(input.app_id, input.release_id);
    let resource_ownership = resource_labels(input.app_id, &resource_names.project_name);
    let mut volumes = BTreeMap::new();
    let mut mounts = Vec::new();
    for (index, volume) in input.draft.volumes.iter().enumerate() {
        let key = format!("solodock-volume-{index}");
        match volume {
            VolumeInput::Owned {
                logical_name,
                target_path,
            } => {
                let name = owned_volume_name(input.slug, logical_name);
                volumes.insert(
                    key.clone(),
                    VolumeDefinition {
                        external: false,
                        name,
                        labels: resource_ownership.clone(),
                    },
                );
                mounts.push(ServiceMount {
                    kind: "volume".into(),
                    source: key,
                    target: literal(target_path),
                    read_only: false,
                });
            }
            VolumeInput::External { name, target_path } => {
                volumes.insert(
                    key.clone(),
                    VolumeDefinition {
                        external: true,
                        name: literal(name),
                        labels: BTreeMap::new(),
                    },
                );
                mounts.push(ServiceMount {
                    kind: "volume".into(),
                    source: key,
                    target: literal(target_path),
                    read_only: false,
                });
            }
        }
    }
    for bind in &input.draft.binds {
        mounts.push(ServiceMount {
            kind: "bind".into(),
            source: literal(&bind.source),
            target: literal(&bind.target_path),
            read_only: bind.readonly,
        });
    }
    for file in &input.draft.files {
        let kind = if file.sensitive { "secret" } else { "public" };
        mounts.push(ServiceMount {
            kind: "bind".into(),
            source: input
                .revision_directory
                .join("files")
                .join(kind)
                .join(&file.logical_name)
                .display()
                .to_string()
                .pipe(|value| literal(&value)),
            target: literal(&file.target_path),
            read_only: true,
        });
    }
    mounts.sort_by(|left, right| left.target.cmp(&right.target));

    let network_plan = network_plan(input.draft.owned_default_network, &input.draft.networks)
        .expect("normalized drafts always contain a valid network plan");
    let mut networks = BTreeMap::new();
    let mut service_network_keys = Vec::new();
    if network_plan.owned_default_network {
        networks.insert(
            "default".into(),
            NetworkDefinition {
                external: false,
                driver: Some("bridge".into()),
                driver_opts: BTreeMap::from([(
                    "com.docker.network.bridge.name".into(),
                    resource_names.bridge_name.clone(),
                )]),
                name: resource_names.owned_default_network_name.clone(),
                labels: resource_ownership,
            },
        );
        service_network_keys.push("default".to_owned());
    }
    for network in &network_plan.external {
        let key = format!("solodock-network-{}", network.source_index);
        networks.insert(
            key.clone(),
            NetworkDefinition {
                external: true,
                driver: None,
                driver_opts: BTreeMap::new(),
                name: literal(&network.name),
                labels: BTreeMap::new(),
            },
        );
        service_network_keys.push(key);
    }
    let service_networks = if network_plan
        .external
        .iter()
        .all(|network| network.aliases.is_empty())
    {
        ServiceNetworks::Short(service_network_keys)
    } else {
        let mut attachments = BTreeMap::new();
        if network_plan.owned_default_network {
            attachments.insert("default".into(), ServiceNetworkAttachment::default());
        }
        for network in &network_plan.external {
            attachments.insert(
                format!("solodock-network-{}", network.source_index),
                ServiceNetworkAttachment {
                    aliases: network.aliases.clone(),
                },
            );
        }
        ServiceNetworks::Long(attachments)
    };

    let ports = input
        .draft
        .ports
        .iter()
        .enumerate()
        .map(|(index, port)| ServicePort {
            name: format!("port-{index}"),
            target: port.container_port,
            published: port.host_port.to_string(),
            host_ip: port.host_ip.clone(),
            protocol: match port.protocol {
                PortProtocol::Tcp => "tcp",
                PortProtocol::Udp => "udp",
            }
            .into(),
        })
        .collect::<Vec<_>>();
    let (restart, healthcheck) = health(&input.draft.health);
    let service = Service {
        image: literal(input.image_ref),
        labels: ownership,
        env_file: vec![
            EnvFile {
                path: literal(
                    &input
                        .revision_directory
                        .join("env/public.env")
                        .display()
                        .to_string(),
                ),
                required: true,
            },
            EnvFile {
                path: literal(
                    &input
                        .revision_directory
                        .join("secrets/runtime.env")
                        .display()
                        .to_string(),
                ),
                required: true,
            },
        ],
        volumes: mounts,
        ports,
        networks: service_networks,
        restart,
        healthcheck,
    };
    let mount_count = service.volumes.len();
    let port_count = service.ports.len();
    let network_count = service.networks.len();
    let mut services = BTreeMap::new();
    services.insert("app".into(), service);
    let document = ComposeDocument {
        services,
        volumes,
        networks,
    };
    let yaml = serde_yml::to_string(&document)?;
    let mut warnings = Vec::new();
    if !runnable {
        warnings.push("PREVIEW_IMAGE_NOT_PINNED");
    }
    if input.draft.binds.iter().any(|bind| !bind.readonly) || !input.draft.volumes.is_empty() {
        warnings.push("DATA_NOT_ROLLED_BACK");
    }
    if matches!(input.draft.health, HealthPolicy::Healthy { http: Some(_) }) {
        warnings.push("HEALTHCHECK_CLIENT_REQUIRED");
    }
    if !network_plan.external.is_empty() {
        warnings.push("EXTERNAL_NETWORK_UNMANAGED");
    }
    Ok((
        yaml,
        ComposePlan {
            service: "app",
            image_ref: input.image_ref.to_owned(),
            runnable,
            ports: port_count,
            mounts: mount_count,
            networks: network_count,
            network_mode: network_plan.mode,
            owned_default_network: network_plan.owned_default_network.then_some({
                OwnedDefaultNetworkIdentity {
                    docker_name: resource_names.owned_default_network_name,
                    bridge_name: resource_names.bridge_name,
                }
            }),
            external_networks: network_plan.external,
            warnings,
        },
    ))
}

fn literal(value: &str) -> String {
    value.replace('$', "$$")
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn service_labels(app_id: Uuid, release_id: Uuid) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (SCHEMA_LABEL.into(), "1".into()),
        (APP_ID_LABEL.into(), app_id.to_string()),
        (RELEASE_ID_LABEL.into(), release_id.to_string()),
    ])
}

pub fn resource_labels(app_id: Uuid, project_name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (SCHEMA_LABEL.into(), "1".into()),
        (APP_ID_LABEL.into(), app_id.to_string()),
        ("com.solodock.project-name".into(), project_name.to_owned()),
    ])
}

fn health(policy: &HealthPolicy) -> (String, Option<Healthcheck>) {
    match policy {
        HealthPolicy::Healthy { http: None } => ("unless-stopped".into(), None),
        HealthPolicy::Healthy { http: Some(http) } => {
            let host = if http.host == "::1" {
                "[::1]"
            } else {
                http.host.as_str()
            };
            let url = literal(&format!("http://{host}:{}{}", http.port, http.path));
            let test = match http.client {
                HttpClient::Curl => vec![
                    "CMD".into(),
                    "curl".into(),
                    "--fail".into(),
                    "--silent".into(),
                    url,
                ],
                HttpClient::Wget => vec![
                    "CMD".into(),
                    "wget".into(),
                    "--quiet".into(),
                    "--spider".into(),
                    url,
                ],
            };
            (
                "unless-stopped".into(),
                Some(Healthcheck {
                    test,
                    interval: Some(format!("{}s", http.interval_seconds)),
                    timeout: Some(format!("{}s", http.timeout_seconds)),
                    retries: Some(http.retries),
                    start_period: Some(format!("{}s", http.start_period_seconds)),
                    disable: false,
                }),
            )
        }
        HealthPolicy::Completed => ("no".into(), None),
        HealthPolicy::Running { .. } => ("unless-stopped".into(), None),
        HealthPolicy::Disabled { .. } => (
            "unless-stopped".into(),
            Some(Healthcheck {
                test: Vec::new(),
                interval: None,
                timeout: None,
                retries: None,
                start_period: None,
                disable: true,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network_draft(
        owned_default_network: bool,
        networks: Vec<crate::domain::NetworkInput>,
    ) -> crate::domain::NormalizedDraft {
        crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Example".into(),
                discovery_image_ref: "registry.example/app:stable".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                environment: crate::domain::EnvironmentInput::default(),
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network,
                networks,
                health: crate::domain::HealthPolicy::default(),
            },
            &crate::domain::ExistingSecrets::default(),
            b"key",
            &[],
        )
        .unwrap()
    }

    #[test]
    fn generated_yaml_is_single_service_and_never_contains_secret_material() {
        let draft = crate::domain::normalize_draft(
            crate::domain::DraftInput {
                display_name: "Example".into(),
                discovery_image_ref: "registry.example/app:latest".into(),
                credential_ref: None,
                auto_deploy_enabled: false,
                auto_deploy_acknowledged: false,
                poll_interval_seconds: 300,
                environment: crate::domain::EnvironmentInput {
                    public: vec![],
                    secrets: vec![crate::domain::SecretEnvInput {
                        key: "TOKEN".into(),
                        operation: crate::domain::SecretOperation::Replace {
                            value: "canary-secret".into(),
                        },
                    }],
                },
                files: vec![],
                ports: vec![],
                volumes: vec![],
                binds: vec![],
                owned_default_network: true,
                networks: vec![],
                health: crate::domain::HealthPolicy::default(),
            },
            &crate::domain::ExistingSecrets::default(),
            b"test-hmac-key",
            &[],
        )
        .unwrap();
        let (yaml, plan) = generate(
            ComposeInput {
                app_id: Uuid::nil(),
                slug: "example",
                release_id: Uuid::new_v4(),
                image_ref: &draft.discovery_image_ref,
                revision_directory: Path::new("/var/lib/solodock/apps/revision"),
                draft: &draft,
            },
            false,
        )
        .unwrap();
        assert!(yaml.contains("app:"));
        assert!(!yaml.contains("canary-secret"));
        assert!(!plan.runnable);
    }

    #[test]
    fn compose_interpolation_metacharacters_are_emitted_as_literals() {
        assert_eq!(literal("/srv/${APP}/$VALUE"), "/srv/$${APP}/$$VALUE");
    }

    #[test]
    fn user_volume_names_cannot_collide_with_internal_resource_keys() {
        let input = crate::domain::DraftInput {
            display_name: "Example".into(),
            discovery_image_ref: "registry.example/app:stable".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: crate::domain::EnvironmentInput::default(),
            files: vec![],
            ports: vec![],
            volumes: vec![
                crate::domain::VolumeInput::Owned {
                    logical_name: "external-2".into(),
                    target_path: "/data/owned".into(),
                },
                crate::domain::VolumeInput::External {
                    name: "customer-data".into(),
                    target_path: "/data/external".into(),
                },
            ],
            binds: vec![],
            owned_default_network: true,
            networks: vec![],
            health: crate::domain::HealthPolicy::default(),
        };
        let draft = crate::domain::normalize_draft(
            input,
            &crate::domain::ExistingSecrets::default(),
            b"key",
            &[],
        )
        .unwrap();
        let (yaml, _) = generate(
            ComposeInput {
                app_id: Uuid::nil(),
                slug: "example",
                release_id: Uuid::new_v4(),
                image_ref: &draft.discovery_image_ref,
                revision_directory: Path::new("/var/lib/solodock/apps/revision"),
                draft: &draft,
            },
            false,
        )
        .unwrap();
        assert!(yaml.contains("solodock-volume-0:"));
        assert!(yaml.contains("solodock-volume-1:"));
        assert!(yaml.contains("source: solodock-volume-0"));
        assert!(yaml.contains("source: solodock-volume-1"));
        assert_eq!(yaml.matches("customer-data").count(), 1);
    }

    #[test]
    fn network_modes_and_aliases_use_typed_deterministic_compose() {
        let app_id = Uuid::nil();
        let draft = network_draft(
            false,
            vec![crate::domain::NetworkInput::External {
                name: "shared".into(),
                aliases: vec!["server".into(), "api".into()],
            }],
        );
        let input = ComposeInput {
            app_id,
            slug: "example",
            release_id: Uuid::nil(),
            image_ref: &draft.discovery_image_ref,
            revision_directory: Path::new("/var/lib/solodock/apps/revision"),
            draft: &draft,
        };
        let (first, plan) = generate(input, true).unwrap();
        let (second, _) = generate(
            ComposeInput {
                app_id,
                slug: "example",
                release_id: Uuid::nil(),
                image_ref: &draft.discovery_image_ref,
                revision_directory: Path::new("/var/lib/solodock/apps/revision"),
                draft: &draft,
            },
            true,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(plan.network_mode, crate::domain::NetworkMode::ExternalOnly);
        assert!(!first.contains("solodock-00000000000000000000000000000000-default"));
        assert!(first.contains("aliases:\n        - api\n        - server"));
        assert!(plan.warnings.contains(&"EXTERNAL_NETWORK_UNMANAGED"));
    }

    #[test]
    fn owned_and_external_networks_keep_driver_options_scoped_to_owned() {
        let draft = network_draft(
            true,
            vec![
                crate::domain::NetworkInput::OwnedDefault,
                crate::domain::NetworkInput::External {
                    name: "shared".into(),
                    aliases: vec![],
                },
            ],
        );
        let (yaml, _) = generate(
            ComposeInput {
                app_id: Uuid::nil(),
                slug: "example",
                release_id: Uuid::nil(),
                image_ref: &draft.discovery_image_ref,
                revision_directory: Path::new("/var/lib/solodock/apps/revision"),
                draft: &draft,
            },
            true,
        )
        .unwrap();
        assert!(yaml.contains("driver: bridge"));
        assert!(yaml.contains("com.docker.network.bridge.name: sd-example"));
        assert!(yaml.contains("name: solodock-example-default"));
        let external = yaml.split("solodock-network-1:").nth(1).unwrap();
        assert!(external.contains("external: true\n    name: shared"));
        assert!(!external.contains("driver:"));
        assert!(!yaml.contains("container_name"));
    }
}
