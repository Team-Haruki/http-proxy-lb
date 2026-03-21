use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::upstream::UpstreamPool;

/// Runs the active health-checker in the background.
///
/// Every `interval_secs` seconds it iterates over every **offline** upstream,
/// attempts a TCP-connect with `timeout_secs` deadline, and marks the upstream
/// **online** if the connection succeeds.
pub async fn run_health_checker(pool: Arc<UpstreamPool>, interval_secs: u64, timeout_secs: u64) {
    let interval = Duration::from_secs(interval_secs);
    let probe_timeout = Duration::from_secs(timeout_secs);

    loop {
        tokio::time::sleep(interval).await;
        probe_offline(&pool, probe_timeout).await;
    }
}

async fn probe_offline(pool: &Arc<UpstreamPool>, probe_timeout: Duration) {
    let offline = pool.offline_entries();
    if offline.is_empty() {
        return;
    }

    debug!(
        count = offline.len(),
        "active health check: probing offline upstreams"
    );

    let tasks: Vec<_> = offline
        .into_iter()
        .map(|entry| {
            let t = probe_timeout;
            tokio::spawn(async move {
                let addr = match entry.host_port() {
                    Ok((h, p)) => format!("{h}:{p}"),
                    Err(e) => {
                        debug!(url = %entry.config.url, error = %e, "invalid upstream URL");
                        return;
                    }
                };
                match timeout(t, TcpStream::connect(&addr)).await {
                    Ok(Ok(_)) => {
                        info!(upstream = %entry.config.url, "health check OK — marking online");
                        entry.mark_online();
                    }
                    Ok(Err(e)) => {
                        debug!(upstream = %entry.config.url, error = %e, "health check failed");
                    }
                    Err(_) => {
                        debug!(upstream = %entry.config.url, "health check timed out");
                    }
                }
            })
        })
        .collect();

    for task in tasks {
        let _ = task.await;
    }
}
