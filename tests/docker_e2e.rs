#![cfg(feature = "docker-e2e")]

use std::collections::HashMap;

use bollard::{
    API_DEFAULT_VERSION, Docker,
    models::ContainerCreateBody,
    query_parameters::{CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder},
};
use futures_util::StreamExt;
use solodock::docker::{
    client::BollardReadClient,
    models::{DockerReadApi, LogRequest, LogStreamKind},
    ownership::*,
};
use uuid::Uuid;

struct Outcome {
    listed: bool,
    inspected_run_token: Option<String>,
    log_stream: LogStreamKind,
    log_contains_canary: bool,
    memory_observed: bool,
    event_container_id: String,
    event_run_token: Option<String>,
}

#[tokio::test]
#[ignore = "requires a dedicated Docker-in-Docker daemon"]
async fn observes_owned_container_on_isolated_daemon() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST")
        .expect("SOLODOCK_TEST_DOCKER_HOST must point to the isolated daemon");
    assert!(endpoint.starts_with("tcp://127.0.0.1:") || endpoint.starts_with("tcp://localhost:"));
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let run_token = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let release_id = Uuid::new_v4();
    let name = format!("solodock-test-{run_token}");
    let labels = HashMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (SCHEMA_LABEL.into(), "1".into()),
        (APP_ID_LABEL.into(), app_id.to_string()),
        (RELEASE_ID_LABEL.into(), release_id.to_string()),
        (
            PROJECT_LABEL.into(),
            format!("solodock-test-{}", app_id.simple()),
        ),
        (SERVICE_LABEL.into(), "app".into()),
        (ONEOFF_LABEL.into(), "False".into()),
        ("com.solodock.test-run".into(), run_token.to_string()),
    ]);
    let created = docker
        .create_container(
            Some(CreateContainerOptionsBuilder::default().name(&name).build()),
            ContainerCreateBody {
                image: Some("alpine:3.20".into()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo solodock-e2e-log; sleep 20".into(),
                ]),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let container_id = created.id;

    let result: Result<Outcome, String> = async {
        let adapter = BollardReadClient::for_test_http(endpoint);
        let mut events = adapter.events().await.map_err(|error| error.to_string())?;
        let event_task = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
                .await
                .map_err(|_| "event timeout".to_string())?
                .ok_or_else(|| "event stream ended".to_string())?
                .map_err(|error| error.to_string())
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        docker
            .start_container(&container_id, None)
            .await
            .map_err(|_| "failed to start test container".to_string())?;

        let listed = adapter
            .list_managed_containers()
            .await
            .map_err(|error| error.to_string())?;
        let inspected = adapter
            .inspect_container(&container_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut logs = adapter
            .logs(
                &container_id,
                LogRequest {
                    tail: 20,
                    since_seconds: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let log = tokio::time::timeout(std::time::Duration::from_secs(10), logs.next())
            .await
            .map_err(|_| "log timeout".to_string())?
            .ok_or_else(|| "log stream ended".to_string())?
            .map_err(|error| error.to_string())?;
        let mut stats = adapter
            .stats(&container_id)
            .await
            .map_err(|error| error.to_string())?;
        let sample = tokio::time::timeout(std::time::Duration::from_secs(10), stats.next())
            .await
            .map_err(|_| "stats timeout".to_string())?
            .ok_or_else(|| "stats stream ended".to_string())?
            .map_err(|error| error.to_string())?;
        let event = event_task
            .await
            .map_err(|_| "event task failed".to_string())??;
        Ok(Outcome {
            listed: listed.iter().any(|container| container.id == container_id),
            inspected_run_token: inspected.labels.get("com.solodock.test-run").cloned(),
            log_stream: log.stream,
            log_contains_canary: log
                .bytes
                .windows(b"solodock-e2e-log".len())
                .any(|value| value == b"solodock-e2e-log"),
            memory_observed: sample.memory_usage.is_some(),
            event_container_id: event.container_id,
            event_run_token: event.labels.get("com.solodock.test-run").cloned(),
        })
    }
    .await;

    let cleanup_target = docker.inspect_container(&container_id, None).await.unwrap();
    assert_eq!(
        cleanup_target
            .config
            .and_then(|config| config.labels)
            .and_then(|labels| labels.get("com.solodock.test-run").cloned()),
        Some(run_token.to_string())
    );
    docker
        .remove_container(
            &container_id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .unwrap();

    let outcome = result.unwrap();
    assert!(outcome.listed);
    assert_eq!(outcome.inspected_run_token, Some(run_token.to_string()));
    assert_eq!(outcome.log_stream, LogStreamKind::Stdout);
    assert!(outcome.log_contains_canary);
    assert!(outcome.memory_observed);
    assert_eq!(outcome.event_container_id, container_id);
    assert_eq!(outcome.event_run_token, Some(run_token.to_string()));
}
