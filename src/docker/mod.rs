pub mod client;
pub mod events;
pub mod logs;
pub mod models;
pub mod ownership;
pub mod probe;
pub mod stats;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use futures_util::StreamExt;
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app_store::recovery::{RecoveredApp, RecoveryReport};
use crate::domain::{DesiredState, NetworkPlan, dto::DraftResponse};

use self::{
    models::{ContainerProjection, ContainerRecord, DockerErrorKind, DockerReadApi, ProbeStatus},
    ownership::{claimed_app_id, is_managed_candidate, validate_observed_identity},
    probe::DockerSupervisor,
};

#[derive(Clone, Debug, Serialize)]
pub struct AppCatalogEntry {
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    #[serde(skip)]
    pub project_name: String,
    pub active_release_id: Option<Uuid>,
    pub active_image_ref: Option<String>,
    pub active_config_revision: Option<Uuid>,
    pub active_config_sha256: Option<String>,
    pub active_network_plan: Option<NetworkPlan>,
    pub pending_release_id: Option<Uuid>,
    pub pending_image_ref: Option<String>,
    pub pending_config_revision: Option<Uuid>,
    pub pending_network_plan: Option<NetworkPlan>,
    pub discovery_image_ref: Option<String>,
    pub draft_revision: Option<Uuid>,
    pub draft_config_sha256: Option<String>,
    pub desired_state: DesiredState,
    pub auto_deploy_enabled: bool,
    pub poll_interval_seconds: u32,
    pub draft: Option<DraftResponse>,
}

