use std::{
    error::Error,
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
};

use solodock::{
    AppState,
    api::{deployments::M4Services, mutations::M3Services},
    app_store::AppStore,
    auth::AuthService,
    compose::{ComposeCapability, ComposeRunner, FixedComposeRunner},
    config::Config,
    db::Database,
    deploy::{
        DeploymentEngine, DeploymentLedger, DeploymentScheduler, FixedImagePuller, HealthVerifier,
    },
    docker::{
        AppCatalog, DockerObserver,
        client::BollardReadClient,
        events::DockerEventHub,
        logs::{EmptySecretProvider, SecretRedactor},
        models::DockerReadApi,
        probe::DockerSupervisor,
        stats::StatsHub,
    },
    mutation::{AppMutationCoordinator, IdempotencyService},
    registry::{CredentialStore, PollCoordinator, PollStateStore, RegistryResolver},
    security::permissions::ensure_private_directory,
    webhook::{WebhookRateLimiter, WebhookServices, WebhookStore},
};
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|value| value == "validate-restore")
    {
        if arguments.len() != 4 {
            return Err("usage: solodock validate-restore STATE_DIRECTORY CONFIG_FILE".into());
        }
        validate_restore(
            std::path::Path::new(&arguments[2]),
            std::path::Path::new(&arguments[3]),
        )?;
        return Ok(());
    }
    let config = Config::load()?;
    solodock::telemetry::initialize();

    ensure_private_directory(&config.state_directory)?;
    ensure_private_directory(&config.runtime_directory)?;
    solodock::compose::cleanup_temporary_directories(&config.runtime_directory)?;
    let database = Database::open(&config.database_path()).await?;
    let idempotency = IdempotencyService::initialize(database.clone(), &config.state_directory)?;
    idempotency.interrupt_pending().await?;
    let app_store = AppStore::initialize_managed(
        config.apps_directory(),
        idempotency.integrity_key(),
        config.allowed_bind_roots.clone(),
    )?;
    let webhook_store = WebhookStore::new(app_store.clone(), idempotency.integrity_key());
    idempotency
        .cleanup_webhook_operation_temps(&webhook_store)
        .await?;
    let recovery = app_store.scan()?;
    database.refresh_app_index(&recovery).await?;
    for issue in &recovery.issues {
        warn!(
            issue_code = issue.code,
            app_id = issue.app_id.map(|id| id.to_string()),
            "filesystem recovery issue"
        );
    }

    let auth = AuthService::new(database.clone(), config.bootstrap_token_path());
    if auth.prepare_bootstrap().await? {
        info!(
            bootstrap_token_path = %config.bootstrap_token_path().display(),
            "administrator bootstrap required"
        );
    }

    let catalog = AppCatalog::from_recovery(&recovery);
    let docker_api = Arc::new(BollardReadClient::production());
    if let Ok(probe) = docker_api.probe().await {
        config.validate_docker_root(probe.docker_root_directory.as_deref())?;
    }
    let shutdown = CancellationToken::new();
    let stream_tasks = TaskTracker::new();
    let supervisor = DockerSupervisor::new();
    let supervisor_task = supervisor.start(docker_api.clone(), shutdown.clone());
    let event_hub = DockerEventHub::new();
    let event_task = event_hub.start(docker_api.clone(), catalog.clone(), shutdown.clone());
    let observer = DockerObserver::new(docker_api.clone(), catalog, supervisor);
    let stats = StatsHub::new(docker_api, shutdown.clone(), stream_tasks.clone());
    let redactor = SecretRedactor::new(&EmptySecretProvider);
    let mut known_secrets = Vec::new();
    for app in &recovery.valid_apps {
        for revision in [
            app.draft_revision,
            app.active_config_revision,
            app.pending_config_revision,
        ]
        .into_iter()
        .flatten()
        {
            let loaded = solodock::app_store::config_revision::load_verified(
                &app_store.app_directory(app.app_id),
                revision,
                &idempotency.integrity_key(),
            )?;
            known_secrets.extend(loaded.known_secrets());
        }
    }
    let credential_store = CredentialStore::initialize(
        config.state_directory.join("registry-credentials"),
        idempotency.integrity_key(),
    )?;
    credential_store.startup_cleanup()?;
    idempotency
        .finalize_succeeded_credential_tombstones(&credential_store)
        .await?;
    for credential in credential_store.list()? {
        let loaded = credential_store.load(credential.id)?;
        known_secrets.push(loaded.secret.expose().as_bytes().to_vec());
    }
    idempotency
        .finalize_succeeded_webhook_revisions(&webhook_store)
        .await?;
    // Cold start has no prior redactor set to retain. Refuse to listen unless
    // every persisted webhook revision can be conservatively inventoried.
    known_secrets.extend(webhook_store.all_secret_bytes()?);
    redactor.replace(known_secrets);
    let coordinator = AppMutationCoordinator::new(config.runtime_directory.clone())?;
    let compose: Arc<dyn ComposeRunner> = Arc::new(FixedComposeRunner::new(
        shutdown.clone(),
        stream_tasks.clone(),
        redactor.clone(),
    ));
    let compose_capability = ComposeCapability::default();
    compose_capability.probe(compose.as_ref()).await;
    let compose_capability_task = compose_capability.start(compose.clone(), shutdown.clone());
    let recovery_degraded = !recovery.issues.is_empty();
    let m3 = Arc::new(M3Services {
        store: app_store,
        database: database.clone(),
        allowed_bind_roots: config.allowed_bind_roots.clone(),
        runtime_directory: config.runtime_directory.clone(),
        idempotency,
        coordinator,
        compose,
        compose_capability,
        projection_degraded: Arc::new(AtomicBool::new(recovery_degraded)),
        reconcile_notify: Arc::new(tokio::sync::Notify::new()),
        publication_lock: Arc::new(tokio::sync::Mutex::new(())),
    });
    let ledger = DeploymentLedger::new(database.clone());
    ledger.interrupt_nonterminal().await?;
    let puller = Arc::new(FixedImagePuller::new(
        config.state_directory.clone(),
        config.runtime_directory.clone(),
        observer.api(),
        shutdown.clone(),
        stream_tasks.clone(),
    )?);
    puller.cleanup_stale()?;
    let resolver = RegistryResolver::production()?;
    let engine = DeploymentEngine {
        store: m3.store.clone(),
        credentials: credential_store.clone(),
        resolver,
        ledger: ledger.clone(),
        puller,
        compose: m3.compose.clone(),
        docker: observer.api(),
        health: HealthVerifier::new(observer.api(), shutdown.clone()),
        shutdown: shutdown.clone(),
        tasks: stream_tasks.clone(),
        #[cfg(feature = "docker-e2e")]
        test_effect_gate: None,
        #[cfg(feature = "docker-e2e")]
        test_candidate_gate: None,
    };
    let scheduler = DeploymentScheduler::new(engine.clone());
    let poll_store = PollStateStore::new(database.clone());
    // Generic projection pruning is safe only when recovery proved a complete
    // inventory. Exact succeeded app tombstones below remain authoritative
    // even while an unrelated app or webhook substate is degraded.
    if recovery.issues.is_empty() {
        poll_store
            .retain_apps(
                &recovery
                    .valid_apps
                    .iter()
                    .map(|app| app.app_id)
                    .collect::<Vec<_>>(),
            )
            .await?;
    }
    for (app_id, _) in m3.idempotency.succeeded_app_tombstones(&m3.store).await? {
        poll_store.remove_app_operational(app_id).await?;
    }
    m3.idempotency
        .finalize_succeeded_tombstones(&m3.store)
        .await?;
    let poller = PollCoordinator::new(poll_store, shutdown.clone(), stream_tasks.clone());
    let m4 = Arc::new(M4Services {
        credentials: credential_store,
        ledger,
        engine,
        scheduler,
        poller,
    });
    let webhook_origin = config.webhook_public_origin.clone().unwrap_or_default();
    let webhook_authority = if webhook_origin.is_empty() {
        String::new()
    } else {
        solodock::config::origin_authority(&webhook_origin)?
    };
    let webhooks = Some(Arc::new(WebhookServices {
        origin: webhook_origin,
        authority: webhook_authority,
        store: webhook_store.clone(),
        poll_states: m4.poller.store.clone(),
        database: database.clone(),
        notify: m4.poller.notify.clone(),
        limiter: WebhookRateLimiter::default(),
        permits: Arc::new(tokio::sync::Semaphore::new(16)),
    }));

    let listener = TcpListener::bind(config.listen_address).await?;
    info!(listen_address = %config.listen_address, "SoloDock API listening");
    let state = AppState {
        auth,
        public_origin: config.public_origin,
        observer,
        events: event_hub,
        stats,
        stream_gate: solodock::api::streams::StreamGate::default(),
        redactor,
        state_directory: config.state_directory.clone(),
        shutdown: shutdown.clone(),
        stream_tasks: stream_tasks.clone(),
        m3: Some(m3),
        m4: Some(m4),
        webhooks,
    };
    let poller_task = {
        let m3 = state.m3.as_ref().expect("M3 services configured").clone();
        let m4 = state.m4.as_ref().expect("M4 services configured").clone();
        m4.poller.start(state.clone(), m3, m4.clone())
    };
    let projection_task =
        solodock::api::mutations::start_projection_reconciler(state.clone(), shutdown.clone());
    {
        let server_shutdown = shutdown.clone();
        let server = axum::serve(
            listener,
            solodock::router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            server_shutdown.cancelled().await;
        });
        let server = async move { server.await };
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => result?,
            () = shutdown_signal() => {
                shutdown.cancel();
                match tokio::time::timeout(std::time::Duration::from_secs(10), &mut server).await {
                    Ok(result) => result?,
                    Err(_) => warn!("graceful HTTP shutdown deadline exceeded"),
                }
            }
        }
    }
    shutdown.cancel();
    stream_tasks.close();
    if tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let _ = supervisor_task.await;
        let _ = event_task.await;
        let _ = compose_capability_task.await;
        let _ = projection_task.await;
        let _ = poller_task.await;
        stream_tasks.wait().await;
    })
    .await
    .is_err()
    {
        warn!("Docker observer task shutdown deadline exceeded");
    }
    database.close().await;
    Ok(())
}

