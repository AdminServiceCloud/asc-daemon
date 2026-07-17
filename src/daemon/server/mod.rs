//! Daemon runtime: startup, main loop, graceful shutdown.
//!
//! Future subsystems — the platform tunnel among them — are spawned from
//! [`run`] as tasks and stopped on shutdown, like the API server, the
//! metrics sampler and the scheduler already are.

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

    // Cron-like scheduler (DMN-012): runs scheduled app backups (DMN-009).
    crate::daemon::scheduler::start(&state.config);

    // One shutdown signal fans out to both listeners: the TCP API (bearer
    // token, for the platform) and the local unix socket (peer-cred auth,
    // for the CLI, DMN-042). The unix socket is best-effort — a host where
    // it cannot be bound (no /run/asc for a non-root daemon) keeps the TCP
    // API instead of refusing to start; the CLI reports the socket's
    // absence with a hint when a user actually needs it.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    tokio::spawn(async move {
        shutdown_signal().await;
        drop(shutdown_tx);
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<()>| async move {
        // Resolves when the sender is dropped after the signal.
        let _ = rx.changed().await;
    };
    let uds_state = std::sync::Arc::clone(&state);
    let uds_shutdown = wait(shutdown_rx.clone());
    let uds = async {
        if let Err(err) = api::uds::serve(uds_state, uds_shutdown).await {
            tracing::warn!(error = %format!("{err:#}"), "local unix-socket API unavailable");
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(api::serve(state, wait(shutdown_rx)), uds)?;

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
