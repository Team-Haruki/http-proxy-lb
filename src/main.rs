mod config;
mod health;
mod proxy;
mod upstream;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use config::{file_mtime, load_config};
use upstream::UpstreamPool;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// HTTP proxy load balancer — relay local HTTP proxy traffic through a pool of
/// upstream proxies with health checking and hot reload.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the YAML configuration file
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Initialise structured logging (RUST_LOG controls verbosity; default: info)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg_path = cli.config.clone();

    // Load initial config
    let cfg =
        load_config(&cfg_path).with_context(|| format!("failed to load config from {cfg_path}"))?;

    info!(listen = %cfg.listen, mode = ?cfg.mode, upstreams = cfg.upstream.len(), "starting");

    let listen_addr = cfg.listen.clone();
    let mode = cfg.mode;
    let reload_interval = cfg.reload_interval_secs;
    let hc_interval = cfg.health_check.interval_secs;
    let hc_timeout = cfg.health_check.timeout_secs;
    let domain_policy = Arc::new(cfg.domain_policy.clone());

    // Build upstream pool
    let pool = UpstreamPool::from_config(&cfg);

    // Bind listener
    let listener = TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind to {listen_addr}"))?;
    info!(addr = %listen_addr, "listening");

    // --- Spawn active health checker ---
    {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            health::run_health_checker(pool, hc_interval, hc_timeout).await;
        });
    }

    // --- Spawn hot-reload watcher ---
    if reload_interval > 0 {
        let pool = Arc::clone(&pool);
        let path = cfg_path.clone();
        tokio::spawn(async move {
            run_hot_reload(pool, path, reload_interval).await;
        });
    }

    // --- Accept loop ---
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug_assert_ne!(peer.port(), 0);
                let pool = Arc::clone(&pool);
                let domain_policy = Arc::clone(&domain_policy);
                tokio::spawn(async move {
                    proxy::handle_client(stream, pool, mode, domain_policy).await;
                });
            }
            Err(e) => {
                error!(error = %e, "accept error");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hot-reload loop
// ---------------------------------------------------------------------------

async fn run_hot_reload(pool: Arc<UpstreamPool>, cfg_path: String, interval_secs: u64) {
    let mut last_mtime = file_mtime(&cfg_path);
    let interval = Duration::from_secs(interval_secs);

    loop {
        tokio::time::sleep(interval).await;

        let current_mtime = file_mtime(&cfg_path);
        if current_mtime == last_mtime {
            continue;
        }

        info!(path = %cfg_path, "config file changed — reloading");
        match load_config(&cfg_path) {
            Ok(new_cfg) => {
                pool.reload(&new_cfg);
                last_mtime = current_mtime;
            }
            Err(e) => {
                warn!(error = %e, "hot reload failed — keeping current config");
            }
        }
    }
}
