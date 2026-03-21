use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::upstream::UpstreamPool;

const CAPTIVE_PROBE_URL: &str = "http://connectivitycheck.gstatic.com/generate_204";
const CAPTIVE_PROBE_HOST: &str = "connectivitycheck.gstatic.com";
// Enough to capture the status line and a few headers for probe validation.
const PROBE_RESPONSE_BUFFER_SIZE: usize = 256;

/// Runs the active health-checker in the background.
///
/// Every `interval_secs` seconds it iterates over every **offline** upstream,
/// sends a captive-style HTTP probe request through the upstream proxy with
/// `timeout_secs` deadline, and marks the upstream **online** if it returns
/// HTTP 204.
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
                match probe_proxy_by_captive_http(&entry, &addr, t).await {
                    Ok(true) => {
                        info!(upstream = %entry.config.url, "health check OK (HTTP 204) — marking online");
                        entry.mark_online();
                    }
                    Ok(false) => {
                        debug!(upstream = %entry.config.url, "health check got non-204 response");
                    }
                    Err(e) => {
                        debug!(upstream = %entry.config.url, error = %e, "health check failed");
                    }
                }
            })
        })
        .collect();

    for task in tasks {
        let _ = task.await;
    }
}

async fn probe_proxy_by_captive_http(
    entry: &Arc<crate::upstream::UpstreamEntry>,
    addr: &str,
    probe_timeout: Duration,
) -> Result<bool, std::io::Error> {
    let mut stream = timeout(probe_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection to upstream proxy timed out",
            )
        })??;

    let mut req = format!(
        "GET {CAPTIVE_PROBE_URL} HTTP/1.1\r\nHost: {CAPTIVE_PROBE_HOST}\r\nConnection: close\r\nUser-Agent: http-proxy-lb-healthcheck/1.0\r\n"
    );
    if let Some(auth) = entry.config.proxy_auth_header() {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");

    timeout(probe_timeout, stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout"))??;

    let mut buf = [0u8; PROBE_RESPONSE_BUFFER_SIZE];
    let n = timeout(probe_timeout, stream.read(&mut buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))??;
    if n == 0 {
        // Proxy closed connection before returning any HTTP response.
        return Ok(false);
    }

    let head = String::from_utf8_lossy(&buf[..n]);
    let Some(status_line) = head.lines().next() else {
        // Malformed HTTP response: no status line.
        return Ok(false);
    };
    Ok(status_line.starts_with("HTTP/1.1 204") || status_line.starts_with("HTTP/1.0 204"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BalanceMode, Config, HealthCheckConfig, UpstreamConfig};
    use crate::upstream::UpstreamPool;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn captive_204_marks_offline_upstream_online() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("GET http://connectivitycheck.gstatic.com/generate_204 HTTP/1.1"));
            let resp = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).await.unwrap();
        });

        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            mode: BalanceMode::RoundRobin,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            upstream: vec![UpstreamConfig {
                url: format!("http://127.0.0.1:{port}"),
                weight: 1,
                priority: 100,
                username: None,
                password: None,
            }],
        };
        let pool = UpstreamPool::from_config(&cfg);
        let (_, entry) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        entry.mark_offline();
        assert!(!entry.is_online());

        probe_offline(&pool, Duration::from_secs(2)).await;
        assert!(entry.is_online());
    }

    #[tokio::test]
    async fn non_204_keeps_upstream_offline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp =
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).await.unwrap();
        });

        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            mode: BalanceMode::RoundRobin,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            upstream: vec![UpstreamConfig {
                url: format!("http://127.0.0.1:{port}"),
                weight: 1,
                priority: 100,
                username: None,
                password: None,
            }],
        };
        let pool = UpstreamPool::from_config(&cfg);
        let (_, entry) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        entry.mark_offline();
        assert!(!entry.is_online());

        probe_offline(&pool, Duration::from_secs(2)).await;
        assert!(!entry.is_online());
    }
}
