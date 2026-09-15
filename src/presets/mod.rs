pub mod pgadmin;
pub mod postgresql;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct PresetDescriptor {
    pub id: &'static str,
    pub schema_version: u32,
    pub display_name: &'static str,
    pub description: &'static str,
    #[serde(flatten)]
    pub defaults: PresetDefaults,
    pub password_generated_by_client: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum PresetDefaults {
    PostgreSql {
        default_major: &'static str,
        supported_majors: &'static [&'static str],
        default_username: &'static str,
        default_database: &'static str,
    },
    PgAdmin {
        default_host_port: u16,
        image: &'static str,
    },
}

pub fn descriptors() -> Vec<PresetDescriptor> {
    vec![postgresql::descriptor(), pgadmin::descriptor()]
}
