use crate::domain::{
    DraftInput, EnvironmentInput, HealthPolicy, PublicEnvInput, SecretEnvInput, SecretOperation,
    VolumeInput,
};

use super::PresetDescriptor;

pub const PRESET_ID: &str = "postgresql";
pub const SCHEMA_VERSION: u32 = 1;

pub struct Variables {
    pub major: String,
    pub username: String,
    pub database: String,
    pub password: String,
    pub initdb_args: String,
}

pub fn descriptor() -> PresetDescriptor {
    PresetDescriptor {
        id: PRESET_ID,
        schema_version: SCHEMA_VERSION,
        display_name: "PostgreSQL",
        description: "Single-instance PostgreSQL with a persistent volume and the platform service-discovery network.",
        default_major: "18",
        supported_majors: &["18", "17"],
        default_username: "postgres",
        default_database: "postgres",
        password_generated_by_client: true,
    }
}

pub fn render(slug: &str, variables: Variables) -> Result<DraftInput, &'static str> {
    let (image, target_path) = match variables.major.as_str() {
        "18" => ("postgres:18", "/var/lib/postgresql"),
        "17" => ("postgres:17", "/var/lib/postgresql/data"),
        _ => return Err("PRESET_MAJOR_UNSUPPORTED"),
    };
    if !valid_identifier(&variables.username) || !valid_identifier(&variables.database) {
        return Err("PRESET_VARIABLE_INVALID");
    }
    if variables.password.len() < 16 || variables.password.len() > 256 {
        return Err("PRESET_PASSWORD_INVALID");
    }
    if variables.initdb_args.len() > 512 || variables.initdb_args.contains('\0') {
        return Err("PRESET_VARIABLE_INVALID");
    }
    let mut public = vec![
        PublicEnvInput {
            key: "POSTGRES_USER".into(),
            value: variables.username,
        },
        PublicEnvInput {
            key: "POSTGRES_DB".into(),
            value: variables.database,
        },
    ];
    if !variables.initdb_args.is_empty() {
        public.push(PublicEnvInput {
            key: "POSTGRES_INITDB_ARGS".into(),
            value: variables.initdb_args,
        });
    }
    Ok(DraftInput {
        display_name: slug.to_owned(),
        discovery_image_ref: image.into(),
        credential_ref: None,
        auto_deploy_enabled: false,
        auto_deploy_acknowledged: false,
        poll_interval_seconds: crate::domain::default_poll_interval(),
        stop_grace_period_seconds: crate::domain::default_stop_grace_period_seconds(),
        environment: EnvironmentInput {
            public,
            secrets: vec![SecretEnvInput {
                key: "POSTGRES_PASSWORD".into(),
                operation: SecretOperation::Replace {
                    value: variables.password,
                },
            }],
        },
        files: Vec::new(),
        ports: Vec::new(),
        volumes: vec![VolumeInput::Owned {
            logical_name: "data".into(),
            target_path: target_path.into(),
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

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_major_specific_volume_targets() {
        for (major, expected) in [
            ("18", "/var/lib/postgresql"),
            ("17", "/var/lib/postgresql/data"),
        ] {
            let draft = render(
                "postgres",
                Variables {
                    major: major.into(),
                    username: "postgres".into(),
                    database: "postgres".into(),
                    password: "a-strong-password".into(),
                    initdb_args: String::new(),
                },
            )
            .unwrap();
            assert!(
                matches!(&draft.volumes[0], VolumeInput::Owned { target_path, .. } if target_path == expected)
            );
            assert!(draft.ports.is_empty());
            assert!(draft.service_discovery_enabled);
        }
    }
}
