use super::*;
use solodock::{app_store::atomic::AtomicWriter, retention};
use std::collections::HashSet;

async fn policy(h: &Harness, app: Uuid, enabled: bool, keep: u16) {
    let before = retention::load(&h.database, app).await.unwrap();
    let request =
        json!({"expected_revision":before.revision,"enabled":enabled,"keep_versions":keep});
    let key = Uuid::new_v4().to_string();
    let (status, response) = body(
        h.mutate(
            "PUT",
            &format!("/api/v1/apps/{app}/retention"),
            Some(&key),
            &request,
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let (status, replayed) = body(
        h.mutate(
            "PUT",
            &format!("/api/v1/apps/{app}/retention"),
            Some(&key),
            &request,
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
}
async fn fixture(count: usize) -> (Harness, Arc<Images>, Uuid, Vec<Uuid>) {
    let mut h = Harness::new().await;
    let (status, created) = body(
        h.create(Some("retention-create"), &draft("retention-test"))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let metadata = h.store.read_metadata(app).unwrap();
    let images = Arc::new(Images::default());
    let mut releases = Vec::new();
    for i in 0..count {
        let release = Uuid::new_v4();
        let image_id = format!("sha256:{i:064x}");
        h.store
            .publish_v2_release(
                &metadata,
                release,
                &solodock::registry::ResolvedImage {
                    source_image_ref: metadata.discovery_image_ref.clone().unwrap(),
                    logical_registry: "registry.example".into(),
                    repository: "app".into(),
                    source_tag: "stable".into(),
                    source_descriptor_digest: image_id.clone(),
                    index_digest: None,
                    manifest_digest: image_id.clone(),
                    runnable_image_ref: format!("registry.example/app@{image_id}"),
                    platform: solodock::registry::Platform::canonical("linux", "amd64", None)
                        .unwrap(),
                    local_image_id: image_id.clone(),
                },
                solodock::app_store::releases::ReleaseTrigger::Manual,
                None,
            )
            .unwrap();
        let mut observed = image(0);
        observed.image.id = image_id.clone();
        observed.image.repo_digests = vec![format!("registry.example/app@{image_id}")];
        images
            .state
            .lock()
            .unwrap()
            .images
            .insert(image_id, observed);
        success(&h, app, metadata.draft_revision.unwrap(), release, i as i64).await;
        releases.push(release);
    }
    AtomicWriter::switch_release_link(
        &h.store.app_directory(app),
        "active",
        *releases.last().unwrap(),
    )
    .unwrap();
    h.state.image_cleanup = images.clone();
    h.app = router(h.state.clone());
    (h, images, app, releases)
}
async fn success(h: &Harness, app: Uuid, revision: Uuid, release: Uuid, order: i64) {
    let at = solodock::db::format_time(
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(order),
    )
    .unwrap();
    sqlx::query("INSERT INTO deployments(id,app_id,trigger,requested_revision,candidate_release_id,status,phase,request_id,created_at,updated_at,completed_at) VALUES(?,?,'manual',?,?,'succeeded','terminal',?,?,?,?)").bind(Uuid::new_v4().to_string()).bind(app.to_string()).bind(revision.to_string()).bind(release.to_string()).bind(Uuid::new_v4().to_string()).bind(&at).bind(&at).bind(&at).execute(h.database.pool()).await.unwrap();
}
async fn candidates(h: &Harness) -> Vec<Uuid> {
    solodock::storage_cleanup::build_plan(&h.store, &h.database)
        .await
        .unwrap()
        .candidates
        .into_iter()
        .filter_map(|c| match c.artifact {
            solodock::app_store::cleanup::CleanupArtifact::Release { release_id, .. } => {
                Some(release_id)
            }
            _ => None,
        })
        .collect()
}
#[tokio::test]
async fn automatic_retention_default_and_five_successes_pending_then_commit() {
    let (h, images, app, r) = fixture(6).await;
    assert!(!retention::load(&h.database, app).await.unwrap().enabled);
    retention::run_pass(&h.state).await.unwrap();
    assert!(images.state.lock().unwrap().removes.is_empty());
    let revision = h.store.read_metadata(app).unwrap().draft_revision.unwrap();
    sqlx::query("DELETE FROM deployments WHERE candidate_release_id=?")
        .bind(r[5].to_string())
        .execute(h.database.pool())
        .await
        .unwrap();
    AtomicWriter::switch_release_link(&h.store.app_directory(app), "active", r[4]).unwrap();
    AtomicWriter::switch_release_link(&h.store.app_directory(app), "pending", r[5]).unwrap();
    policy(&h, app, true, 3).await;
    assert_eq!(
        candidates(&h).await.into_iter().collect::<HashSet<_>>(),
        HashSet::from([r[0], r[1]])
    );
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(app, r[2]).is_ok());
    assert!(h.store.load_v2_release(app, r[5]).is_ok());
    success(&h, app, revision, r[5], 10).await;
    AtomicWriter::switch_release_link(&h.store.app_directory(app), "active", r[5]).unwrap();
    AtomicWriter::remove_release_link(&h.store.app_directory(app), "pending").unwrap();
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(app, r[2]).is_err());
    for release in &r[3..] {
        assert!(h.store.load_v2_release(app, *release).is_ok());
    }
    assert_eq!(images.state.lock().unwrap().removes.len(), 3);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deployments")
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 6);
    assert_eq!(
        solodock::storage_cleanup::pending_operation_count(&h.store, &h.database)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        solodock::image_cleanup::pending_operation_count(&h.database)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn automatic_retention_failed_noop_duplicates_and_rollback_do_not_consume_slots() {
    let (h, _, app, r) = fixture(8).await;
    let revision = h.store.read_metadata(app).unwrap().draft_revision.unwrap();
    sqlx::query("UPDATE deployments SET status='failed' WHERE candidate_release_id=?")
        .bind(r[6].to_string())
        .execute(h.database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET status='no_op' WHERE candidate_release_id=?")
        .bind(r[5].to_string())
        .execute(h.database.pool())
        .await
        .unwrap();
    success(&h, app, revision, r[4], 90).await;
    success(&h, app, revision, r[4], 100).await;
    policy(&h, app, true, 3).await;
    let candidates = candidates(&h).await;
    for i in [7, 4, 3] {
        assert!(!candidates.contains(&r[i]));
    }
    for i in [0, 1, 2, 5, 6] {
        assert!(candidates.contains(&r[i]));
    }
    // A successful rollback is a new successful activation of an existing release.
    success(&h, app, revision, r[1], 101).await;
    AtomicWriter::switch_release_link(&h.store.app_directory(app), "active", r[1]).unwrap();
    let candidates = super::retention::candidates(&h).await;
    for i in [1, 4, 7] {
        assert!(!candidates.contains(&r[i]));
    }
}
#[tokio::test]
async fn automatic_retention_batches_restart_and_image_failure_compensate() {
    let (mut h, images, app, r) = fixture(106).await;
    policy(&h, app, true, 3).await;
    images.state.lock().unwrap().unavailable = true;
    retention::run_pass(&h.state).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases")
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 100);
    assert_eq!(
        retention::load(&h.database, app)
            .await
            .unwrap()
            .last_error_code
            .as_deref(),
        Some("IMAGE_CLEANUP_INVENTORY_INCOMPLETE")
    );
    assert!(images.state.lock().unwrap().removes.is_empty());
    h.restart_cleanup_router().await;
    images.state.lock().unwrap().unavailable = false;
    retention::run_pass(&h.state).await.unwrap();
    retention::run_pass(&h.state).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases")
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 103);
    assert_eq!(images.state.lock().unwrap().removes.len(), 103);
    for release in &r[103..] {
        assert!(h.store.load_v2_release(app, *release).is_ok());
    }
}
#[tokio::test]
async fn automatic_retention_disable_after_detach_resumes_only_irreversible_effects() {
    let (mut h, images, app, r) = fixture(5).await;
    policy(&h, app, true, 3).await;
    sqlx::query("CREATE TRIGGER retention_fail_progress BEFORE UPDATE ON storage_cleanup_items WHEN NEW.status='detached' BEGIN SELECT RAISE(ABORT,'test'); END").execute(h.database.pool()).await.unwrap();
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(app, r[0]).is_err());
    assert!(h.store.load_v2_release(app, r[1]).is_ok());
    policy(&h, app, false, 3).await;
    sqlx::query("DROP TRIGGER retention_fail_progress")
        .execute(h.database.pool())
        .await
        .unwrap();
    h.restart_cleanup_router().await;
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(app, r[1]).is_ok());
    assert_eq!(
        solodock::storage_cleanup::pending_operation_count(&h.store, &h.database)
            .await
            .unwrap(),
        0
    );
    assert!(images.state.lock().unwrap().removes.is_empty());
}
#[tokio::test]
async fn automatic_retention_image_interruption_and_disabled_policy_finish_exact_proof() {
    let (mut h, images, app, _) = fixture(5).await;
    policy(&h, app, true, 3).await;
    images.state.lock().unwrap().fault = Some("lost");
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(images.state.lock().unwrap().removes.len(), 1);
    h.restart_cleanup_router().await;
    policy(&h, app, false, 3).await;
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(images.state.lock().unwrap().removes.len(), 1);
    assert_eq!(
        solodock::image_cleanup::pending_operation_count(&h.database)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn automatic_retention_busy_app_and_corrupt_authorization_delete_nothing() {
    let (h, images, app, r) = fixture(5).await;
    policy(&h, app, true, 3).await;
    let guard = h
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .try_app(app)
        .unwrap();
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(
        retention::load(&h.database, app)
            .await
            .unwrap()
            .last_error_code
            .as_deref(),
        Some("APP_BUSY")
    );
    assert!(h.store.load_v2_release(app, r[0]).is_ok());
    drop(guard);
    sqlx::query("CREATE TRIGGER retention_fail_progress BEFORE UPDATE ON storage_cleanup_items WHEN NEW.status='detached' BEGIN SELECT RAISE(ABORT,'test'); END").execute(h.database.pool()).await.unwrap();
    retention::run_pass(&h.state).await.unwrap();
    sqlx::query("DROP TRIGGER retention_fail_progress")
        .execute(h.database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE automatic_cleanup_authorizations SET app_id=?")
        .bind(Uuid::new_v4().to_string())
        .execute(h.database.pool())
        .await
        .unwrap();
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(app, r[1]).is_ok());
    assert!(images.state.lock().unwrap().removes.is_empty());
}

#[tokio::test]
async fn automatic_retention_stopped_and_running_containers_veto_images() {
    for running in [false, true] {
        let (h, images, app, _) = fixture(5).await;
        policy(&h, app, true, 3).await;
        images.state.lock().unwrap().containers = vec![container(running, false)];
        retention::run_pass(&h.state).await.unwrap();
        {
            let observed = images.state.lock().unwrap();
            assert!(!observed.removes.contains(&digest(0)));
            assert_eq!(observed.removes.len(), 1);
        }
        assert_eq!(
            retention::load(&h.database, app)
                .await
                .unwrap()
                .last_status
                .as_deref(),
            Some("partially_retained")
        );
    }
}
#[tokio::test]
async fn automatic_retention_disabled_app_releases_veto_shared_images_and_are_not_selected() {
    let (h, images, app, r) = fixture(5).await;
    let mut input = draft("other-app");
    input["slug"] = json!("other-retention");
    let (status, created) = body(h.create(Some("other-retention-create"), &input).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let other: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let metadata = h.store.read_metadata(other).unwrap();
    let source = h.store.load_v2_release(app, r[0]).unwrap();
    let other_release = Uuid::new_v4();
    h.store
        .publish_v2_release(
            &metadata,
            other_release,
            &solodock::registry::ResolvedImage {
                source_image_ref: metadata.discovery_image_ref.clone().unwrap(),
                logical_registry: "registry.example".into(),
                repository: "app".into(),
                source_tag: "stable".into(),
                source_descriptor_digest: source.manifest_digest.clone(),
                index_digest: None,
                manifest_digest: source.manifest_digest.clone(),
                runnable_image_ref: source.runnable_image_ref.clone(),
                platform: solodock::registry::Platform::canonical("linux", "amd64", None).unwrap(),
                local_image_id: source.local_image_id.clone(),
            },
            solodock::app_store::releases::ReleaseTrigger::Manual,
            None,
        )
        .unwrap();
    // No active link or successful history: this would be a global manual candidate.
    policy(&h, app, true, 3).await;
    retention::run_pass(&h.state).await.unwrap();
    assert!(h.store.load_v2_release(other, other_release).is_ok());
    assert!(
        !images
            .state
            .lock()
            .unwrap()
            .removes
            .contains(&source.local_image_id)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cleaned_releases WHERE app_id=?")
        .bind(other.to_string())
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn automatic_retention_policy_revision_and_authentication_are_required() {
    let (h, _, app, _) = fixture(5).await;
    let before = h.store.read_metadata(app).unwrap();
    policy(&h, app, true, 3).await;
    let (status, response) = body(
        h.mutate(
            "PUT",
            &format!("/api/v1/apps/{app}/retention"),
            Some("stale-retention-policy-change"),
            &json!({"expected_revision":Uuid::nil(),"enabled":false,"keep_versions":1}),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert!(response.contains("RETENTION_REVISION_STALE"));
    assert!(retention::load(&h.database, app).await.unwrap().enabled);
    assert_eq!(
        h.store.read_metadata(app).unwrap().draft_revision,
        before.draft_revision
    );
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/apps/{app}/retention"))
                .header(header::HOST, "localhost:8080")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_client_error());
}

#[tokio::test]
async fn automatic_retention_missing_multiarch_child_blocks_images_and_retries() {
    let (h, images, app, _) = fixture(5).await;
    policy(&h, app, true, 3).await;
    let descriptor = |index| solodock::registry::ManifestDescriptor {
        digest: Some(digest(index)),
        os: Some("linux".into()),
        architecture: Some("amd64".into()),
        variant: None,
    };
    {
        let mut state = images.state.lock().unwrap();
        let mut parent = image(12);
        parent.is_index = true;
        parent.image.manifest_descriptor = Some(descriptor(12));
        state.images.insert(digest(12), parent);
        let mut c = container(true, false);
        c.image_id = Some(digest(12));
        c.manifest_descriptor = Some(descriptor(13));
        state.containers = vec![c];
    }
    retention::run_pass(&h.state).await.unwrap();
    assert!(images.state.lock().unwrap().removes.is_empty());
    assert_eq!(
        retention::load(&h.database, app)
            .await
            .unwrap()
            .last_error_code
            .as_deref(),
        Some("IMAGE_CLEANUP_INVENTORY_INCOMPLETE")
    );
    let mut child = image(13);
    child.image.manifest_descriptor = Some(descriptor(13));
    images
        .state
        .lock()
        .unwrap()
        .images
        .insert(digest(13), child);
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(images.state.lock().unwrap().removes.len(), 2);
    assert!(
        images
            .state
            .lock()
            .unwrap()
            .images
            .contains_key(&digest(12))
    );
}

#[tokio::test]
async fn automatic_retention_policy_response_failure_rolls_back_authorization_atomically() {
    let (mut h, images, app, _) = fixture(5).await;
    let route = format!("/api/v1/apps/{app}/retention");
    let request = json!({"expected_revision":Uuid::nil(),"enabled":true,"keep_versions":3});
    let key = "retention-atomic-failure-test";
    sqlx::query("CREATE TRIGGER retention_response_failure BEFORE UPDATE ON idempotency_records WHEN NEW.status='succeeded' AND NEW.route LIKE '%/retention' BEGIN SELECT RAISE(ABORT,'test response failure'); END").execute(h.database.pool()).await.unwrap();
    let (status, result) = body(h.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{result}");
    assert!(!retention::load(&h.database, app).await.unwrap().enabled);
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action='retention_policy_update'",
    )
    .fetch_one(h.database.pool())
    .await
    .unwrap();
    assert_eq!(audit, 0);
    retention::run_pass(&h.state).await.unwrap();
    assert!(images.state.lock().unwrap().removes.is_empty());
    sqlx::query("DROP TRIGGER retention_response_failure")
        .execute(h.database.pool())
        .await
        .unwrap();
    h.restart_cleanup_router().await;
    let guard = h
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .try_app(app)
        .unwrap();
    let (status, result) = body(h.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert!(result.contains("APP_BUSY"));
    assert!(!retention::load(&h.database, app).await.unwrap().enabled);
    drop(guard);
}
#[tokio::test]
async fn automatic_retention_committed_policy_replays_after_restart_busy_and_newer_policy() {
    let (mut h, _, app, _) = fixture(5).await;
    let route = format!("/api/v1/apps/{app}/retention");
    let request = json!({"expected_revision":Uuid::nil(),"enabled":true,"keep_versions":3});
    let key = "retention-committed-response-lost";
    let (status, original) = body(h.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{original}");
    let original: Value = serde_json::from_str(&original).unwrap();
    // Discard the response as if the transport failed, then restart.
    h.restart_cleanup_router().await;
    let guard = h
        .state
        .m3
        .as_ref()
        .unwrap()
        .coordinator
        .try_app(app)
        .unwrap();
    let (status, result) = body(h.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["revision"],
        original["revision"]
    );
    drop(guard);
    policy(&h, app, false, 10).await;
    let latest = retention::load(&h.database, app).await.unwrap();
    h.restart_cleanup_router().await;
    let (status, result) = body(h.mutate("PUT", &route, Some(key), &request).await).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["revision"],
        original["revision"]
    );
    assert_eq!(retention::load(&h.database, app).await.unwrap(), latest);
}
#[tokio::test]
async fn automatic_retention_persistent_image_conflicts_do_not_starve_later_batches() {
    let (mut h, images, app, _) = fixture(106).await;
    policy(&h, app, true, 3).await;
    images.state.lock().unwrap().always_retained =
        (0..100).map(|i| format!("sha256:{i:064x}")).collect();
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(images.state.lock().unwrap().removes.len(), 100);
    assert!(
        images
            .state
            .lock()
            .unwrap()
            .images
            .contains_key(&format!("sha256:{:064x}", 100))
    );
    h.restart_cleanup_router().await;
    retention::run_pass(&h.state).await.unwrap();
    {
        let state = images.state.lock().unwrap();
        assert_eq!(state.removes.len(), 200);
        for i in 100..103 {
            assert!(!state.images.contains_key(&format!("sha256:{i:064x}")));
        }
        for i in 0..100 {
            assert!(state.images.contains_key(&format!("sha256:{i:064x}")));
        }
        for i in 103..106 {
            assert!(state.images.contains_key(&format!("sha256:{i:064x}")));
        }
    }
    retention::run_pass(&h.state).await.unwrap();
    assert_eq!(images.state.lock().unwrap().removes.len(), 300);
}

#[tokio::test]
async fn automatic_retention_upgrade_preserves_pending_and_terminal_manual_image_recovery() {
    for interrupted in [false, true] {
        let (mut h, images, _, _) = super::fixture().await;
        let preview = super::preview(&h).await;
        let request = super::request(&preview);
        if interrupted {
            images.state.lock().unwrap().fault = Some("lost");
        }
        let (status, result) = body(
            h.image_mutate("POST", APPLY, Some("upgrade-manual-proof"), &request)
                .await,
        )
        .await;
        assert_eq!(
            status,
            if interrupted {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::OK
            },
            "{result}"
        );
        let before: Vec<(String, String)> = sqlx::query_as(
            "SELECT operation_id,status FROM image_cleanup_items ORDER BY operation_id,ordinal",
        )
        .fetch_all(h.database.pool())
        .await
        .unwrap();
        // Model an installed pre-retention database with its real manual proofs.
        let legacy = include_str!("../../migrations/202609060001_image_cleanup.sql");
        let tables = legacy[legacy
            .find("CREATE TABLE image_cleanup_operations")
            .unwrap()..]
            .replace(
                "image_cleanup_operations",
                "image_cleanup_operations_legacy",
            )
            .replace("image_cleanup_items", "image_cleanup_items_legacy");
        let mut tx = h.database.pool().begin().await.unwrap();
        sqlx::raw_sql(&tables).execute(&mut *tx).await.unwrap();
        sqlx::raw_sql("INSERT INTO image_cleanup_operations_legacy SELECT * FROM image_cleanup_operations; INSERT INTO image_cleanup_items_legacy SELECT * FROM image_cleanup_items; DROP TABLE image_cleanup_items; DROP TABLE image_cleanup_operations; ALTER TABLE image_cleanup_operations_legacy RENAME TO image_cleanup_operations; ALTER TABLE image_cleanup_items_legacy RENAME TO image_cleanup_items; DROP TABLE app_retention; DROP TABLE automatic_cleanup_authorizations; DELETE FROM _sqlx_migrations WHERE version=202609090001;").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        h.restart_cleanup_router().await;
        solodock::image_cleanup::validate_operations(&h.database)
            .await
            .unwrap();
        let after: Vec<(String, String)> = sqlx::query_as(
            "SELECT operation_id,status FROM image_cleanup_items ORDER BY operation_id,ordinal",
        )
        .fetch_all(h.database.pool())
        .await
        .unwrap();
        assert_eq!(before, after);
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(h.database.pool())
                .await
                .unwrap()
                .is_empty()
        );
        let (status, result) = body(
            h.image_mutate("POST", APPLY, Some("upgrade-manual-proof"), &request)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(images.state.lock().unwrap().removes, vec![digest(0)]);
    }
}
