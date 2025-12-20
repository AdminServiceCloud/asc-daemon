//! Daemon runtime: startup, main loop, graceful shutdown.
//!
//! Future subsystems — ConnectRPC API (DMN-005), platform tunnel, scheduler,
//! monitoring — are spawned from [`run`] as tasks and stopped on shutdown.

use anyhow::Context;
use tracing::info;

use crate::daemon::api;
use crate::daemon::config::Config;

/// Run the daemon until SIGTERM / Ctrl+C.
pub async fn run(mut config: Config) -> anyhow::Result<()> {
    println!("{}", crate::BANNER);
    println!();
    info!(version = crate::VERSION, "asc daemon starting");

    std::fs::create_dir_all(&config.daemon.data_dir).with_context(|| {
        format!(
            "cannot create data directory {}",
            config.daemon.data_dir.display()
        )
    })?;
    info!(data_dir = %config.daemon.data_dir.display(), "data directory ready");

    // Recovery pass: bring apps back to their desired state after a reboot.
    let apps = crate::daemon::apps::AppManager::new(&config);
    tokio::task::spawn_blocking(move || {
        if let Err(err) = apps.reconcile() {
            tracing::warn!(error = %format!("{err:#}"), "app reconcile failed");
        }
    })
    .await
    .context("app reconcile task panicked")?;

    // API server (gRPC + REST) runs until a shutdown signal arrives.
    let token = api::ensure_api_token(&mut config)?;
    let state = api::ApiState::new(config, token);

    // Background metrics sampler (DMN-006) feeds the API's ring buffer;
    // the task dies with the runtime on shutdown.
    state.monitor.start_sampler(&state.config.monitor);

    api::serve(state, shutdown_signal()).await?;

    info!("asc daemon stopped");
    Ok(())
}

/// Wait for a shutdown request: SIGTERM (systemd stop) or Ctrl+C.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("received Ctrl+C"),
        _ = term.recv() => info!("received SIGTERM"),
    }
}