impl From<&RecoveredApp> for AppCatalogEntry {
    fn from(value: &RecoveredApp) -> Self {
        Self {
            id: value.app_id,
            slug: value.slug.clone(),
            display_name: value.display_name.clone(),
            project_name: value.project_name.clone(),
            active_release_id: value.active_release_id,
            active_image_ref: value.active_image_ref.clone(),
            active_config_revision: value.active_config_revision,
            active_config_sha256: value.active_config_sha256.clone(),
            active_network_plan: value.active_network_plan.clone(),
            pending_release_id: value.pending_release_id,
            pending_image_ref: value.pending_image_ref.clone(),
            pending_config_revision: value.pending_config_revision,
            pending_network_plan: value.pending_network_plan.clone(),
            discovery_image_ref: value.discovery_image_ref.clone(),
            draft_revision: value.draft_revision,
            draft_config_sha256: value.draft_config_sha256.clone(),
            desired_state: value.desired_state,
            auto_deploy_enabled: value.auto_deploy_enabled,
            poll_interval_seconds: value.poll_interval_seconds,
            draft: value.draft.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppCatalog {
    snapshot: Arc<RwLock<Arc<CatalogSnapshot>>>,
}

#[derive(Clone, Debug, Default)]
pub struct CatalogSnapshot {
    pub apps: Vec<AppCatalogEntry>,
    pub recovery_issues: HashMap<&'static str, usize>,
}

impl AppCatalog {
    pub fn from_recovery(report: &RecoveryReport) -> Self {
        let mut issues = HashMap::new();
        for issue in &report.issues {
            *issues.entry(issue.code).or_insert(0) += 1;
        }
        Self {
            snapshot: Arc::new(RwLock::new(Arc::new(CatalogSnapshot {
                apps: report
                    .valid_apps
                    .iter()
                    .map(AppCatalogEntry::from)
                    .collect(),
                recovery_issues: issues,
            }))),
        }
    }

    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.snapshot
            .read()
            .expect("catalog lock is not poisoned")
            .clone()
    }

    pub fn replace(&self, report: &RecoveryReport) {
        let replacement = Self::from_recovery(report).snapshot();
        *self.snapshot.write().expect("catalog lock is not poisoned") = replacement;
    }

    pub fn get(&self, id: Uuid) -> Option<AppCatalogEntry> {
        self.snapshot()
            .apps
            .iter()
            .find(|app| app.id == id)
            .cloned()
    }

    pub fn recovery_issues(&self) -> HashMap<&'static str, usize> {
        self.snapshot().recovery_issues.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DriftCode {
    DockerUnavailable,
    ContainerMissing,
    ContainerAmbiguous,
    LabelInvalid,
    OrphanContainer,
    ActiveReleaseMissing,
    ReleaseIdMismatch,
    ImageRefMismatch,
    DeploymentPending,
    NetworkAttachmentMismatch,
    NetworkAliasMismatch,
    NetworkBridgeIdentityMismatch,
}

#[derive(Clone, Debug, Serialize)]
pub struct DriftIssue {
    pub code: DriftCode,
    pub app_id: Option<Uuid>,
    pub container_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveRelease {
    pub id: Uuid,
    pub image_ref: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AppObservation {
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub active_release: Option<ActiveRelease>,
    pub actual_release_id: Option<Uuid>,
    pub actual: Option<ContainerProjection>,
    pub expected_network_plan: Option<NetworkPlan>,
    pub expected_owned_default_network: Option<crate::compose::OwnedDefaultNetworkIdentity>,
    pub actual_owned_default_network: Option<OwnedDefaultNetworkObservation>,
    pub drift_codes: Vec<DriftCode>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OwnedDefaultNetworkObservation {
    pub docker_name: String,
    pub driver: Option<String>,
    pub bridge_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ObservationSnapshot {
    pub docker_status: ProbeStatus,
    pub observed_at: OffsetDateTime,
    pub apps: Vec<AppObservation>,
    pub issues: Vec<DriftIssue>,
    pub complete: bool,
}

#[derive(Clone)]
pub struct DockerObserver {
    pub api: Arc<dyn DockerReadApi>,
    pub catalog: AppCatalog,
    pub supervisor: DockerSupervisor,
}

impl DockerObserver {
    pub fn api(&self) -> Arc<dyn DockerReadApi> {
        self.api.clone()
    }
    pub fn new(
        api: Arc<dyn DockerReadApi>,
        catalog: AppCatalog,
        supervisor: DockerSupervisor,
    ) -> Self {
        Self {
            api,
            catalog,
            supervisor,
        }
    }

    pub async fn snapshot(&self) -> ObservationSnapshot {
        let probe = self.supervisor.current().await;
        if probe.status != ProbeStatus::Ready {
            let issues = vec![DriftIssue {
                code: DriftCode::DockerUnavailable,
                app_id: None,
                container_id: None,
            }];
            return ObservationSnapshot {
                docker_status: probe.status,
                observed_at: probe.observed_at,
                apps: self
                    .catalog
                    .snapshot()
                    .apps
                    .iter()
                    .map(|app| {
                        let expected_network_plan = select_expected_network_plan(app, None);
                        AppObservation {
                            id: app.id,
                            slug: app.slug.clone(),
                            display_name: app.display_name.clone(),
                            active_release: active_release(app),
                            actual_release_id: None,
                            actual: None,
                            expected_owned_default_network: expected_owned_default_network(
                                app,
                                expected_network_plan.as_ref(),
                            ),
                            expected_network_plan,
                            actual_owned_default_network: None,
                            drift_codes: vec![DriftCode::DockerUnavailable],
                        }
                    })
                    .collect(),
                issues,
                complete: false,
            };
        }
        match self.api.list_managed_containers().await {
            Ok(containers) => {
                let mut snapshot = associate(&self.catalog, containers, probe.status);
                self.observe_owned_networks(&mut snapshot).await;
                snapshot
            }
            Err(_) => ObservationSnapshot {
                docker_status: ProbeStatus::Unavailable,
                observed_at: OffsetDateTime::now_utc(),
                apps: self
                    .catalog
                    .snapshot()
                    .apps
                    .iter()
                    .map(|app| {
                        let expected_network_plan = select_expected_network_plan(app, None);
                        AppObservation {
                            id: app.id,
                            slug: app.slug.clone(),
                            display_name: app.display_name.clone(),
                            active_release: active_release(app),
                            actual_release_id: None,
                            actual: None,
                            expected_owned_default_network: expected_owned_default_network(
                                app,
                                expected_network_plan.as_ref(),
                            ),
                            expected_network_plan,
                            actual_owned_default_network: None,
                            drift_codes: vec![DriftCode::DockerUnavailable],
                        }
                    })
                    .collect(),
                issues: vec![DriftIssue {
                    code: DriftCode::DockerUnavailable,
                    app_id: None,
                    container_id: None,
                }],
                complete: false,
            },
        }
    }

    async fn observe_owned_networks(&self, snapshot: &mut ObservationSnapshot) {
        self.observe_owned_networks_with_timeout(snapshot, std::time::Duration::from_secs(5))
            .await;
    }

    async fn observe_owned_networks_with_timeout(
        &self,
        snapshot: &mut ObservationSnapshot,
        timeout: std::time::Duration,
    ) {
        let identities = snapshot
            .apps
            .iter()
            .filter_map(|app| {
                app.expected_owned_default_network
                    .as_ref()
                    .map(|identity| (app.id, identity.clone()))
            })
            .collect::<Vec<_>>();
        let requests = identities.into_iter().map(|(app_id, identity)| {
            let api = self.api.clone();
            async move {
                (
                    app_id,
                    identity.clone(),
                    api.inspect_network(&identity.docker_name).await,
                )
            }
        });
        let deadline = tokio::time::Instant::now() + timeout;
        let results = tokio::time::timeout_at(
            deadline,
            futures_util::stream::iter(requests)
                .buffer_unordered(8)
                .collect::<Vec<_>>(),
        )
        .await;
        let Ok(results) = results else {
            snapshot.complete = false;
            return;
        };
        for (app_id, expected, result) in results {
            let Some(app) = snapshot.apps.iter_mut().find(|app| app.id == app_id) else {
                continue;
            };
            match result {
                Ok(Some(network)) if network.name == expected.docker_name => {
                    let bridge_name = network
                        .options
                        .get("com.docker.network.bridge.name")
                        .cloned();
                    let mismatch = network.driver.as_deref() != Some("bridge")
                        || bridge_name.as_deref() != Some(expected.bridge_name.as_str());
                    app.actual_owned_default_network = Some(OwnedDefaultNetworkObservation {
                        docker_name: network.name,
                        driver: network.driver,
                        bridge_name,
                    });
                    if mismatch {
                        app.drift_codes
                            .push(DriftCode::NetworkBridgeIdentityMismatch);
                        snapshot.issues.push(issue(
                            DriftCode::NetworkBridgeIdentityMismatch,
                            Some(app_id),
                            None,
                        ));
                    }
                }
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => snapshot.complete = false,
            }
        }
    }

    pub async fn owned_container(
        &self,
        app_id: Uuid,
    ) -> Result<ContainerRecord, OwnedContainerError> {
        let app = self
            .catalog
            .get(app_id)
            .ok_or(OwnedContainerError::AppNotFound)?;
        let probe_status = self.supervisor.current().await.status;
        if probe_status != ProbeStatus::Ready {
            let kind = match probe_status {
                ProbeStatus::PermissionDenied => DockerErrorKind::PermissionDenied,
                ProbeStatus::Incompatible => DockerErrorKind::Incompatible,
                ProbeStatus::Starting | ProbeStatus::Unavailable => DockerErrorKind::Unavailable,
                ProbeStatus::Ready => unreachable!("ready status was handled above"),
            };
            return Err(OwnedContainerError::Docker(kind));
        }
        let containers = self
            .api
            .list_managed_containers()
            .await
            .map_err(|error| OwnedContainerError::Docker(error.kind))?;
        let mut valid = Vec::new();
        let mut invalid = false;
        for container in containers {
            if is_managed_candidate(&container.labels)
                && claimed_app_id(&container.labels) == Some(app_id)
            {
                if validate_observed_identity(&container.labels, &app).is_some() {
                    valid.push(container);
                } else {
                    invalid = true;
                }
            }
        }
        let candidate = match valid.len() {
            0 if invalid => return Err(OwnedContainerError::Invalid),
            0 => return Err(OwnedContainerError::Missing),
            1 if !invalid => valid.into_iter().next().expect("one candidate"),
            1 => return Err(OwnedContainerError::Invalid),
            _ => return Err(OwnedContainerError::Ambiguous),
        };
        let inspected =
            self.api
                .inspect_container(&candidate.id)
                .await
                .map_err(|error| match error.kind {
                    DockerErrorKind::ContainerChanged => OwnedContainerError::Changed,
                    kind => OwnedContainerError::Docker(kind),
                })?;
        validate_observed_identity(&inspected.labels, &app).ok_or(OwnedContainerError::Invalid)?;
        Ok(inspected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnedContainerError {
    AppNotFound,
    Missing,
    Ambiguous,
    Invalid,
    Changed,
    Docker(DockerErrorKind),
}

fn active_release(app: &AppCatalogEntry) -> Option<ActiveRelease> {
    Some(ActiveRelease {
        id: app.active_release_id?,
        image_ref: app.active_image_ref.clone()?,
    })
}

fn select_expected_network_plan(
    app: &AppCatalogEntry,
    actual_release_id: Option<Uuid>,
) -> Option<NetworkPlan> {
    match actual_release_id {
        Some(release_id) if app.pending_release_id == Some(release_id) => {
            app.pending_network_plan.clone()
        }
        Some(_) => app.active_network_plan.clone(),
        None if app.active_release_id.is_none() => app.pending_network_plan.clone(),
        None => app.active_network_plan.clone(),
    }
}

fn expected_owned_default_network(
    app: &AppCatalogEntry,
    plan: Option<&NetworkPlan>,
) -> Option<crate::compose::OwnedDefaultNetworkIdentity> {
    let plan = plan?;
    plan.owned_default_network.then(|| {
        let names = crate::domain::app_resource_names(&app.slug);
        crate::compose::OwnedDefaultNetworkIdentity {
            docker_name: names.owned_default_network_name,
            bridge_name: names.bridge_name,
        }
    })
}

fn associate(
    catalog: &AppCatalog,
    containers: Vec<ContainerRecord>,
    status: ProbeStatus,
) -> ObservationSnapshot {
    let mut issues = Vec::new();
    let mut observations = Vec::new();
    let mut considered = vec![false; containers.len()];
    let snapshot = catalog.snapshot();
    for app in &snapshot.apps {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();
        for (index, container) in containers.iter().enumerate() {
            if !is_managed_candidate(&container.labels) {
                continue;
            }
            if claimed_app_id(&container.labels) == Some(app.id) {
                considered[index] = true;
                if let Some(identity) = validate_observed_identity(&container.labels, app) {
                    valid.push((container, identity));
                } else {
                    invalid.push(container);
                }
            }
        }
        let mut codes = Vec::new();
        if !invalid.is_empty() {
            codes.push(DriftCode::LabelInvalid);
            for container in &invalid {
                issues.push(issue(
                    DriftCode::LabelInvalid,
                    Some(app.id),
                    Some(&container.id),
                ));
            }
        }
        let mut actual_release_id = None;
        let mut expected_network_plan = select_expected_network_plan(app, None);
        let actual = if valid.len() > 1 {
            codes.push(DriftCode::ContainerAmbiguous);
            for (container, _) in &valid {
                issues.push(issue(
                    DriftCode::ContainerAmbiguous,
                    Some(app.id),
                    Some(&container.id),
                ));
            }
            None
        } else if let Some((container, identity)) = valid.first() {
            actual_release_id = Some(identity.release_id);
            expected_network_plan = select_expected_network_plan(app, Some(identity.release_id));
            if app.pending_release_id.is_some() {
                codes.push(DriftCode::DeploymentPending);
            }
            let observed_image = if app.pending_release_id == Some(identity.release_id) {
                app.pending_image_ref.as_deref()
            } else {
                app.active_image_ref.as_deref()
            };
            if app.active_release_id.is_none()
                && app.pending_release_id != Some(identity.release_id)
            {
                codes.push(DriftCode::ActiveReleaseMissing);
            } else if app.active_release_id != Some(identity.release_id)
                && app.pending_release_id != Some(identity.release_id)
            {
                codes.push(DriftCode::ReleaseIdMismatch);
            }
            if observed_image != container.configured_image_ref.as_deref() {
                codes.push(DriftCode::ImageRefMismatch);
            }
            if let Some(plan) = &expected_network_plan {
                let names = crate::domain::app_resource_names(&app.slug);
                let expected = plan.expected_networks(&names);
                let expected_names = expected
                    .iter()
                    .map(|network| network.name.as_str())
                    .collect::<std::collections::BTreeSet<_>>();
                let actual_names = container
                    .networks
                    .iter()
                    .map(|network| network.name.as_str())
                    .collect::<std::collections::BTreeSet<_>>();
                if expected_names != actual_names {
                    codes.push(DriftCode::NetworkAttachmentMismatch);
                } else if expected.iter().any(|network| {
                    network.kind == crate::domain::ExpectedNetworkKind::External
                        && network.aliases.iter().any(|alias| {
                            !container.networks.iter().any(|actual| {
                                actual.name == network.name && actual.aliases.contains(alias)
                            })
                        })
                }) {
                    codes.push(DriftCode::NetworkAliasMismatch);
                }
            }
            for code in codes
                .iter()
                .filter(|code| **code != DriftCode::LabelInvalid)
            {
                issues.push(issue(*code, Some(app.id), Some(&container.id)));
            }
            Some(ContainerProjection::from(*container))
        } else if !invalid.is_empty() {
            None
        } else {
            codes.push(DriftCode::ContainerMissing);
            issues.push(issue(DriftCode::ContainerMissing, Some(app.id), None));
            None
        };
        observations.push(AppObservation {
            id: app.id,
            slug: app.slug.clone(),
            display_name: app.display_name.clone(),
            active_release: active_release(app),
            actual_release_id,
            actual,
            expected_network_plan: expected_network_plan.clone(),
            expected_owned_default_network: expected_owned_default_network(
                app,
                expected_network_plan.as_ref(),
            ),
            actual_owned_default_network: None,
            drift_codes: codes,
        });
    }
    for (index, container) in containers.iter().enumerate() {
        if !considered[index] && is_managed_candidate(&container.labels) {
            issues.push(issue(
                DriftCode::OrphanContainer,
                claimed_app_id(&container.labels),
                Some(&container.id),
            ));
        }
    }
    ObservationSnapshot {
        docker_status: status,
        observed_at: OffsetDateTime::now_utc(),
        apps: observations,
        issues,
        complete: true,
    }
}

fn issue(code: DriftCode, app_id: Option<Uuid>, container_id: Option<&str>) -> DriftIssue {
    DriftIssue {
        code,
        app_id,
        container_id: container_id.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;

    use super::*;
    use crate::docker::{models::*, ownership::*};

    fn fixture() -> (AppCatalog, Uuid, Uuid, ContainerRecord) {
        let app_id = Uuid::new_v4();
        let release_id = Uuid::new_v4();
        let image = format!("example@sha256:{}", "a".repeat(64));
        let app = RecoveredApp {
            app_id,
            slug: "example".into(),
            display_name: "Example".into(),
            project_name: "solodock-example".into(),
            active_release_id: Some(release_id),
            active_image_ref: Some(image.clone()),
            active_config_revision: None,
            active_config_sha256: None,
            active_network_plan: None,
            pending_release_id: None,
            pending_image_ref: None,
            pending_config_revision: None,
            pending_network_plan: None,
            discovery_image_ref: None,
            draft_revision: None,
            draft_config_sha256: None,
            desired_state: DesiredState::Stopped,
            poll_interval_seconds: 300,
            auto_deploy_enabled: false,
            last_operation_id: None,
            draft: None,
            source_updated_at: OffsetDateTime::UNIX_EPOCH,
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
        let container = ContainerRecord {
            id: "a".repeat(64),
            name: "solodock-example-app-1".into(),
            labels,
            status: ContainerStatus::Running,
            health: HealthStatus::Healthy,
            exit_code: None,
            restart_count: Some(0),
            started_at: None,
            finished_at: None,
            configured_image_ref: Some(image),
            image_id: None,
            manifest_descriptor: None,
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        };
        (
            AppCatalog::from_recovery(&RecoveryReport {
                valid_apps: vec![app],
                issues: Vec::new(),
            }),
            app_id,
            release_id,
            container,
        )
    }

    #[test]
    fn valid_and_invalid_candidates_keep_issue_container_ids_exact() {
        let (catalog, app_id, _, valid) = fixture();
        let mut invalid = valid.clone();
        invalid.id = "b".repeat(64);
        invalid
            .labels
            .insert(PROJECT_LABEL.into(), "wrong-project".into());
        let snapshot = associate(
            &catalog,
            vec![valid.clone(), invalid.clone()],
            ProbeStatus::Ready,
        );

        assert_eq!(snapshot.apps[0].actual.as_ref().unwrap().id, valid.id);
        assert!(
            snapshot.apps[0]
                .drift_codes
                .contains(&DriftCode::LabelInvalid)
        );
        let label_issues: Vec<_> = snapshot
            .issues
            .iter()
            .filter(|issue| issue.code == DriftCode::LabelInvalid)
            .collect();
        assert_eq!(label_issues.len(), 1);
        assert_eq!(label_issues[0].app_id, Some(app_id));
        assert_eq!(
            label_issues[0].container_id.as_deref(),
            Some(invalid.id.as_str())
        );
    }

    fn catalog_with_network_plan(
        app_id: Uuid,
        release_id: Uuid,
        image: String,
        plan: crate::domain::NetworkPlan,
    ) -> AppCatalog {
        AppCatalog::from_recovery(&RecoveryReport {
            valid_apps: vec![RecoveredApp {
                app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                project_name: "solodock-example".into(),
                active_release_id: Some(release_id),
                active_image_ref: Some(image),
                active_config_revision: Some(Uuid::new_v4()),
                active_config_sha256: Some("a".repeat(64)),
                active_network_plan: Some(plan),
                pending_release_id: None,
                pending_image_ref: None,
                pending_config_revision: None,
                pending_network_plan: None,
                discovery_image_ref: None,
                draft_revision: None,
                draft_config_sha256: None,
                desired_state: DesiredState::Running,
                poll_interval_seconds: 300,
                auto_deploy_enabled: false,
                last_operation_id: None,
                draft: None,
                source_updated_at: OffsetDateTime::UNIX_EPOCH,
            }],
            issues: vec![],
        })
    }

    fn catalog_with_pending_network_plan(
        app_id: Uuid,
        release_id: Uuid,
        plan: crate::domain::NetworkPlan,
    ) -> AppCatalog {
        AppCatalog::from_recovery(&RecoveryReport {
            valid_apps: vec![RecoveredApp {
                app_id,
                slug: "example".into(),
                display_name: "Example".into(),
                project_name: "solodock-example".into(),
                active_release_id: None,
                active_image_ref: None,
                active_config_revision: None,
                active_config_sha256: None,
                active_network_plan: None,
                pending_release_id: Some(release_id),
                pending_image_ref: Some(format!("example@sha256:{}", "a".repeat(64))),
                pending_config_revision: Some(Uuid::new_v4()),
                pending_network_plan: Some(plan),
                discovery_image_ref: None,
                draft_revision: None,
                draft_config_sha256: None,
                desired_state: DesiredState::Running,
                poll_interval_seconds: 300,
                auto_deploy_enabled: false,
                last_operation_id: None,
                draft: None,
                source_updated_at: OffsetDateTime::UNIX_EPOCH,
            }],
            issues: vec![],
        })
    }

    #[test]
    fn network_drift_requires_exact_attachments_and_expected_alias_subset() {
        let (_, app_id, release_id, mut container) = fixture();
        let plan = crate::domain::network_plan(
            false,
            &[crate::domain::NetworkInput::External {
                name: "shared".into(),
                aliases: vec!["api".into()],
            }],
        )
        .unwrap();
        let image = container.configured_image_ref.clone().unwrap();
        container.networks = vec![NetworkProjection {
            name: "shared".into(),
            container_ip: Some("172.20.0.2".into()),
            aliases: vec!["api".into(), "automatic-name".into()],
        }];
        let catalog = catalog_with_network_plan(app_id, release_id, image.clone(), plan.clone());
        let snapshot = associate(&catalog, vec![container.clone()], ProbeStatus::Ready);
        assert!(
            !snapshot.apps[0]
                .drift_codes
                .contains(&DriftCode::NetworkAliasMismatch)
        );

        container.networks[0].aliases = vec!["automatic-name".into()];
        let snapshot = associate(&catalog, vec![container.clone()], ProbeStatus::Ready);
        assert!(
            snapshot.apps[0]
                .drift_codes
                .contains(&DriftCode::NetworkAliasMismatch)
        );

        container.networks.push(NetworkProjection {
            name: "unexpected".into(),
            container_ip: None,
            aliases: vec![],
        });
        let snapshot = associate(&catalog, vec![container], ProbeStatus::Ready);
        assert!(
            snapshot.apps[0]
                .drift_codes
                .contains(&DriftCode::NetworkAttachmentMismatch)
        );
    }

    struct NetworkObserverApi {
        response: Result<Option<DockerNetworkResource>, DockerErrorKind>,
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DockerReadApi for NetworkObserverApi {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            unreachable!()
        }

        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            unreachable!()
        }

        async fn inspect_container(&self, _id: &str) -> Result<ContainerRecord, DockerError> {
            unreachable!()
        }

        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            unreachable!()
        }

        async fn logs(
            &self,
            _id: &str,
            _request: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            unreachable!()
        }

        async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
            unreachable!()
        }

        async fn inspect_network(
            &self,
            _name: &str,
        ) -> Result<Option<DockerNetworkResource>, DockerError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(self.delay).await;
            match &self.response {
                Ok(network) => Ok(network.clone()),
                Err(kind) => Err(DockerError::new(*kind)),
            }
        }
    }

    fn network_observer(
        catalog: AppCatalog,
        response: Result<Option<DockerNetworkResource>, DockerErrorKind>,
        delay: Duration,
        calls: Arc<AtomicUsize>,
    ) -> DockerObserver {
        DockerObserver::new(
            Arc::new(NetworkObserverApi {
                response,
                delay,
                calls,
            }),
            catalog,
            DockerSupervisor::default(),
        )
    }

    fn owned_network_resource(driver: &str, bridge_name: &str) -> DockerNetworkResource {
        DockerNetworkResource {
            name: "solodock-example-default".into(),
            labels: HashMap::new(),
            driver: Some(driver.into()),
            options: HashMap::from([("com.docker.network.bridge.name".into(), bridge_name.into())]),
        }
    }

    #[tokio::test]
    async fn pending_only_network_identity_is_observed_without_a_container() {
        let app_id = Uuid::new_v4();
        let plan = crate::domain::network_plan(true, &[crate::domain::NetworkInput::OwnedDefault])
            .unwrap();
        let catalog = catalog_with_pending_network_plan(app_id, Uuid::new_v4(), plan);
        let mut snapshot = associate(&catalog, vec![], ProbeStatus::Ready);
        assert!(snapshot.apps[0].expected_network_plan.is_some());
        assert_eq!(
            snapshot.apps[0]
                .expected_owned_default_network
                .as_ref()
                .unwrap()
                .bridge_name,
            "sd-example"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let observer = network_observer(
            catalog,
            Ok(Some(owned_network_resource("bridge", "sd-example"))),
            Duration::ZERO,
            calls.clone(),
        );
        observer
            .observe_owned_networks_with_timeout(&mut snapshot, Duration::from_millis(50))
            .await;
        assert!(snapshot.complete);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            snapshot.apps[0]
                .actual_owned_default_network
                .as_ref()
                .unwrap()
                .bridge_name
                .as_deref(),
            Some("sd-example")
        );
        assert!(
            !snapshot.apps[0]
                .drift_codes
                .contains(&DriftCode::NetworkBridgeIdentityMismatch)
        );
    }

    #[tokio::test]
    async fn network_observation_classifies_mismatch_error_timeout_and_external_only() {
        let app_id = Uuid::new_v4();
        let owned = crate::domain::network_plan(true, &[crate::domain::NetworkInput::OwnedDefault])
            .unwrap();
        let catalog = catalog_with_pending_network_plan(app_id, Uuid::new_v4(), owned);

        for resource in [
            owned_network_resource("overlay", "sd-example"),
            owned_network_resource("bridge", "br-wrong"),
        ] {
            let mut snapshot = associate(&catalog, vec![], ProbeStatus::Ready);
            let observer = network_observer(
                catalog.clone(),
                Ok(Some(resource)),
                Duration::ZERO,
                Arc::new(AtomicUsize::new(0)),
            );
            observer
                .observe_owned_networks_with_timeout(&mut snapshot, Duration::from_millis(50))
                .await;
            assert!(snapshot.complete);
            assert!(
                snapshot.apps[0]
                    .drift_codes
                    .contains(&DriftCode::NetworkBridgeIdentityMismatch)
            );
        }

        for (response, delay, timeout) in [
            (
                Err(DockerErrorKind::Unavailable),
                Duration::ZERO,
                Duration::from_millis(50),
            ),
            (
                Ok(Some(owned_network_resource("bridge", "sd-example"))),
                Duration::from_millis(50),
                Duration::from_millis(1),
            ),
        ] {
            let mut snapshot = associate(&catalog, vec![], ProbeStatus::Ready);
            let observer = network_observer(
                catalog.clone(),
                response,
                delay,
                Arc::new(AtomicUsize::new(0)),
            );
            observer
                .observe_owned_networks_with_timeout(&mut snapshot, timeout)
                .await;
            assert!(!snapshot.complete);
            assert!(
                !snapshot.apps[0]
                    .drift_codes
                    .contains(&DriftCode::NetworkBridgeIdentityMismatch)
            );
        }

        let external = crate::domain::network_plan(
            false,
            &[crate::domain::NetworkInput::External {
                name: "shared".into(),
                aliases: vec![],
            }],
        )
        .unwrap();
        let external_catalog = catalog_with_pending_network_plan(app_id, Uuid::new_v4(), external);
        let mut snapshot = associate(&external_catalog, vec![], ProbeStatus::Ready);
        let calls = Arc::new(AtomicUsize::new(0));
        let observer = network_observer(
            external_catalog,
            Err(DockerErrorKind::Unavailable),
            Duration::ZERO,
            calls.clone(),
        );
        observer
            .observe_owned_networks_with_timeout(&mut snapshot, Duration::from_millis(50))
            .await;
        assert!(snapshot.complete);
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(snapshot.apps[0].expected_owned_default_network.is_none());
    }

    struct ObserverApi {
        containers: Vec<ContainerRecord>,
        list_error: Option<DockerErrorKind>,
        inspect_error: Option<DockerErrorKind>,
    }

    #[async_trait]
    impl DockerReadApi for ObserverApi {
        async fn probe(&self) -> Result<ProbeSnapshot, DockerError> {
            unreachable!()
        }

        async fn list_managed_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
            if let Some(kind) = self.list_error {
                Err(DockerError::new(kind))
            } else {
                Ok(self.containers.clone())
            }
        }

        async fn inspect_container(&self, id: &str) -> Result<ContainerRecord, DockerError> {
            if let Some(kind) = self.inspect_error {
                return Err(DockerError::new(kind));
            }
            self.containers
                .iter()
                .find(|container| container.id == id)
                .cloned()
                .ok_or_else(|| DockerError::new(DockerErrorKind::ContainerChanged))
        }

        async fn events(&self) -> Result<DockerStream<RawDockerEvent>, DockerError> {
            unreachable!()
        }

        async fn logs(
            &self,
            _id: &str,
            _request: LogRequest,
        ) -> Result<DockerStream<LogChunk>, DockerError> {
            unreachable!()
        }

        async fn stats(&self, _id: &str) -> Result<DockerStream<RawStats>, DockerError> {
            unreachable!()
        }
    }

    fn observer(catalog: AppCatalog, api: ObserverApi) -> DockerObserver {
        DockerObserver::new(
            Arc::new(api),
            catalog,
            DockerSupervisor::from_snapshot(ProbeSnapshot {
                status: ProbeStatus::Ready,
                error_code: None,
                server_version: None,
                api_version: None,
                os: None,
                architecture: None,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                docker_root_directory: None,
            }),
        )
    }

    #[tokio::test]
    async fn pending_only_network_plan_is_consistent_when_docker_or_listing_is_unavailable() {
        let app_id = Uuid::new_v4();
        let plan = crate::domain::network_plan(true, &[crate::domain::NetworkInput::OwnedDefault])
            .unwrap();
        let catalog = catalog_with_pending_network_plan(app_id, Uuid::new_v4(), plan);
        let api = ObserverApi {
            containers: vec![],
            list_error: None,
            inspect_error: None,
        };
        let unavailable = DockerObserver::new(
            Arc::new(api),
            catalog.clone(),
            DockerSupervisor::from_snapshot(ProbeSnapshot {
                status: ProbeStatus::Unavailable,
                error_code: Some("DOCKER_UNAVAILABLE"),
                server_version: None,
                api_version: None,
                os: None,
                architecture: None,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                docker_root_directory: None,
            }),
        );
        let snapshot = unavailable.snapshot().await;
        assert_eq!(
            snapshot.apps[0]
                .expected_owned_default_network
                .as_ref()
                .unwrap()
                .bridge_name,
            "sd-example"
        );

        let list_failure = observer(
            catalog,
            ObserverApi {
                containers: vec![],
                list_error: Some(DockerErrorKind::Unavailable),
                inspect_error: None,
            },
        );
        let snapshot = list_failure.snapshot().await;
        assert_eq!(
            snapshot.apps[0]
                .expected_owned_default_network
                .as_ref()
                .unwrap()
                .bridge_name,
            "sd-example"
        );
    }

    #[tokio::test]
    async fn owned_container_preserves_typed_failures_and_invalid_identity() {
        let (catalog, app_id, _, valid) = fixture();
        let permission = observer(
            catalog.clone(),
            ObserverApi {
                containers: Vec::new(),
                list_error: Some(DockerErrorKind::PermissionDenied),
                inspect_error: None,
            },
        );
        assert!(matches!(
            permission.owned_container(app_id).await,
            Err(OwnedContainerError::Docker(
                DockerErrorKind::PermissionDenied
            ))
        ));

        let mut invalid = valid.clone();
        invalid
            .labels
            .insert(PROJECT_LABEL.into(), "wrong-project".into());
        let invalid_observer = observer(
            catalog.clone(),
            ObserverApi {
                containers: vec![invalid],
                list_error: None,
                inspect_error: None,
            },
        );
        assert!(matches!(
            invalid_observer.owned_container(app_id).await,
            Err(OwnedContainerError::Invalid)
        ));

        let inspect_permission = observer(
            catalog,
            ObserverApi {
                containers: vec![valid],
                list_error: None,
                inspect_error: Some(DockerErrorKind::PermissionDenied),
            },
        );
        assert!(matches!(
            inspect_permission.owned_container(app_id).await,
            Err(OwnedContainerError::Docker(
                DockerErrorKind::PermissionDenied
            ))
        ));
    }
}
