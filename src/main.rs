use std::{error::Error, net::SocketAddr};

use solodock::{
    AppState, app_store::AppStore, auth::AuthService, config::Config, db::Database,
    security::permissions::ensure_private_directory,
};
use tokio::net::TcpListener;
use tokio::sync::watch;
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

    let listener = TcpListener::bind(config.listen_address).await?;
    info!(listen_address = %config.listen_address, "SoloDock API listening");
    let state = AppState {
        auth,
        public_origin: config.public_origin,
    };
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_sender.send(true);
    });
    let mut graceful_receiver = shutdown_receiver.clone();
    let server = axum::serve(
        listener,
        solodock::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        while !*graceful_receiver.borrow() {
            if graceful_receiver.changed().await.is_err() {
                break;
            }
        }
    });
    let server = async move { server.await };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        () = shutdown_deadline(shutdown_receiver) => {
            warn!("graceful shutdown deadline exceeded");
        }
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

async fn shutdown_deadline(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
}
