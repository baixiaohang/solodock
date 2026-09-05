use super::*;
use bollard::query_parameters::{CommitContainerOptionsBuilder, RemoveImageOptionsBuilder};

#[tokio::test]
#[ignore = "requires an isolated classic Docker daemon"]
async fn classic_manual_image_cleanup_preserves_unselected_and_container_resources() {
    run().await;
}

#[tokio::test]
#[ignore = "requires an isolated containerd Docker daemon"]
async fn containerd_manual_image_cleanup_preserves_unselected_and_container_resources() {
    if std::env::var("SOLODOCK_EXPECT_CONTAINERD").as_deref() == Ok("1") {
        run().await;
    }
}

async fn run() {
    let endpoint = std::env::var("SOLODOCK_TEST_DOCKER_HOST").expect("isolated daemon required");
    let docker = Docker::connect_with_http(&endpoint, 5, API_DEFAULT_VERSION)
        .unwrap()
        .negotiate_version()
        .await
        .unwrap();
    let token = Uuid::new_v4().simple().to_string();
    let labels = HashMap::from([("com.solodock.test-run".to_owned(), token.clone())]);
    let mut containers = Vec::<String>::new();
    let mut images = Vec::<String>::new();
    let volume = format!("image-cleanup-volume-{token}");
    let network = format!("image-cleanup-network-{token}");
    let outcome=tokio::time::timeout(Duration::from_secs(240),AssertUnwindSafe(async {
        docker_cli(&endpoint,&["pull","registry:2"]).await;
        let registry=docker.create_container(Some(CreateContainerOptionsBuilder::default().name(&format!("image-cleanup-registry-{token}")).build()),ContainerCreateBody {
            image:Some("registry:2".into()),labels:Some(labels.clone()),
            exposed_ports:Some(vec!["5000/tcp".into()]),
            host_config:Some(HostConfig{port_bindings:Some(HashMap::from([("5000/tcp".into(),Some(vec![bollard::models::PortBinding{host_ip:Some("127.0.0.1".into()),host_port:Some("0".into())}]))])),tmpfs:Some(HashMap::from([("/var/lib/registry".into(),"rw".into())])),..Default::default()}),..Default::default()
        }).await.unwrap().id;
        containers.push(registry.clone());docker.start_container(&registry,None).await.unwrap();
        let raw=docker.inspect_container(&registry,None).await.unwrap();
        let port=raw.network_settings.unwrap().ports.unwrap()["5000/tcp"].as_ref().unwrap()[0].host_port.clone().unwrap();
        let base=docker.create_container(None::<bollard::query_parameters::CreateContainerOptions>,ContainerCreateBody{image:Some("alpine:3.20".into()),cmd:Some(vec!["sleep".into(),"300".into()]),labels:Some(labels.clone()),..Default::default()}).await.unwrap().id;
        containers.push(base.clone());
        let mut records=Vec::new();
        for index in 0..4 {
            let repository=format!("127.0.0.1:{port}/cleanup-{token}-{index}");
            let committed=docker.commit_container(CommitContainerOptionsBuilder::default().container(&base).repo(&repository).tag("test").changes(&format!("LABEL com.solodock.cleanup-fixture={token}-{index}")).build(),bollard::models::ContainerConfig::default()).await.unwrap();
            let committed_id=committed.id;images.push(committed_id.clone());
            // Registry readiness and push are bounded by the existing CLI deadline.
            docker_cli(&endpoint,&["push",&format!("{repository}:test")]).await;
            let observed=docker.inspect_image(&format!("{repository}:test")).await.unwrap();
            let id=observed.id.unwrap();
            if !images.contains(&id){images.push(id.clone());}
            let manifest=observed.repo_digests.as_ref().unwrap()[0].rsplit_once('@').unwrap().1.to_owned();
            records.push((id,manifest,observed.os.unwrap(),observed.architecture.unwrap(),observed.variant));
        }
        for(index,running)in [(2,true),(3,false)] {
            let id=docker.create_container(None::<bollard::query_parameters::CreateContainerOptions>,ContainerCreateBody{image:Some(records[index].0.clone()),cmd:Some(vec!["sleep".into(),"300".into()]),labels:Some(labels.clone()),..Default::default()}).await.unwrap().id;
            containers.push(id.clone());if running{docker.start_container(&id,None).await.unwrap();}
        }
        docker.create_volume(VolumeCreateRequest{name:Some(volume.clone()),labels:Some(labels.clone()),..Default::default()}).await.unwrap();
        docker.create_network(NetworkCreateRequest{name:network.clone(),labels:Some(labels.clone()),..Default::default()}).await.unwrap();
        let harness=MutationHarness::new(endpoint.clone(),std::env::temp_dir()).await;
        let create=harness.request("POST","/api/v1/apps",Some("image-cleanup-e2e-app-create"),Some(&json!({"slug":"image-cleanup","display_name":"Image cleanup","discovery_image_ref":"registry.example/app:stable","credential_ref":null,"auto_deploy_enabled":false,"poll_interval_seconds":300,"environment":{"public":[],"secrets":[]},"files":[],"ports":[],"volumes":[],"binds":[],"networks":[],"service_discovery_enabled":false,"health":{"policy":"running","stable_window_seconds":5}}))).await;
        assert_eq!(create.status(),StatusCode::CREATED);let created=MutationHarness::json(create).await;
        let app:Uuid=created["app"]["id"].as_str().unwrap().parse().unwrap();let metadata=harness.store.read_metadata(app).unwrap();
        for(id,manifest,os,arch,variant)in &records {
            harness.store.publish_v2_release(&metadata,Uuid::new_v4(),&solodock::registry::ResolvedImage{source_image_ref:"registry.example/app:stable".into(),logical_registry:"registry.example".into(),repository:"app".into(),source_tag:"stable".into(),source_descriptor_digest:manifest.clone(),index_digest:None,manifest_digest:manifest.clone(),runnable_image_ref:format!("registry.example/app@{manifest}"),platform:solodock::registry::Platform::canonical(os,arch,variant.as_deref()).unwrap(),local_image_id:id.clone()},solodock::app_store::releases::ReleaseTrigger::Manual,None).unwrap();
        }
        let preview=MutationHarness::json(harness.request("POST","/api/v1/system/storage-cleanup/preview",None,Some(&json!({}))).await).await;
        let apply=harness.request("POST","/api/v1/system/storage-cleanup/apply",Some("image-cleanup-e2e-artifacts"),Some(&json!({"confirmation_token":preview["confirmation_token"],"acknowledge_rollback_loss":true}))).await;
        assert_eq!(apply.status(),StatusCode::OK,"{}",MutationHarness::json(apply).await);
        let preview=MutationHarness::json(harness.request("POST","/api/v1/system/image-cleanup/preview",None,Some(&json!({}))).await).await;
        let eligible:Vec<_>=preview["candidates"].as_array().expect("complete image preview").iter().map(|item|item["image_id"].as_str().unwrap()).collect();
        assert_eq!(eligible.len(),2);assert!(eligible.contains(&records[0].0.as_str()));assert!(eligible.contains(&records[1].0.as_str()));
        let apply=harness.request("POST","/api/v1/system/image-cleanup/apply",Some("image-cleanup-e2e-selected"),Some(&json!({"confirmation_token":preview["confirmation_token"],"image_ids":[records[0].0],"acknowledge_image_removal":true}))).await;
        assert_eq!(apply.status(),StatusCode::OK);let result=MutationHarness::json(apply).await;
        assert_eq!(result["items"][0]["status"],"removed");
        assert!(matches!(docker.inspect_image(&records[0].0).await,Err(bollard::errors::Error::DockerResponseServerError{status_code:404,..})));
        for record in &records[1..]{assert!(docker.inspect_image(&record.0).await.is_ok());}
        for id in &containers{assert!(docker.inspect_container(id,None).await.is_ok());}
        assert!(docker.inspect_volume(&volume).await.is_ok());assert!(docker.inspect_network(&network,None).await.is_ok());
        harness.state.shutdown.cancel();harness.state.stream_tasks.close();harness.state.stream_tasks.wait().await;
    }).catch_unwind()).await;
    // Exact resources created above, even on assertion failure. No prune or
    // force-image deletion; never remove an image/container discovered by scan.
    for id in containers.iter().rev() {
        if docker.inspect_container(id, None).await.is_ok_and(|value| {
            value.id.as_deref() == Some(id)
                && value
                    .config
                    .and_then(|c| c.labels)
                    .is_some_and(|values| values.get("com.solodock.test-run") == Some(&token))
        }) {
            let _ = docker
                .remove_container(
                    id,
                    Some(
                        RemoveContainerOptionsBuilder::default()
                            .force(true)
                            .v(true)
                            .build(),
                    ),
                )
                .await;
        }
    }
    for id in images.iter().rev() {
        let _ = docker
            .remove_image(
                id,
                Some(
                    RemoveImageOptionsBuilder::default()
                        .force(false)
                        .noprune(true)
                        .build(),
                ),
                None,
            )
            .await;
    }
    if docker
        .inspect_volume(&volume)
        .await
        .is_ok_and(|v| v.labels.get("com.solodock.test-run") == Some(&token))
    {
        let _ = docker
            .remove_volume(
                &volume,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await;
    }
    if let Ok(value) = docker.inspect_network(&network, None).await
        && value
            .labels
            .is_some_and(|labels| labels.get("com.solodock.test-run") == Some(&token))
        && let Some(id) = value.id
    {
        let _ = docker.remove_network(&id).await;
    }
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("image cleanup E2E exceeded its bounded scenario deadline"),
    }
}
