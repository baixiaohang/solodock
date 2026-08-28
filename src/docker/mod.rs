pub mod client;
pub mod events;
pub mod logs;
pub mod models;
pub mod ownership;
pub mod probe;
pub mod stats;

use std::{collections::HashMap, sync::Arc};

use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app_store::recovery::{RecoveredApp, RecoveryReport};

use self::{
    models::{ContainerProjection, ContainerRecord, DockerErrorKind, DockerReadApi, ProbeStatus},
    ownership::{claimed_app_id, is_managed_candidate, validate_identity},
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
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppCatalog {
    apps: Arc<Vec<AppCatalogEntry>>,
    recovery_issues: Arc<HashMap<&'static str, usize>>,
}

impl AppCatalog {
    pub fn from_recovery(report: &RecoveryReport) -> Self {
        let mut issues = HashMap::new();
        for issue in &report.issues {
            *issues.entry(issue.code).or_insert(0) += 1;
        }
        Self {
            apps: Arc::new(
                report
                    .valid_apps
                    .iter()
                    .map(AppCatalogEntry::from)
                    .collect(),
            ),
            recovery_issues: Arc::new(issues),
        }
    }

    pub fn apps(&self) -> &[AppCatalogEntry] {
        &self.apps
    }

    pub fn get(&self, id: Uuid) -> Option<&AppCatalogEntry> {
        self.apps.iter().find(|app| app.id == id)
    }

    pub fn recovery_issues(&self) -> &HashMap<&'static str, usize> {
        &self.recovery_issues
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
    pub actual: Option<ContainerProjection>,
    pub drift_codes: Vec<DriftCode>,
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
                    .apps()
                    .iter()
                    .map(|app| AppObservation {
                        id: app.id,
                        slug: app.slug.clone(),
                        display_name: app.display_name.clone(),
                        active_release: active_release(app),
                        actual: None,
                        drift_codes: vec![DriftCode::DockerUnavailable],
                    })
                    .collect(),
                issues,
                complete: false,
            };
        }
        match self.api.list_managed_containers().await {
            Ok(containers) => associate(&self.catalog, containers, probe.status),
            Err(_) => ObservationSnapshot {
                docker_status: ProbeStatus::Unavailable,
                observed_at: OffsetDateTime::now_utc(),
                apps: self
                    .catalog
                    .apps()
                    .iter()
                    .map(|app| AppObservation {
                        id: app.id,
                        slug: app.slug.clone(),
                        display_name: app.display_name.clone(),
                        active_release: active_release(app),
                        actual: None,
                        drift_codes: vec![DriftCode::DockerUnavailable],
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
                if validate_identity(&container.labels, app).is_some() {
                    valid.push(container);
                } else {
                    invalid = true;
                }
            }
        }
        let candidate = match valid.len() {
            0 if invalid => return Err(OwnedContainerError::Invalid),
            0 => return Err(OwnedContainerError::Missing),
            1 => valid.into_iter().next().expect("one candidate"),
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
        validate_identity(&inspected.labels, app).ok_or(OwnedContainerError::Invalid)?;
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

fn associate(
    catalog: &AppCatalog,
    containers: Vec<ContainerRecord>,
    status: ProbeStatus,
) -> ObservationSnapshot {
    let mut issues = Vec::new();
    let mut observations = Vec::new();
    let mut considered = vec![false; containers.len()];
    for app in catalog.apps() {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();
        for (index, container) in containers.iter().enumerate() {
            if !is_managed_candidate(&container.labels) {
                continue;
            }
            if claimed_app_id(&container.labels) == Some(app.id) {
                considered[index] = true;
                if let Some(identity) = validate_identity(&container.labels, app) {
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
            if app.active_release_id.is_none() {
                codes.push(DriftCode::ActiveReleaseMissing);
            } else if app.active_release_id != Some(identity.release_id) {
                codes.push(DriftCode::ReleaseIdMismatch);
            }
            if app.active_image_ref.as_deref() != container.configured_image_ref.as_deref() {
                codes.push(DriftCode::ImageRefMismatch);
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
            actual,
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
    use std::collections::HashMap;

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
