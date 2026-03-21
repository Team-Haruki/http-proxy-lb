mod admin;
mod config;
mod health;
mod proxy;
mod upstream;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use admin::Metrics;
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

    /// Validate configuration file and exit
    #[arg(long)]
    check: bool,
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

    // Config check mode
    if cli.check {
        println!("Configuration file '{}' is valid.", cfg_path);
        println!("  Listen: {}", cfg.listen);
        if let Some(ref admin) = cfg.admin_listen {
            println!("  Admin: {}", admin);
        }
        println!("  Mode: {:?}", cfg.mode);
        println!("  Upstreams: {}", cfg.upstream.len());
        for u in &cfg.upstream {
            println!("    - {} (weight={}, priority={})", u.url, u.weight, u.priority);
        }
        println!("  Domain policy: {:?}", cfg.domain_policy.mode);
        println!("  Access log: {}", cfg.access_log);
        println!("  Limits:");
        println!("    max_connections: {}", if cfg.limits.max_connections == 0 { "unlimited".to_string() } else { cfg.limits.max_connections.to_string() });
        println!("    request_timeout: {}s", if cfg.limits.request_timeout_secs == 0 { "unlimited".to_string() } else { cfg.limits.request_timeout_secs.to_string() });
        println!("    shutdown_timeout: {}s", cfg.limits.shutdown_timeout_secs);
        return Ok(());
    }

    info!(listen = %cfg.listen, mode = ?cfg.mode, upstreams = cfg.upstream.len(), "starting");

    let listen_addr = cfg.listen.clone();
    let admin_listen = cfg.admin_listen.clone();
    let mode = cfg.mode;
    let reload_interval = cfg.reload_interval_secs;
    let hc_interval = cfg.health_check.interval_secs;
    let hc_timeout = cfg.health_check.timeout_secs;
    let domain_policy = Arc::new(cfg.domain_policy.clone());
    let access_log = cfg.access_log;
    let max_connections = cfg.limits.max_connections;
    let request_timeout = if cfg.limits.request_timeout_secs > 0 {
        Some(Duration::from_secs(cfg.limits.request_timeout_secs))
    } else {
        None
    };
    let shutdown_timeout = Duration::from_secs(cfg.limits.shutdown_timeout_secs);

    // Build upstream pool
    let pool = UpstreamPool::from_config(&cfg);

    // Create metrics
    let metrics = Metrics::new();

    // Connection limiter (None if unlimited)
    let conn_semaphore = if max_connections > 0 {
        Some(Arc::new(Semaphore::new(max_connections)))
    } else {
        None
    };

    // Shutdown flag
    let shutdown = Arc::new(AtomicBool::new(false));

    // Bind listener
    let listener = TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind to {listen_addr}"))?;
    info!(addr = %listen_addr, "listening");

    // --- Spawn admin server ---
    if let Some(admin_addr) = admin_listen {
        let pool = Arc::clone(&pool);
        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            admin::run_admin_server(admin_addr, pool, metrics).await;
        });
    }

    // --- Spawn active health checker ---
    {
        let pool = Arc::clone(&pool);
        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            health::run_health_checker(pool, metrics, hc_interval, hc_timeout).await;
        });
    }

    // --- Spawn hot-reload watcher ---
    if reload_interval > 0 {
        let pool = Arc::clone(&pool);
        let metrics = Arc::clone(&metrics);
        let path = cfg_path.clone();
        tokio::spawn(async move {
            run_hot_reload(pool, metrics, path, reload_interval).await;
        });
    }

    // --- Run accept loop with graceful shutdown ---
    run_server(
        listener,
        pool,
        mode,
        domain_policy,
        metrics,
        conn_semaphore,
        request_timeout,
        access_log,
        shutdown,
        shutdown_timeout,
    )
    .await
}

