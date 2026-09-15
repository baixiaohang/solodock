use crate::domain::{
    DraftInput, EnvironmentInput, HealthPolicy, PortInput, PortProtocol, PublicEnvInput,
    SecretEnvInput, SecretOperation, VolumeInput,
};

use super::{PresetDefaults, PresetDescriptor};

pub const PRESET_ID: &str = "pgadmin";
pub const SCHEMA_VERSION: u32 = 1;
pub const IMAGE: &str = "dpage/pgadmin4:9.17";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Variables {
    pub email: String,
    pub password: String,
    pub host_port: u16,
}

pub fn descriptor() -> PresetDescriptor {
    PresetDescriptor {
        id: PRESET_ID,
        schema_version: SCHEMA_VERSION,
        display_name: "pgAdmin",
        description: "pgAdmin with persistent settings and internal access to PostgreSQL services.",
        defaults: PresetDefaults::PgAdmin {
            default_host_port: 5050,
            image: IMAGE,
        },
        password_generated_by_client: true,
    }
}

pub fn render(slug: &str, variables: Variables) -> Result<DraftInput, &'static str> {
    if !valid_email(&variables.email) || variables.host_port == 0 {
        return Err("PRESET_VARIABLE_INVALID");
    }
    if variables.password.len() < 16 || variables.password.len() > 256 {
        return Err("PRESET_PASSWORD_INVALID");
    }
    Ok(DraftInput {
        security_profile: None,
        display_name: slug.to_owned(),
        discovery_image_ref: IMAGE.into(),
        credential_ref: None,
        auto_deploy_enabled: false,
        auto_deploy_acknowledged: false,
        poll_interval_seconds: crate::domain::default_poll_interval(),
        stop_grace_period_seconds: crate::domain::default_stop_grace_period_seconds(),
        environment: EnvironmentInput {
            public: vec![
                PublicEnvInput {
                    key: "PGADMIN_DEFAULT_EMAIL".into(),
                    value: variables.email,
                },
                PublicEnvInput {
                    key: "PGADMIN_LISTEN_PORT".into(),
                    value: "5050".into(),
                },
                PublicEnvInput {
                    key: "PGADMIN_DISABLE_POSTFIX".into(),
                    value: "1".into(),
                },
            ],
            secrets: vec![SecretEnvInput {
                key: "PGADMIN_DEFAULT_PASSWORD".into(),
                operation: SecretOperation::Replace {
                    value: variables.password,
                },
            }],
        },
        files: Vec::new(),
        ports: vec![PortInput {
            host_ip: "127.0.0.1".into(),
            host_port: variables.host_port,
            container_port: 5050,
            protocol: PortProtocol::Tcp,
        }],
        volumes: vec![VolumeInput::Owned {
            logical_name: "data".into(),
            target_path: "/var/lib/pgadmin".into(),
        }],
        binds: Vec::new(),
        owned_default_network: true,
        service_discovery_enabled: true,
        networks: Vec::new(),
        health: HealthPolicy::Running {
            stable_window_seconds: 15,
        },
    })
}

fn valid_email(value: &str) -> bool {
    if value.len() > 254
        || !value.is_ascii()
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !domain.contains('@')
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_bootstrap_variables() {
        for email in [
            "",
            "admin",
            "admin@",
            "admin@@example.com",
            "a b@example.com",
            "a\n@example.com",
            ".a@example.com",
            "a@example..com",
        ] {
            assert!(
                render(
                    "pgadmin",
                    Variables {
                        email: email.into(),
                        password: "a-strong-password".into(),
                        host_port: 5050
                    }
                )
                .is_err()
            );
        }
        assert!(valid_email("admin+db@example.com"));
        for (password, host_port) in [
            ("short".to_owned(), 5050),
            ("x".repeat(257), 5050),
            ("a-strong-password".to_owned(), 0),
        ] {
            assert!(
                render(
                    "pgadmin",
                    Variables {
                        email: "admin@example.com".into(),
                        password,
                        host_port
                    }
                )
                .is_err()
            );
        }
    }
}
