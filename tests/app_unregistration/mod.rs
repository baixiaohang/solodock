use super::*;
use solodock::docker::image_cleanup::{
    CleanupImage, ExactImageId, ImageCleanup, RemoveImageResult,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct EmptyImages(AtomicUsize);
#[async_trait]
impl ImageCleanup for EmptyImages {
    async fn all_containers(&self) -> Result<Vec<ContainerRecord>, DockerError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    async fn inspect(&self, _: &ExactImageId) -> Result<Option<CleanupImage>, DockerError> {
        panic!("unexpected image inspect")
    }
    async fn remove(&self, _: &ExactImageId) -> Result<RemoveImageResult, DockerError> {
        panic!("preview must never remove images")
    }
}

async fn attention(h: &Harness, app: Uuid, revision: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let now = solodock::db::format_time(time::OffsetDateTime::now_utc()).unwrap();
    sqlx::query("INSERT INTO deployments (id,app_id,trigger,requested_revision,candidate_release_id,status,phase,error_code,request_id,created_at,updated_at,completed_at) VALUES (?,?,'manual',?,?,'needs_attention','terminal','CANDIDATE_INVALID',?,?,?,?)")
        .bind(id.to_string()).bind(app.to_string()).bind(revision.to_string()).bind(Uuid::new_v4().to_string())
        .bind(Uuid::new_v4().to_string()).bind(&now).bind(&now).bind(&now).execute(h.database.pool()).await.unwrap();
    id
}

async fn fixture() -> (Harness, Uuid, Value, Arc<EmptyImages>) {
    let mut h = Harness::new().await;
    let images = Arc::new(EmptyImages::default());
    h.state.image_cleanup = images.clone();
    h.app = router(h.state.clone());
    let (status, created) = body(
        h.create(Some("unregistration-create"), &draft("private-canary"))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: Value = serde_json::from_str(&created).unwrap();
    let app: Uuid = created["app"]["id"].as_str().unwrap().parse().unwrap();
    let revision: Uuid = created["app"]["config_revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    attention(&h, app, revision).await;
    let route = format!("/api/v1/apps/{app}/deletion-preview");
    let (status, p) = body(
        h.mutate("POST", &route, None, &json!({"remove_container":false}))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    let p: Value = serde_json::from_str(&p).unwrap();
    let request = json!({"confirmation_token":p["confirmation_token"],"slug":"example","expected_revision":revision,"remove_container":false});
    (h, app, request, images)
}

async fn previews(h: &Harness, expected: StatusCode, code: Option<&str>) {
    for kind in ["storage", "image"] {
        let (status, value) = body(
            h.mutate(
                "POST",
                &format!("/api/v1/system/{kind}-cleanup/preview"),
                None,
                &json!({}),
            )
            .await,
        )
        .await;
        assert_eq!(status, expected, "{kind}: {value}");
        let value: Value = serde_json::from_str(&value).unwrap();
        if let Some(code) = code {
            assert_eq!(value["code"], code);
            assert!(
                value["request_id"]
                    .as_str()
                    .is_some_and(|v| v.parse::<Uuid>().is_ok())
            );
            assert!(value.get("confirmation_token").is_none());
            assert!(!value.to_string().contains("private-canary"));
        }
    }
}

async fn receipt_count(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM app_unregistrations")
        .fetch_one(h.database.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn deletion_retires_only_its_history_and_survives_replay_gc_and_restart() {
    let (mut h, app, request, images) = fixture().await;
    previews(
        &h,
        StatusCode::CONFLICT,
        Some("CLEANUP_RECOVERY_REFERENCE_MISSING"),
    )
    .await;
    assert_eq!(images.0.load(Ordering::SeqCst), 0);
    let route = format!("/api/v1/apps/{app}");
    for _ in 0..2 {
        let (status, response) = body(
            h.mutate("DELETE", &route, Some("unregistration-delete"), &request)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(receipt_count(&h).await, 1);
    }
    assert!(h.store.tombstones().unwrap().is_empty());
    previews(&h, StatusCode::OK, None).await;
    sqlx::query("UPDATE idempotency_records SET updated_at='2000-01-01T00:00:00Z'")
        .execute(h.database.pool())
        .await
        .unwrap();
    h.idempotency
        .gc_terminal_records(&Default::default())
        .await
        .unwrap();
    let proof_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE route=?")
            .bind(&route)
            .fetch_one(h.database.pool())
            .await
            .unwrap();
    assert_eq!(proof_count, 0);
    h.restart_cleanup_router().await;
    previews(&h, StatusCode::OK, None).await;
    let status: String = sqlx::query_scalar("SELECT status FROM deployments WHERE app_id=?")
        .bind(app.to_string())
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(status, "needs_attention");
    attention(&h, Uuid::new_v4(), Uuid::new_v4()).await;
    previews(
        &h,
        StatusCode::CONFLICT,
        Some("CLEANUP_RECOVERY_REFERENCE_MISSING"),
    )
    .await;
}

#[tokio::test]
async fn receipt_write_failure_retains_proof_until_replay_startup_or_background_retry() {
    for recovery in ["replay", "startup", "background"] {
        let (h, app, request, _) = fixture().await;
        sqlx::query("CREATE TRIGGER reject_unregistration BEFORE INSERT ON app_unregistrations BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END").execute(h.database.pool()).await.unwrap();
        let route = format!("/api/v1/apps/{app}");
        let (status, value) = body(
            h.mutate("DELETE", &route, Some("unregistration-retry"), &request)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(receipt_count(&h).await, 0);
        assert_eq!(h.store.tombstones().unwrap().len(), 1);
        previews(
            &h,
            StatusCode::CONFLICT,
            Some("CLEANUP_RECOVERY_REFERENCE_MISSING"),
        )
        .await;
        sqlx::query("DROP TRIGGER reject_unregistration")
            .execute(h.database.pool())
            .await
            .unwrap();
        match recovery {
            "replay" => {
                assert_eq!(
                    h.mutate("DELETE", &route, Some("unregistration-retry"), &request)
                        .await
                        .status(),
                    StatusCode::OK
                );
            }
            "startup" => {
                h.idempotency
                    .finalize_succeeded_tombstones(&h.store)
                    .await
                    .unwrap();
            }
            "background" => {
                let cancel = tokio_util::sync::CancellationToken::new();
                let worker = solodock::api::mutations::start_projection_reconciler(
                    h.state.clone(),
                    cancel.clone(),
                );
                h.state.m3.as_ref().unwrap().reconcile_notify.notify_one();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !h.store.tombstones().unwrap().is_empty() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                cancel.cancel();
                worker.await.unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(receipt_count(&h).await, 1);
        assert!(h.store.tombstones().unwrap().is_empty());
        previews(&h, StatusCode::OK, None).await;
    }
}

#[tokio::test]
async fn absent_app_without_proof_and_conflicting_or_unfinished_retirement_remain_blocked() {
    for damage in [
        "no-proof",
        "reappeared",
        "inflight",
        "unaudited-repair",
        "wrong-operation",
        "malformed-time",
    ] {
        let (h, app, request, _) = fixture().await;
        let route = format!("/api/v1/apps/{app}");
        assert_eq!(
            h.mutate("DELETE", &route, Some("unregistration-negative"), &request)
                .await
                .status(),
            StatusCode::OK
        );
        match damage {
            "no-proof" => {
                sqlx::query("DELETE FROM app_unregistrations")
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "reappeared" => {
                fs::create_dir(h.store.app_directory(app)).unwrap();
                fs::set_permissions(
                    h.store.app_directory(app),
                    fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            "inflight" => {
                sqlx::query(
                    "UPDATE deployments SET status='running',phase='pulling',completed_at=NULL",
                )
                .execute(h.database.pool())
                .await
                .unwrap();
            }
            "unaudited-repair" => {
                sqlx::query("UPDATE app_unregistrations SET source='operator_repair'")
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "wrong-operation" => {
                sqlx::query("UPDATE app_unregistrations SET operation_id='invalid'")
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "malformed-time" => {
                sqlx::query("UPDATE app_unregistrations SET completed_at='invalid'")
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        previews(
            &h,
            StatusCode::CONFLICT,
            Some(if damage == "no-proof" {
                "CLEANUP_RECOVERY_REFERENCE_MISSING"
            } else {
                "CLEANUP_INVENTORY_INCOMPLETE"
            }),
        )
        .await;
    }
}

#[tokio::test]
#[cfg(feature = "docker-e2e")]
async fn rename_sync_response_and_finalizer_failures_do_not_release_recovery_early() {
    use solodock::app_store::cleanup::CleanupFault;
    for fault in [
        Some(CleanupFault::AppTombstoneRenamed),
        Some(CleanupFault::AppTombstoneSync),
        Some(CleanupFault::AppTombstoneFinalize),
        Some(CleanupFault::AppTombstoneRemoved),
        None,
    ] {
        let (h, app, request, _) = fixture().await;
        if let Some(fault) = fault {
            h.store.fail_cleanup_once(fault);
        } else {
            sqlx::query("CREATE TRIGGER reject_delete_response BEFORE UPDATE ON idempotency_records WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected response failure'); END").execute(h.database.pool()).await.unwrap();
        }
        let route = format!("/api/v1/apps/{app}");
        let first = h
            .mutate("DELETE", &route, Some("unregistration-fault"), &request)
            .await;
        assert_eq!(
            first.status(),
            if fault.is_none() || fault == Some(CleanupFault::AppTombstoneRenamed) {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::OK
            }
        );
        assert_eq!(
            h.store.tombstones().unwrap().len(),
            usize::from(fault != Some(CleanupFault::AppTombstoneRemoved))
        );
        assert_eq!(
            receipt_count(&h).await,
            i64::from(matches!(
                fault,
                Some(CleanupFault::AppTombstoneFinalize | CleanupFault::AppTombstoneRemoved)
            ))
        );
        if fault == Some(CleanupFault::AppTombstoneRemoved) {
            sqlx::query(
                "UPDATE idempotency_records SET updated_at='2000-01-01T00:00:00Z' WHERE route=?",
            )
            .bind(&route)
            .execute(h.database.pool())
            .await
            .unwrap();
            h.idempotency
                .gc_terminal_records(&Default::default())
                .await
                .unwrap();
            let retained: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE route=?")
                    .bind(&route)
                    .fetch_one(h.database.pool())
                    .await
                    .unwrap();
            assert_eq!(
                retained, 1,
                "pending final sync keeps the replay proof even without a visible marker"
            );
        }
        previews(
            &h,
            StatusCode::CONFLICT,
            Some("CLEANUP_RECOVERY_REFERENCE_MISSING"),
        )
        .await;
        if fault.is_none() {
            sqlx::query("DROP TRIGGER reject_delete_response")
                .execute(h.database.pool())
                .await
                .unwrap();
        }
        assert_eq!(
            h.mutate("DELETE", &route, Some("unregistration-fault"), &request)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(receipt_count(&h).await, 1);
        assert!(h.store.tombstones().unwrap().is_empty());
        previews(&h, StatusCode::OK, None).await;
    }
}

#[tokio::test]
async fn old_completed_proof_is_backfilled_but_absence_or_wrong_proof_is_not() {
    for proof in [
        "valid",
        "wrong-app",
        "wrong-route",
        "not-unregistered",
        "failed",
        "missing",
        "conflicting-operation",
    ] {
        let (h, app, request, _) = fixture().await;
        let route = format!("/api/v1/apps/{app}");
        assert_eq!(
            h.mutate("DELETE", &route, Some("unregistration-backfill"), &request)
                .await
                .status(),
            StatusCode::OK
        );
        sqlx::query("DELETE FROM app_unregistrations")
            .execute(h.database.pool())
            .await
            .unwrap();
        match proof {
            "wrong-app" => {
                sqlx::query("UPDATE idempotency_records SET response_body=? WHERE route=?")
                    .bind(json!({"app_id":Uuid::new_v4(),"unregistered":true}).to_string())
                    .bind(&route)
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "wrong-route" => {
                sqlx::query("UPDATE idempotency_records SET route=route || '/start' WHERE route=?")
                    .bind(&route)
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "not-unregistered" => {
                sqlx::query("UPDATE idempotency_records SET response_body='{}' WHERE route=?")
                    .bind(&route)
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "failed" => {
                sqlx::query("UPDATE idempotency_records SET status='failed',response_status=409 WHERE route=?").bind(&route).execute(h.database.pool()).await.unwrap();
            }
            "missing" => {
                sqlx::query("DELETE FROM idempotency_records WHERE route=?")
                    .bind(&route)
                    .execute(h.database.pool())
                    .await
                    .unwrap();
            }
            "conflicting-operation" => {
                sqlx::query("INSERT INTO app_unregistrations (app_id,operation_id,source,completed_at) VALUES (?,?,'deletion','2026-01-01T00:00:00Z')").bind(app.to_string()).bind(Uuid::new_v4().to_string()).execute(h.database.pool()).await.unwrap();
            }
            _ => {}
        }
        let result = h
            .idempotency
            .preserve_completed_unregistrations(&h.store)
            .await;
        if proof == "conflicting-operation" {
            assert!(result.is_err());
            continue;
        }
        result.unwrap();
        assert_eq!(receipt_count(&h).await, i64::from(proof == "valid"));
        if proof == "valid" {
            previews(&h, StatusCode::OK, None).await;
        } else {
            previews(
                &h,
                StatusCode::CONFLICT,
                Some("CLEANUP_RECOVERY_REFERENCE_MISSING"),
            )
            .await;
        }
    }
}

#[tokio::test]
async fn terminal_interrupted_deployment_is_history_after_successful_unregistration() {
    let (h, app, request, _) = fixture().await;
    // DeploymentLedger startup recovery seals queued/running work this way.
    sqlx::query(
        "UPDATE deployments SET status='interrupted',phase='terminal',completed_at=updated_at",
    )
    .execute(h.database.pool())
    .await
    .unwrap();
    let route = format!("/api/v1/apps/{app}");
    assert_eq!(
        h.mutate(
            "DELETE",
            &route,
            Some("unregistration-interrupted"),
            &request
        )
        .await
        .status(),
        StatusCode::OK
    );
    previews(&h, StatusCode::OK, None).await;
    let status: String = sqlx::query_scalar("SELECT status FROM deployments WHERE app_id=?")
        .bind(app.to_string())
        .fetch_one(h.database.pool())
        .await
        .unwrap();
    assert_eq!(status, "interrupted");
}