fn validate_restore(
    state_directory: &std::path::Path,
    config_file: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    use solodock::security::permissions::{check_private, check_private_tree};

    check_private(state_directory, true)?;
    let config = Config::load_from(config_file)?;
    let key_path = state_directory.join("secrets/idempotency.key");
    check_private_tree(state_directory, &key_path, false)?;
    let key = std::fs::read(&key_path)?;
    if key.len() != 32 {
        return Err("restore integrity key is invalid".into());
    }
    let apps_directory = state_directory.join("apps");
    check_private_tree(state_directory, &apps_directory, true)?;
    solodock::app_store::config_revision::normalize_managed_file_permissions(&apps_directory)?;
    let store = AppStore::initialize_managed(
        apps_directory,
        key.clone(),
        config.allowed_bind_roots.clone(),
    )?;
    let report = solodock::app_store::recovery::scan_read_only_relocated(
        store.apps_directory(),
        &config.apps_directory(),
        Some(store.integrity_key()?),
        store.allowed_bind_roots(),
    )?;
    if !report.issues.is_empty() {
        return Err("restored application state is degraded".into());
    }
    let webhooks = WebhookStore::new(store.clone(), key.clone());
    for app in &report.valid_apps {
        if webhooks.status(app.app_id)?.configured {
            let _secret = webhooks.load_current(app.app_id)?;
        }
    }
    let credential_root = state_directory.join("registry-credentials");
    if credential_root.exists() {
        check_private_tree(state_directory, &credential_root, true)?;
        check_private_tree(state_directory, &credential_root.join(".trash"), true)?;
        let credentials = CredentialStore::initialize(credential_root, key)?;
        for metadata in credentials.list()? {
            let _credential = credentials.load(metadata.id)?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("shutdown signal received");
}