// ---------------------------------------------------------------------------
// Server with graceful shutdown
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn run_server(
    listener: TcpListener,
    pool: Arc<UpstreamPool>,
    mode: config::BalanceMode,
    domain_policy: Arc<config::DomainPolicyConfig>,
    metrics: Arc<Metrics>,
    conn_semaphore: Option<Arc<Semaphore>>,
    request_timeout: Option<Duration>,
    access_log: bool,
    shutdown: Arc<AtomicBool>,
    shutdown_timeout: Duration,
) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

    loop {
        tokio::select! {
            result = listener.accept() => {
                handle_accept(
                    result, &pool, mode, &domain_policy, &metrics,
                    &conn_semaphore, request_timeout, access_log
                );
            }
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, initiating graceful shutdown");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, initiating graceful shutdown");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
        }
    }

    wait_for_shutdown(&metrics, shutdown_timeout).await;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
async fn run_server(
    listener: TcpListener,
    pool: Arc<UpstreamPool>,
    mode: config::BalanceMode,
    domain_policy: Arc<config::DomainPolicyConfig>,
    metrics: Arc<Metrics>,
    conn_semaphore: Option<Arc<Semaphore>>,
    request_timeout: Option<Duration>,
    access_log: bool,
    shutdown: Arc<AtomicBool>,
    shutdown_timeout: Duration,
) -> Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                handle_accept(
                    result, &pool, mode, &domain_policy, &metrics,
                    &conn_semaphore, request_timeout, access_log
                );
            }
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, initiating graceful shutdown");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
        }
    }

    wait_for_shutdown(&metrics, shutdown_timeout).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_accept(
    result: std::io::Result<(TcpStream, std::net::SocketAddr)>,
    pool: &Arc<UpstreamPool>,
    mode: config::BalanceMode,
    domain_policy: &Arc<config::DomainPolicyConfig>,
    metrics: &Arc<Metrics>,
    conn_semaphore: &Option<Arc<Semaphore>>,
    request_timeout: Option<Duration>,
    access_log: bool,
) {
    match result {
        Ok((stream, peer)) => {
            // Check connection limit
            let permit = if let Some(ref sem) = conn_semaphore {
                match sem.clone().try_acquire_owned() {
                    Ok(p) => Some(p),
                    Err(_) => {
                        warn!(peer = %peer, "connection limit reached, rejecting");
                        drop(stream);
                        return;
                    }
                }
            } else {
                None
            };

            let pool = Arc::clone(pool);
            let domain_policy = Arc::clone(domain_policy);
            let metrics = Arc::clone(metrics);

            metrics.inc_active();

            tokio::spawn(async move {
                if let Some(timeout) = request_timeout {
                    let _ = tokio::time::timeout(
                        timeout,
                        proxy::handle_client(stream, pool, mode, domain_policy, &metrics, access_log),
                    )
                    .await;
                } else {
                    proxy::handle_client(stream, pool, mode, domain_policy, &metrics, access_log).await;
                }
                metrics.dec_active();
                drop(permit);
            });
        }
        Err(e) => {
            error!(error = %e, "accept error");
        }
    }
}

async fn wait_for_shutdown(metrics: &Arc<Metrics>, shutdown_timeout: Duration) {
    info!(timeout_secs = shutdown_timeout.as_secs(), "waiting for active connections to complete");

    let wait_start = std::time::Instant::now();
    loop {
        let active = metrics.active_connections.load(Ordering::Relaxed);
        if active == 0 {
            info!("all connections closed, shutting down");
            break;
        }
        if wait_start.elapsed() >= shutdown_timeout {
            warn!(active = active, "shutdown timeout reached, forcing shutdown");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Hot-reload loop
// ---------------------------------------------------------------------------

async fn run_hot_reload(
    pool: Arc<UpstreamPool>,
    metrics: Arc<Metrics>,
    cfg_path: String,
    interval_secs: u64,
) {
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
                metrics.inc_hot_reload();
                last_mtime = current_mtime;
            }
            Err(e) => {
                warn!(error = %e, "hot reload failed — keeping current config");
            }
        }
    }
}

