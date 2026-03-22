//! Admin server providing metrics and status endpoints.
//!
//! Endpoints:
//! * `GET /metrics` — Prometheus-compatible metrics
//! * `GET /status` — JSON status of all upstreams
//! * `GET /health` — Simple health check (returns 200 OK)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

use crate::upstream::UpstreamPool;

// ---------------------------------------------------------------------------
// Global metrics
// ---------------------------------------------------------------------------

/// Global metrics counters shared across the application.
pub struct Metrics {
    /// Total number of requests received
    pub requests_total: AtomicU64,
    /// Total number of successful requests
    pub requests_success: AtomicU64,
    /// Total number of failed requests
    pub requests_failed: AtomicU64,
    /// Total number of CONNECT requests
    pub connect_requests: AtomicU64,
    /// Total number of HTTP (non-CONNECT) requests
    pub http_requests: AtomicU64,
    /// Total number of direct connections (bypassing upstream)
    pub direct_connections: AtomicU64,
    /// Total bytes received from clients
    pub bytes_received: AtomicU64,
    /// Total bytes sent to clients
    pub bytes_sent: AtomicU64,
    /// Current active connections
    pub active_connections: AtomicU64,
    /// Total health check probes
    pub health_checks_total: AtomicU64,
    /// Successful health check probes
    pub health_checks_success: AtomicU64,
    /// Hot reload count
    pub hot_reloads: AtomicU64,
    /// Start time for uptime calculation
    start_time: Instant,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            requests_total: AtomicU64::new(0),
            requests_success: AtomicU64::new(0),
            requests_failed: AtomicU64::new(0),
            connect_requests: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            direct_connections: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            health_checks_total: AtomicU64::new(0),
            health_checks_success: AtomicU64::new(0),
            hot_reloads: AtomicU64::new(0),
            start_time: Instant::now(),
        })
    }

    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    pub fn inc_requests_total(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_requests_success(&self) {
        self.requests_success.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_requests_failed(&self) {
        self.requests_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_connect(&self) {
        self.connect_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_http(&self) {
        self.http_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_direct(&self) {
        self.direct_connections.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn add_bytes_received(&self, n: u64) {
        self.bytes_received.fetch_add(n, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn add_bytes_sent(&self, n: u64) {
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_active(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_active(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_health_check(&self) {
        self.health_checks_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_health_check_success(&self) {
        self.health_checks_success.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_hot_reload(&self) {
        self.hot_reloads.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            requests_success: AtomicU64::new(0),
            requests_failed: AtomicU64::new(0),
            connect_requests: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            direct_connections: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            health_checks_total: AtomicU64::new(0),
            health_checks_success: AtomicU64::new(0),
            hot_reloads: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Admin server
// ---------------------------------------------------------------------------

/// Run the admin HTTP server on the given address.
pub async fn run_admin_server(addr: String, pool: Arc<UpstreamPool>, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to bind admin server");
            return;
        }
    };

    info!(addr = %addr, "admin server listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let pool = Arc::clone(&pool);
                let metrics = Arc::clone(&metrics);
                tokio::spawn(async move {
                    if let Err(e) = handle_admin_request(stream, &pool, &metrics).await {
                        debug!(error = %e, "admin request error");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "admin accept error");
            }
        }
    }
}

async fn handle_admin_request(
    mut stream: TcpStream,
    pool: &Arc<UpstreamPool>,
    metrics: &Arc<Metrics>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");

    if first_line.starts_with("GET /metrics") {
        let body = build_prometheus_metrics(pool, metrics);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await?;
    } else if first_line.starts_with("GET /status") {
        let body = build_status_json(pool, metrics);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await?;
    } else if first_line.starts_with("GET /health") {
        let body = r#"{"status":"ok"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await?;
    } else {
        let body = "Not Found";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Prometheus metrics format
// ---------------------------------------------------------------------------

fn build_prometheus_metrics(pool: &Arc<UpstreamPool>, metrics: &Arc<Metrics>) -> String {
    let mut out = String::with_capacity(4096);

    // Basic metrics
    out.push_str("# HELP http_proxy_lb_uptime_seconds Time since proxy started\n");
    out.push_str("# TYPE http_proxy_lb_uptime_seconds gauge\n");
    out.push_str(&format!(
        "http_proxy_lb_uptime_seconds {}\n",
        metrics.uptime_secs()
    ));

    out.push_str("# HELP http_proxy_lb_requests_total Total number of requests received\n");
    out.push_str("# TYPE http_proxy_lb_requests_total counter\n");
    out.push_str(&format!(
        "http_proxy_lb_requests_total {}\n",
        metrics.requests_total.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_requests_success Total number of successful requests\n");
    out.push_str("# TYPE http_proxy_lb_requests_success counter\n");
    out.push_str(&format!(
        "http_proxy_lb_requests_success {}\n",
        metrics.requests_success.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_requests_failed Total number of failed requests\n");
    out.push_str("# TYPE http_proxy_lb_requests_failed counter\n");
    out.push_str(&format!(
        "http_proxy_lb_requests_failed {}\n",
        metrics.requests_failed.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_connect_requests Total number of CONNECT requests\n");
    out.push_str("# TYPE http_proxy_lb_connect_requests counter\n");
    out.push_str(&format!(
        "http_proxy_lb_connect_requests {}\n",
        metrics.connect_requests.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_http_requests Total number of HTTP requests\n");
    out.push_str("# TYPE http_proxy_lb_http_requests counter\n");
    out.push_str(&format!(
        "http_proxy_lb_http_requests {}\n",
        metrics.http_requests.load(Ordering::Relaxed)
    ));

    out.push_str(
        "# HELP http_proxy_lb_direct_connections Total number of direct (non-proxy) connections\n",
    );
    out.push_str("# TYPE http_proxy_lb_direct_connections counter\n");
    out.push_str(&format!(
        "http_proxy_lb_direct_connections {}\n",
        metrics.direct_connections.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_bytes_received Total bytes received from clients\n");
    out.push_str("# TYPE http_proxy_lb_bytes_received counter\n");
    out.push_str(&format!(
        "http_proxy_lb_bytes_received {}\n",
        metrics.bytes_received.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_bytes_sent Total bytes sent to clients\n");
    out.push_str("# TYPE http_proxy_lb_bytes_sent counter\n");
    out.push_str(&format!(
        "http_proxy_lb_bytes_sent {}\n",
        metrics.bytes_sent.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_active_connections Current number of active connections\n");
    out.push_str("# TYPE http_proxy_lb_active_connections gauge\n");
    out.push_str(&format!(
        "http_proxy_lb_active_connections {}\n",
        metrics.active_connections.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_health_checks_total Total health check probes\n");
    out.push_str("# TYPE http_proxy_lb_health_checks_total counter\n");
    out.push_str(&format!(
        "http_proxy_lb_health_checks_total {}\n",
        metrics.health_checks_total.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_health_checks_success Successful health check probes\n");
    out.push_str("# TYPE http_proxy_lb_health_checks_success counter\n");
    out.push_str(&format!(
        "http_proxy_lb_health_checks_success {}\n",
        metrics.health_checks_success.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP http_proxy_lb_hot_reloads Total config hot reloads\n");
    out.push_str("# TYPE http_proxy_lb_hot_reloads counter\n");
    out.push_str(&format!(
        "http_proxy_lb_hot_reloads {}\n",
        metrics.hot_reloads.load(Ordering::Relaxed)
    ));

    // Per-upstream metrics
    out.push_str(
        "# HELP http_proxy_lb_upstream_online Whether upstream is online (1) or offline (0)\n",
    );
    out.push_str("# TYPE http_proxy_lb_upstream_online gauge\n");

    out.push_str(
        "# HELP http_proxy_lb_upstream_active_connections Active connections per upstream\n",
    );
    out.push_str("# TYPE http_proxy_lb_upstream_active_connections gauge\n");

    out.push_str(
        "# HELP http_proxy_lb_upstream_latency_ms Latency EMA in milliseconds per upstream\n",
    );
    out.push_str("# TYPE http_proxy_lb_upstream_latency_ms gauge\n");

    out.push_str(
        "# HELP http_proxy_lb_upstream_consecutive_failures Consecutive failures per upstream\n",
    );
    out.push_str("# TYPE http_proxy_lb_upstream_consecutive_failures gauge\n");

    for status in pool.all_status() {
        let url_escaped = status.url.replace('"', "\\\"");
        out.push_str(&format!(
            "http_proxy_lb_upstream_online{{url=\"{}\"}} {}\n",
            url_escaped,
            if status.online { 1 } else { 0 }
        ));
        out.push_str(&format!(
            "http_proxy_lb_upstream_active_connections{{url=\"{}\"}} {}\n",
            url_escaped, status.active_conns
        ));
        out.push_str(&format!(
            "http_proxy_lb_upstream_latency_ms{{url=\"{}\"}} {}\n",
            url_escaped, status.latency_ema_ms
        ));
        out.push_str(&format!(
            "http_proxy_lb_upstream_consecutive_failures{{url=\"{}\"}} {}\n",
            url_escaped, status.consec_failures
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// JSON status
// ---------------------------------------------------------------------------

fn build_status_json(pool: &Arc<UpstreamPool>, metrics: &Arc<Metrics>) -> String {
    let upstreams: Vec<String> = pool
        .all_status()
        .into_iter()
        .map(|s| {
            format!(
                r#"{{"url":"{}","online":{},"active_connections":{},"latency_ms":{},"consecutive_failures":{},"weight":{},"priority":{}}}"#,
                s.url.replace('"', "\\\""),
                s.online,
                s.active_conns,
                s.latency_ema_ms,
                s.consec_failures,
                s.weight,
                s.priority
            )
        })
        .collect();

    format!(
        r#"{{"uptime_seconds":{},"requests_total":{},"requests_success":{},"requests_failed":{},"connect_requests":{},"http_requests":{},"direct_connections":{},"bytes_received":{},"bytes_sent":{},"active_connections":{},"health_checks_total":{},"health_checks_success":{},"hot_reloads":{},"upstreams":[{}]}}"#,
        metrics.uptime_secs(),
        metrics.requests_total.load(Ordering::Relaxed),
        metrics.requests_success.load(Ordering::Relaxed),
        metrics.requests_failed.load(Ordering::Relaxed),
        metrics.connect_requests.load(Ordering::Relaxed),
        metrics.http_requests.load(Ordering::Relaxed),
        metrics.direct_connections.load(Ordering::Relaxed),
        metrics.bytes_received.load(Ordering::Relaxed),
        metrics.bytes_sent.load(Ordering::Relaxed),
        metrics.active_connections.load(Ordering::Relaxed),
        metrics.health_checks_total.load(Ordering::Relaxed),
        metrics.health_checks_success.load(Ordering::Relaxed),
        metrics.hot_reloads.load(Ordering::Relaxed),
        upstreams.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_increment() {
        let m = Metrics::new();
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 0);
        m.inc_requests_total();
        m.inc_requests_total();
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_metrics_active_connections() {
        let m = Metrics::new();
        m.inc_active();
        m.inc_active();
        assert_eq!(m.active_connections.load(Ordering::Relaxed), 2);
        m.dec_active();
        assert_eq!(m.active_connections.load(Ordering::Relaxed), 1);
    }
}
