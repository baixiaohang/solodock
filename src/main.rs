use std::{error::Error, net::SocketAddr, sync::Arc};

use solodock::{
    AppState,
    app_store::AppStore,
    auth::AuthService,
    config::Config,
    db::Database,
    docker::{
        AppCatalog, DockerObserver,
        client::BollardReadClient,
        events::DockerEventHub,
        logs::{EmptySecretProvider, SecretRedactor},
        probe::DockerSupervisor,
        stats::StatsHub,
    },
    security::permissions::ensure_private_directory,
};
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::load()?;
    solodock::telemetry::initialize();

    ensure_private_directory(&config.state_directory)?;
    ensure_private_directory(&config.runtime_directory)?;
    let app_store = AppStore::initialize(config.apps_directory())?;
    let database = Database::open(&config.database_path()).await?;
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
    let shutdown = CancellationToken::new();
    let stream_tasks = TaskTracker::new();
    let supervisor = DockerSupervisor::new();
    let supervisor_task = supervisor.start(docker_api.clone(), shutdown.clone());
    let event_hub = DockerEventHub::new();
    let event_task = event_hub.start(docker_api.clone(), catalog.clone(), shutdown.clone());
    let observer = DockerObserver::new(docker_api.clone(), catalog, supervisor);
    let stats = StatsHub::new(docker_api, shutdown.clone(), stream_tasks.clone());

    let listener = TcpListener::bind(config.listen_address).await?;
    info!(listen_address = %config.listen_address, "SoloDock API listening");
    let state = AppState {
        auth,
        public_origin: config.public_origin,
        observer,
        events: event_hub,
        stats,
        stream_gate: solodock::api::streams::StreamGate::default(),
        redactor: SecretRedactor::new(&EmptySecretProvider),
        state_directory: config.state_directory.clone(),
        shutdown: shutdown.clone(),
        stream_tasks: stream_tasks.clone(),
    };
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
