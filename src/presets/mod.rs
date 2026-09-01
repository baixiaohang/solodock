pub mod postgresql;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct PresetDescriptor {
    pub id: &'static str,
    pub schema_version: u32,
    pub display_name: &'static str,
    pub description: &'static str,
    pub default_major: &'static str,
    pub supported_majors: &'static [&'static str],
    pub default_username: &'static str,
    pub default_database: &'static str,
    pub password_generated_by_client: bool,
}

pub fn descriptors() -> Vec<PresetDescriptor> {
    vec![postgresql::descriptor()]
}
