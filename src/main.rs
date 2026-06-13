//! Binary entry point: init tracing, load+validate config (fail fast), build shared state, serve
//! with a graceful SIGTERM/Ctrl-C drain that waits for in-flight inbound forwards to finish.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

use wagw_shimmy::{AppState, build_router, config::Config, error::DynError};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        // The error messages here name variables, not values — safe to print.
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), DynError> {
    init_tracing();

    let config = Arc::new(Config::from_env()?);
    let bind = config.bind;
    let state = AppState::new(config)?;
    let tasks = state.tasks.clone();

    let app = build_router(state);
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "wagw-shimmy listening");
    tracing::info!("routes: POST /webhook/gowa, POST /send, GET /healthz");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // The HTTP server has stopped accepting; drain spawned inbound-forward tasks so an in-flight
    // agent delivery isn't dropped on the floor at shutdown. Bounded so a wedged agent can't block
    // exit forever.
    tracing::info!("draining in-flight inbound forwards");
    tasks.close();
    tokio::select! {
        _ = tasks.wait() => tracing::info!("drain complete"),
        _ = tokio::time::sleep(Duration::from_secs(15)) => {
            tracing::warn!("drain timed out after 15s; exiting anyway");
        }
    }
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

/// Resolve on Ctrl-C or SIGTERM (systemd stop), whichever comes first.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::warn!(%error, "failed to install SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
