use std::collections::HashMap;

use uuid::Uuid;

use crate::docker::AppCatalogEntry;

pub const MANAGED_LABEL: &str = "com.solodock.managed";
pub const SCHEMA_LABEL: &str = "com.solodock.schema-version";
pub const APP_ID_LABEL: &str = "com.solodock.app-id";
pub const RELEASE_ID_LABEL: &str = "com.solodock.release-id";
pub const PROJECT_LABEL: &str = "com.docker.compose.project";
pub const SERVICE_LABEL: &str = "com.docker.compose.service";
pub const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedIdentity {
    pub app_id: Uuid,
    pub release_id: Uuid,
}

pub fn validate_identity(
    labels: &HashMap<String, String>,
    app: &AppCatalogEntry,
) -> Option<OwnedIdentity> {
    let identity = validate_syntactic_identity(labels, app)?;
    (app.active_release_id == Some(identity.release_id)).then_some(identity)
}

/// Validates only the immutable SoloDock/Compose identity. Callers must apply
/// their own active/pending/deployment release policy afterwards.
pub fn validate_syntactic_identity(
    labels: &HashMap<String, String>,
    app: &AppCatalogEntry,
) -> Option<OwnedIdentity> {
    let app_id = canonical_uuid(labels.get(APP_ID_LABEL)?)?;
    let release_id = canonical_uuid(labels.get(RELEASE_ID_LABEL)?)?;
    (labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
        && labels.get(SCHEMA_LABEL).map(String::as_str) == Some("1")
        && app_id == app.id
        && labels.get(PROJECT_LABEL).map(String::as_str) == Some(app.project_name.as_str())
        && labels.get(SERVICE_LABEL).map(String::as_str) == Some("app")
        && labels.get(ONEOFF_LABEL).map(String::as_str) == Some("False"))
    .then_some(OwnedIdentity { app_id, release_id })
}

pub fn release_is_allowed(identity: OwnedIdentity, releases: &[Uuid]) -> bool {
    releases.contains(&identity.release_id)
}

pub fn validate_observed_identity(
    labels: &HashMap<String, String>,
    app: &AppCatalogEntry,
) -> Option<OwnedIdentity> {
    let identity = validate_syntactic_identity(labels, app)?;
    (Some(identity.release_id) == app.active_release_id
        || Some(identity.release_id) == app.pending_release_id)
        .then_some(identity)
}

pub fn claimed_app_id(labels: &HashMap<String, String>) -> Option<Uuid> {
    labels.get(APP_ID_LABEL)?.parse().ok()
}

pub fn is_managed_candidate(labels: &HashMap<String, String>) -> bool {
    labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
}

pub fn valid_container_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    let parsed = value.parse::<Uuid>().ok()?;
    (value == parsed.to_string()).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::AppCatalogEntry;

    fn fixture() -> (AppCatalogEntry, HashMap<String, String>, Uuid) {
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let app = AppCatalogEntry {
            id: app_id,
            slug: "example".into(),
            display_name: "Example".into(),
            project_name: "solodock-example".into(),
            active_release_id: Some(release_id),
            active_image_ref: Some(format!("example@sha256:{}", "a".repeat(64))),
            active_config_revision: None,
            active_config_sha256: None,
            pending_release_id: None,
            pending_image_ref: None,
            pending_config_revision: None,
            discovery_image_ref: None,
            draft_revision: None,
            draft_config_sha256: None,
            desired_state: crate::domain::DesiredState::Stopped,
            auto_deploy_enabled: false,
            poll_interval_seconds: 300,
            draft: None,
        };
        let labels = HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (SCHEMA_LABEL.into(), "1".into()),
            (APP_ID_LABEL.into(), app_id.to_string()),
            (RELEASE_ID_LABEL.into(), release_id.to_string()),
            (PROJECT_LABEL.into(), app.project_name.clone()),
            (SERVICE_LABEL.into(), "app".into()),
            (ONEOFF_LABEL.into(), "False".into()),
        ]);
        (app, labels, release_id)
    }

    #[test]
    fn requires_every_exact_label_and_canonical_uuid() {
        let (app, labels, release_id) = fixture();
        assert_eq!(
            validate_identity(&labels, &app).unwrap().release_id,
            release_id
        );
        for key in [
            MANAGED_LABEL,
            SCHEMA_LABEL,
            APP_ID_LABEL,
            RELEASE_ID_LABEL,
            PROJECT_LABEL,
            SERVICE_LABEL,
            ONEOFF_LABEL,
        ] {
            let mut missing = labels.clone();
            missing.remove(key);
            assert!(
                validate_identity(&missing, &app).is_none(),
                "accepted missing {key}"
            );
        }
        let mut uppercase = labels.clone();
        uppercase.insert(APP_ID_LABEL.into(), app.id.to_string().to_uppercase());
        assert!(validate_identity(&uppercase, &app).is_none());
        assert_eq!(claimed_app_id(&uppercase), Some(app.id));

        for (key, wrong) in [
            (MANAGED_LABEL, "True"),
            (SCHEMA_LABEL, "2"),
            (PROJECT_LABEL, "other-project"),
            (SERVICE_LABEL, "worker"),
            (ONEOFF_LABEL, "false"),
        ] {
            let mut changed = labels.clone();
            changed.insert(key.into(), wrong.into());
            assert!(
                validate_identity(&changed, &app).is_none(),
                "accepted invalid {key}"
            );
        }
        let mut release = labels;
        release.insert(RELEASE_ID_LABEL.into(), release_id.simple().to_string());
        assert!(validate_identity(&release, &app).is_none());

        let mut stale = release;
        stale.insert(RELEASE_ID_LABEL.into(), Uuid::new_v4().to_string());
        assert!(validate_identity(&stale, &app).is_none());
    }
}
