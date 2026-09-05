use super::*;
use solodock::docker::{
    image_cleanup::{CleanupImage, ExactImageId, ImageCleanup, RemoveImageResult},
    models::{DockerErrorKind, ImageRecord},
};
use std::sync::Mutex;

const PREVIEW: &str = "/api/v1/system/image-cleanup/preview";
const APPLY: &str = "/api/v1/system/image-cleanup/apply";

impl Harness {
    async fn image_mutate(
        &self,
        method: &str,
        route: &str,
        key: Option<&str>,
        value: &Value,
    ) -> axum::response::Response {
        let key = key.map(|key| format!("image-cleanup-test-{key}"));
        self.mutate(method, route, key.as_deref(), value).await
    }
}
fn digest(index: usize) -> String {
    format!("sha256:{}", format!("{index:x}").repeat(64))
}
#[derive(Default)]
struct Images {
    state: Mutex<ImageState>,
}
#[derive(Default)]
struct ImageState {
    images: HashMap<String, CleanupImage>,
    containers: Vec<ContainerRecord>,
    removes: Vec<String>,
    fault: Option<&'static str>,
    inspect_failure: bool,
    unavailable: bool,
}
#[async_trait]
impl ImageCleanup for Images {
    async fn all_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        let state = self.state.lock().unwrap();
        if state.unavailable {
            return Err(DockerError::new(DockerErrorKind::Unavailable));
        }
        Ok(state.containers.clone())
    }
    async fn inspect(&self, id: &ExactImageId) -> Result<Option<CleanupImage>, DockerError> {
        let mut state = self.state.lock().unwrap();
        if state.inspect_failure {
            state.inspect_failure = false;
            return Err(DockerError::new(DockerErrorKind::Unavailable));
        }
        Ok(state.images.get(id.as_str()).cloned())
    }
    async fn remove(&self, id: &ExactImageId) -> Result<RemoveImageResult, DockerError> {
        let mut state = self.state.lock().unwrap();
        state.removes.push(id.as_str().into());
        let fault = state.fault.take();
        if fault == Some("before") {
            return Err(DockerError::new(DockerErrorKind::Unavailable));
        }
        if fault == Some("conflict") {
            return Ok(RemoveImageResult::Retained);
        }
        state.images.remove(id.as_str());
        if fault == Some("lost") {
            return Err(DockerError::new(DockerErrorKind::Unavailable));
        }
        if fault == Some("inspect") {
            state.inspect_failure = true;
        }
        Ok(RemoveImageResult::Accepted)
    }
}
fn image(index: usize) -> CleanupImage {
    CleanupImage {
        image: ImageRecord {
            id: digest(index),
            manifest_descriptor: None,
            repo_digests: vec![format!("registry.example/app@{}", digest(index))],
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
        reported_size_bytes: 1024,
        repo_tags: vec![],
    }
}
async fn fixture() -> (Harness, Arc<Images>, Uuid, Uuid) {
    let mut h = Harness::new().await;
    let (app, revision, _, _) = storage_cleanup_fixture(&h).await;
    let images = Arc::new(Images::default());
    images.state.lock().unwrap().images = (0..16).map(|i| (digest(i), image(i))).collect();
    h.state.image_cleanup = images.clone();
    h.app = router(h.state.clone());
    let request = cleanup_request(&h).await;
    let (status, result) = body(
        h.image_mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("prepare-images"),
            &request,
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(
        images.state.lock().unwrap().removes.is_empty(),
        "artifact cleanup must not mutate Docker"
    );
    (h, images, app, revision)
}
async fn preview(h: &Harness) -> Value {
    let (status, value) = body(h.image_mutate("POST", PREVIEW, None, &json!({})).await).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    serde_json::from_str(&value).unwrap()
}
fn request(preview: &Value) -> Value {
    json!({"confirmation_token":preview["confirmation_token"],"image_ids":[digest(0)],"acknowledge_image_removal":true})
}
fn container(running: bool, managed: bool) -> ContainerRecord {
    ContainerRecord {
        id: "c".repeat(64),
        name: "canary".into(),
        labels: if managed {
            HashMap::from([(MANAGED_LABEL.into(), "true".into())])
        } else {
            HashMap::new()
        },
        status: if running {
            ContainerStatus::Running
        } else {
            ContainerStatus::Exited
        },
        health: HealthStatus::None,
        exit_code: None,
        restart_count: Some(0),
        started_at: None,
        finished_at: None,
        configured_image_ref: Some("operator/canary".into()),
        image_id: Some(digest(0)),
        manifest_descriptor: None,
        ports: vec![],
        mounts: vec![],
        networks: vec![],
    }
}

#[tokio::test]
async fn only_cleaned_images_and_all_four_container_classes_are_considered() {
    let (h, images, _, _) = fixture().await;
    let p = preview(&h).await;
    assert_eq!(p["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(p["candidates"][0]["image_id"], digest(0));
    for running in [true, false] {
        for managed in [true, false] {
            images.state.lock().unwrap().containers = vec![container(running, managed)];
            assert!(
                preview(&h).await["candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }
    images.state.lock().unwrap().containers.clear();
    let response = body(
        h.image_mutate("POST", APPLY, Some("image-confirmed"), &request(&p))
            .await,
    )
    .await;
    assert_eq!(response.0, StatusCode::OK, "{}", response.1);
    let replay = body(
        h.image_mutate("POST", APPLY, Some("image-confirmed"), &request(&p))
            .await,
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert!(
        serde_json::from_str::<Value>(&replay.1).unwrap()["idempotency_replayed"]
            .as_bool()
            .unwrap()
    );
    let state = images.state.lock().unwrap();
    assert_eq!(state.removes, vec![digest(0)]);
    assert_eq!(state.images.len(), 15);
    assert!(
        state.images.contains_key(&digest(15)),
        "unselected operator image survives"
    );
}

#[tokio::test]
async fn fresh_refs_busy_and_invalid_selection_have_zero_consume_or_remove() {
    let (h, images, app, _) = fixture().await;
    let p = preview(&h).await;
    for (index, mut invalid) in [
        json!({"image_ids":[]}),
        json!({"image_ids":[digest(15)]}),
        json!({"image_ids":["operator/tag"]}),
        json!({"acknowledge_image_removal":false}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut input = request(&p);
        for (k, v) in invalid.as_object_mut().unwrap().iter() {
            input[k] = v.clone();
        }
        let status = h
            .image_mutate("POST", APPLY, Some(&format!("bad-{index}")), &input)
            .await
            .status();
        assert!(status.is_client_error());
    }
    let m3 = h.state.m3.as_ref().unwrap();
    let guard = m3.coordinator.try_app(app).unwrap();
    let busy = body(
        h.image_mutate("POST", APPLY, Some("busy-app"), &request(&p))
            .await,
    )
    .await;
    assert!(busy.1.contains("APP_BUSY"));
    drop(guard);
    let guard = m3.coordinator.try_compose().unwrap();
    let busy = body(
        h.image_mutate("POST", APPLY, Some("busy-compose"), &request(&p))
            .await,
    )
    .await;
    assert!(busy.1.contains("APP_BUSY"));
    drop(guard);
    images
        .state
        .lock()
        .unwrap()
        .containers
        .push(container(false, false));
    let stale = body(
        h.image_mutate("POST", APPLY, Some("stale-container"), &request(&p))
            .await,
    )
    .await;
    assert!(stale.1.contains("CLEANUP_PREVIEW_STALE"), "{}", stale.1);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM image_cleanup_previews WHERE consumed_at IS NOT NULL",
    )
    .fetch_one(h.database.pool())
    .await
    .unwrap();
    assert_eq!(count, 0);
    assert!(images.state.lock().unwrap().removes.is_empty());
}

#[tokio::test]
async fn incomplete_docker_identity_inventory_and_artifact_proof_fail_closed() {
    let (h, images, _, _) = fixture().await;
    let p = preview(&h).await;
    for case in 0..7 {
        {
            let mut state = images.state.lock().unwrap();
            state.unavailable = false;
            state.containers.clear();
            state.images.insert(digest(0), image(0));
            match case {
                0 => state.unavailable = true,
                1 => {
                    let mut c = container(false, false);
                    c.image_id = None;
                    state.containers.push(c);
                }
                2 => state.images.get_mut(&digest(0)).unwrap().image.architecture = "arm64".into(),
                3 => state
                    .images
                    .get_mut(&digest(0))
                    .unwrap()
                    .image
                    .repo_digests
                    .clear(),
                4 => state.images.get_mut(&digest(0)).unwrap().image.id = digest(15),
                5 => {
                    let mut c = container(false, false);
                    c.manifest_descriptor = Some(solodock::registry::ManifestDescriptor {
                        digest: Some(digest(15)),
                        os: Some("linux".into()),
                        architecture: Some("amd64".into()),
                        variant: None,
                    });
                    state.containers.push(c);
                }
                _ => state.containers = vec![container(false, false); 4097],
            }
        }
        let (status, value) = body(h.image_mutate("POST", PREVIEW, None, &json!({})).await).await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        assert!(!value.contains("confirmation_token"));
        assert!(
            h.image_mutate(
                "POST",
                APPLY,
                Some(&format!("incomplete-{case}")),
                &request(&p)
            )
            .await
            .status()
            .is_client_error()
        );
    }
    images.state.lock().unwrap().containers.clear();
    images
        .state
        .lock()
        .unwrap()
        .images
        .insert(digest(0), image(0));
    sqlx::query("UPDATE cleaned_releases SET manifest_digest=?")
        .bind(digest(15))
        .execute(h.database.pool())
        .await
        .unwrap();
    assert_eq!(
        h.image_mutate("POST", PREVIEW, None, &json!({}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(images.state.lock().unwrap().removes.is_empty());
}

#[tokio::test]
async fn unrelated_container_inspect_conflicts_fail_closed_and_proven_aliases_work() {
    let (h, images, _, _) = fixture().await;
    let p = preview(&h).await;
    let descriptor = |index| solodock::registry::ManifestDescriptor {
        digest: Some(digest(index)),
        os: Some("linux".into()),
        architecture: Some("amd64".into()),
        variant: None,
    };
    for with_descriptor in [false, true] {
        {
            let mut state = images.state.lock().unwrap();
            let mut c = container(false, false);
            c.image_id = Some(digest(12));
            state.containers = vec![c];
            let mut conflicting = image(13);
            if with_descriptor {
                conflicting.image.manifest_descriptor = Some(descriptor(14));
            }
            state.images.insert(digest(12), conflicting);
        }
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM image_cleanup_previews")
            .fetch_one(h.database.pool())
            .await
            .unwrap();
        let (status, value) = body(h.image_mutate("POST", PREVIEW, None, &json!({})).await).await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        assert!(!value.contains("confirmation_token"));
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM image_cleanup_previews")
            .fetch_one(h.database.pool())
            .await
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            h.image_mutate(
                "POST",
                APPLY,
                Some(&format!("container-id-conflict-{with_descriptor}")),
                &request(&p)
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        let consumed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM image_cleanup_previews WHERE consumed_at IS NOT NULL",
        )
        .fetch_one(h.database.pool())
        .await
        .unwrap();
        assert_eq!(consumed, 0);
        assert!(images.state.lock().unwrap().removes.is_empty());
    }
    // A requested manifest may resolve to a config ID only with explicit
    // descriptor/RepoDigest evidence. The opposite containerd projection is valid too.
    for case in 0..4 {
        {
            let mut state = images.state.lock().unwrap();
            let mut c = container(false, false);
            c.image_id = Some(digest(12));
            let mut observed = image(13);
            if case == 0 {
                observed.image.repo_digests = vec![format!("registry.example/app@{}", digest(12))];
            } else {
                observed.image.manifest_descriptor =
                    Some(descriptor(if case == 3 { 13 } else { 12 }));
            }
            if case == 2 {
                c.manifest_descriptor = Some(descriptor(12));
            }
            state.containers = vec![c];
            state.images.insert(digest(12), observed);
        }
        assert_eq!(preview(&h).await["candidates"][0]["image_id"], digest(0));
    }
}

#[tokio::test]
async fn multiple_selected_images_resume_real_progress_and_retain_new_references() {
    for new_reference in [false, true] {
        let (mut h, images, app, _) = fixture().await;
        publish_image_release(&h, app, 14);
        publish_image_release(&h, app, 15);
        let artifacts = cleanup_request(&h).await;
        assert_eq!(
            h.image_mutate(
                "POST",
                "/api/v1/system/storage-cleanup/apply",
                Some("multi-artifacts"),
                &artifacts
            )
            .await
            .status(),
            StatusCode::OK
        );
        let p = preview(&h).await;
        assert_eq!(p["candidates"].as_array().unwrap().len(), 3);
        let mut input = request(&p);
        input["image_ids"] = json!([digest(0), digest(14)]);
        let trigger = if new_reference {
            "CREATE TRIGGER reject_second BEFORE UPDATE ON image_cleanup_items WHEN NEW.ordinal=1 AND NEW.status='started' BEGIN SELECT RAISE(ABORT,'second start fault'); END"
        } else {
            "CREATE TRIGGER reject_second BEFORE UPDATE ON image_cleanup_items WHEN NEW.ordinal=1 AND NEW.status='removed' BEGIN SELECT RAISE(ABORT,'second progress fault'); END"
        };
        sqlx::query(trigger)
            .execute(h.database.pool())
            .await
            .unwrap();
        assert_eq!(
            h.image_mutate("POST", APPLY, Some("multi-resume"), &input)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let states: Vec<String> =
            sqlx::query_scalar("SELECT status FROM image_cleanup_items ORDER BY ordinal")
                .fetch_all(h.database.pool())
                .await
                .unwrap();
        assert_eq!(
            states,
            vec!["removed", if new_reference { "planned" } else { "started" }]
        );
        assert_eq!(
            images.state.lock().unwrap().removes,
            if new_reference {
                vec![digest(0)]
            } else {
                vec![digest(0), digest(14)]
            }
        );
        sqlx::query("DROP TRIGGER reject_second")
            .execute(h.database.pool())
            .await
            .unwrap();
        if new_reference {
            let mut c = container(false, false);
            c.image_id = Some(digest(14));
            images.state.lock().unwrap().containers.push(c);
        }
        h.restart_cleanup_router().await;
        solodock::image_cleanup::validate_operations(&h.database)
            .await
            .unwrap();
        let (status, result) = body(
            h.image_mutate("POST", APPLY, Some("multi-resume"), &input)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result["status"],
            if new_reference {
                "completed_with_failures"
            } else {
                "completed"
            }
        );
        assert_eq!(
            result["items"],
            json!([
                {"image_id":digest(0),"status":"removed"},
                {"image_id":digest(14),"status":if new_reference {"retained"} else {"removed"}}
            ])
        );
        assert_eq!(
            h.image_mutate("POST", APPLY, Some("multi-resume"), &input)
                .await
                .status(),
            StatusCode::OK
        );
        let state = images.state.lock().unwrap();
        assert_eq!(
            state.removes,
            if new_reference {
                vec![digest(0)]
            } else {
                vec![digest(0), digest(14)]
            }
        );
        assert!(
            state.images.contains_key(&digest(15)),
            "unselected eligible image survives"
        );
        assert_eq!(state.images.contains_key(&digest(14)), new_reference);
    }
}

#[tokio::test]
async fn actual_remove_and_sqlite_failures_resume_without_losing_identity() {
    for fault in ["before", "lost", "inspect", "progress", "response", "audit"] {
        let (mut h, images, _, _) = fixture().await;
        let p = preview(&h).await;
        let input = request(&p);
        if ["before", "lost", "inspect"].contains(&fault) {
            images.state.lock().unwrap().fault = Some(fault);
        }
        let trigger = match fault {
            "progress" => Some(
                "CREATE TRIGGER reject_image BEFORE UPDATE ON image_cleanup_items WHEN NEW.status='removed' BEGIN SELECT RAISE(ABORT,'image progress fault'); END",
            ),
            "response" => Some(
                "CREATE TRIGGER reject_image BEFORE UPDATE ON idempotency_records WHEN NEW.route='/api/v1/system/image-cleanup/apply' AND NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'image response fault'); END",
            ),
            "audit" => Some(
                "CREATE TRIGGER reject_image BEFORE INSERT ON audit_events WHEN NEW.action='image_cleanup_apply' BEGIN SELECT RAISE(ABORT,'image audit fault'); END",
            ),
            _ => None,
        };
        if let Some(sql) = trigger {
            sqlx::query(sql).execute(h.database.pool()).await.unwrap();
        }
        assert_eq!(
            h.image_mutate("POST", APPLY, Some("recover-image"), &input)
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{fault}"
        );
        if fault == "audit" {
            assert!(images.state.lock().unwrap().removes.is_empty());
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM image_cleanup_operations")
                .fetch_one(h.database.pool())
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
        if trigger.is_some() {
            sqlx::query("DROP TRIGGER reject_image")
                .execute(h.database.pool())
                .await
                .unwrap();
        }
        h.restart_cleanup_router().await;
        let (status, result) = body(
            h.image_mutate("POST", APPLY, Some("recover-image"), &input)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{fault}: {result}");
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["items"][0]["status"],
            "removed"
        );
        assert!(!result.contains(p["confirmation_token"].as_str().unwrap()));
        assert_eq!(
            images.state.lock().unwrap().removes.len(),
            if fault == "before" { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn published_retry_busy_session_and_new_reference_stay_resumable() {
    let (h, images, app, _) = fixture().await;
    let input = request(&preview(&h).await);
    images.state.lock().unwrap().fault = Some("before");
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("published"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        preview(&h).await["candidates"]
            .as_array()
            .unwrap()
            .is_empty(),
        "another operation cannot claim an interrupted reservation"
    );
    sqlx::query("UPDATE idempotency_records SET updated_at='2020-01-01T00:00:00Z' WHERE route=?")
        .bind(APPLY)
        .execute(h.database.pool())
        .await
        .unwrap();
    h.idempotency
        .gc_terminal_records(&std::collections::HashSet::new())
        .await
        .unwrap();
    let proofs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM idempotency_records WHERE route=? AND status='interrupted'",
    )
    .bind(APPLY)
    .fetch_one(h.database.pool())
    .await
    .unwrap();
    assert_eq!(proofs, 1);
    let m3 = h.state.m3.as_ref().unwrap();
    let guard = m3.coordinator.try_app(app).unwrap();
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("published"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    drop(guard);
    let session: String = sqlx::query_scalar("SELECT session_id FROM image_cleanup_previews")
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE image_cleanup_previews SET session_id='other'")
        .execute(h.database.pool())
        .await
        .unwrap();
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("published"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    sqlx::query("UPDATE image_cleanup_previews SET session_id=?")
        .bind(session)
        .execute(h.database.pool())
        .await
        .unwrap();
    images
        .state
        .lock()
        .unwrap()
        .containers
        .push(container(false, false));
    let (status, result) = body(
        h.image_mutate("POST", APPLY, Some("published"), &input)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["status"],
        "completed_with_failures"
    );
    assert!(images.state.lock().unwrap().images.contains_key(&digest(0)));
    assert_eq!(images.state.lock().unwrap().removes.len(), 1);
}

#[tokio::test]
async fn nonforce_conflict_is_confirmed_retained() {
    let (h, images, _, _) = fixture().await;
    let input = request(&preview(&h).await);
    images.state.lock().unwrap().fault = Some("conflict");
    let (status, result) = body(
        h.image_mutate("POST", APPLY, Some("conflict"), &input)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["items"][0]["status"],
        "retained"
    );
    assert!(images.state.lock().unwrap().images.contains_key(&digest(0)));
}

fn publish_image_release(h: &Harness, app: Uuid, index: usize) -> Uuid {
    let metadata = h.store.read_metadata(app).unwrap();
    let id = Uuid::new_v4();
    let digest = digest(index);
    h.store
        .publish_v2_release(
            &metadata,
            id,
            &solodock::registry::ResolvedImage {
                source_image_ref: metadata.discovery_image_ref.clone().unwrap(),
                logical_registry: "registry.example".into(),
                repository: "app".into(),
                source_tag: "stable".into(),
                source_descriptor_digest: digest.clone(),
                index_digest: None,
                manifest_digest: digest.clone(),
                runnable_image_ref: format!("registry.example/app@{digest}"),
                platform: solodock::registry::Platform::canonical("linux", "amd64", None).unwrap(),
                local_image_id: digest,
            },
            solodock::app_store::releases::ReleaseTrigger::Manual,
            None,
        )
        .unwrap();
    id
}

#[tokio::test]
async fn sqlite_snapshot_restores_unknown_image_progress_and_exact_retry() {
    let (mut h, images, _, _) = fixture().await;
    let input = request(&preview(&h).await);
    images.state.lock().unwrap().fault = Some("lost");
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("snapshot-unknown"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let snapshot = h._root.path().join("image-cleanup-snapshot.sqlite3");
    sqlx::query("VACUUM INTO ?")
        .bind(snapshot.to_str().unwrap())
        .execute(h.database.pool())
        .await
        .unwrap();
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
    let restored = Database::open(&snapshot).await.unwrap();
    solodock::image_cleanup::validate_operations(&restored)
        .await
        .unwrap();
    assert_eq!(
        solodock::image_cleanup::pending_operation_count(&restored)
            .await
            .unwrap(),
        1
    );
    let old = h.state.m3.as_ref().unwrap();
    let idempotency =
        IdempotencyService::initialize(restored.clone(), &h.state.state_directory).unwrap();
    idempotency.interrupt_pending().await.unwrap();
    h.state.m3 = Some(Arc::new(M3Services {
        store: old.store.clone(),
        database: restored.clone(),
        allowed_bind_roots: old.allowed_bind_roots.clone(),
        runtime_directory: old.runtime_directory.clone(),
        idempotency,
        coordinator: AppMutationCoordinator::new(old.runtime_directory.clone()).unwrap(),
        compose: old.compose.clone(),
        compose_capability: old.compose_capability.clone(),
        projection_degraded: old.projection_degraded.clone(),
        reconcile_notify: old.reconcile_notify.clone(),
        publication_lock: old.publication_lock.clone(),
    }));
    h.app = router(h.state.clone());
    assert_eq!(
        images.state.lock().unwrap().removes.len(),
        1,
        "restore validation never removes images"
    );
    let (status, result) = body(
        h.image_mutate("POST", APPLY, Some("snapshot-unknown"), &input)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["items"][0]["status"],
        "removed"
    );
    assert_eq!(images.state.lock().unwrap().removes.len(), 1);
    solodock::image_cleanup::validate_operations(&restored)
        .await
        .unwrap();
    assert_eq!(
        solodock::image_cleanup::pending_operation_count(&restored)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ordinary_retained_release_and_new_app_guard_protect_before_any_effect() {
    let (h, images, app, _) = fixture().await;
    let p = preview(&h).await;
    let retained = publish_image_release(&h, app, 0);
    assert_ne!(
        h.store.read_release_link(app, "active").unwrap(),
        Some(retained)
    );
    assert!(
        preview(&h).await["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let stale = body(
        h.image_mutate("POST", APPLY, Some("retained-release-race"), &request(&p))
            .await,
    )
    .await;
    assert!(stale.1.contains("CLEANUP_PREVIEW_STALE"));
    let mut input = draft("new-app-secret");
    input["slug"] = json!("new-image-guard");
    let (status, value) = body(h.create(Some("new-image-guard-app"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    let other: Uuid = serde_json::from_str::<Value>(&value).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let m3 = h.state.m3.as_ref().unwrap();
    let guard = m3.coordinator.try_app(other).unwrap();
    let busy = body(
        h.image_mutate("POST", APPLY, Some("all-app-guard-race"), &request(&p))
            .await,
    )
    .await;
    assert!(busy.1.contains("APP_BUSY"), "{}", busy.1);
    drop(guard);
    assert!(images.state.lock().unwrap().removes.is_empty());
}

#[tokio::test]
async fn application_tombstone_releases_protect_images_and_dangling_recovery_ref_fails_closed() {
    let (h, images, _, _) = fixture().await;
    let p = preview(&h).await;
    let mut input = draft("tombstone-image-secret");
    input["slug"] = json!("tombstone-image");
    let (status, value) = body(h.create(Some("tombstone-image-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED);
    let app: Uuid = serde_json::from_str::<Value>(&value).unwrap()["app"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let release = publish_image_release(&h, app, 0);
    solodock::app_store::atomic::AtomicWriter::switch_release_link(
        &h.store.app_directory(app),
        "active",
        release,
    )
    .unwrap();
    let route = format!("/api/v1/apps/{app}");
    let operation = match h
        .idempotency
        .claim(&route, "image-tombstone-delete", &[9; 32], Uuid::new_v4())
        .await
        .unwrap()
    {
        solodock::mutation::ClaimResult::New(id) => id,
        _ => unreachable!(),
    };
    h.store.tombstone(app, operation).unwrap();
    assert!(
        preview(&h).await["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let source = h
        .store
        .tombstone_path(app, operation)
        .join("releases")
        .join(release.to_string());
    fs::rename(source, h._root.path().join("displaced-release")).unwrap();
    assert_eq!(
        h.image_mutate("POST", PREVIEW, None, &json!({}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("tombstone-stale-image"), &request(&p))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(images.state.lock().unwrap().removes.is_empty());
}

#[tokio::test]
async fn unselected_eligible_image_survives_and_corrupt_durable_plan_blocks_recovery() {
    let (h, images, app, _) = fixture().await;
    publish_image_release(&h, app, 14);
    let artifacts = cleanup_request(&h).await;
    assert_eq!(
        h.image_mutate(
            "POST",
            "/api/v1/system/storage-cleanup/apply",
            Some("second-artifact-plan"),
            &artifacts
        )
        .await
        .status(),
        StatusCode::OK
    );
    let p = preview(&h).await;
    assert_eq!(p["candidates"].as_array().unwrap().len(), 2);
    images.state.lock().unwrap().fault = Some("before");
    let input = request(&p);
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("durable-plan-corruption"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    solodock::image_cleanup::validate_operations(&h.database)
        .await
        .unwrap();
    sqlx::query("UPDATE image_cleanup_items SET image_id=?")
        .bind(digest(15))
        .execute(h.database.pool())
        .await
        .unwrap();
    assert!(
        solodock::image_cleanup::validate_operations(&h.database)
            .await
            .is_err()
    );
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("durable-plan-corruption"), &input)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    sqlx::query("UPDATE image_cleanup_items SET image_id=?")
        .bind(digest(0))
        .execute(h.database.pool())
        .await
        .unwrap();
    assert_eq!(
        h.image_mutate("POST", APPLY, Some("durable-plan-corruption"), &input)
            .await
            .status(),
        StatusCode::OK
    );
    solodock::image_cleanup::validate_operations(&h.database)
        .await
        .unwrap();
    assert!(
        images
            .state
            .lock()
            .unwrap()
            .images
            .contains_key(&digest(14))
    );
    assert!(
        images
            .state
            .lock()
            .unwrap()
            .removes
            .iter()
            .all(|id| id == &digest(0))
    );
}
