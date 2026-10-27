//! sandboxd: host-side manager for Apple Container agent sandboxes.
//!
//! Owns container lifecycles, per-sandbox networks and egress proxies,
//! workspaces, and cleanup. Clients talk to it over a Unix socket that is
//! never exposed to the sandboxes themselves.

mod agents;
mod config;
mod dns;
mod gitguard;
mod manager;
mod policy;
mod proxy;
mod runtime;
mod server;
mod state;
mod workspace;

use std::sync::Arc;

use anyhow::Result;
use sandbox_core::paths::Paths;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("SANDBOXD_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let paths = Paths::resolve();
    manager::ensure_dirs(&paths)?;
    let cfg = config::Config::load(&paths.config())?;
    let profiles = config::load_profiles(&paths.policies())?;
    let presets = agents::load(&paths.home.join("agents"))?;
    let store = state::Store::open(&paths.state_db())?;
    let mgr = Arc::new(manager::Manager::new(cfg, profiles, presets, paths.clone(), store));

    let listener = server::bind(&paths.socket()).await?;
    if let Err(e) = mgr.reconcile().await {
        tracing::error!(error = %format!("{e:#}"), "startup reconciliation failed");
    }
    tokio::spawn(mgr.clone().reap_loop());
    tracing::info!(socket = %paths.socket().display(), "sandboxd ready");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = server::serve(listener, mgr) => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    // Sandboxes keep running; their proxies stop with us, so they fail closed
    // until the next start re-attaches.
    let _ = std::fs::remove_file(paths.socket());
    tracing::info!("sandboxd stopped");
    Ok(())
}
