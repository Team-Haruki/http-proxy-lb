//! HTTP/1.x proxy request handler.
//!
//! Supports:
//! * `CONNECT` tunneling (HTTPS)
//! * Plain HTTP forwarding
//! * HTTP/1.1 persistent connections (keep-alive) from the client
//! * Retry on upstream failure (passive health detection)

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

use crate::admin::Metrics;
use crate::config::{BalanceMode, DomainPolicyConfig, DomainPolicyMode};
use crate::upstream::UpstreamPool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_HEADER_SIZE: usize = 64 * 1024; // 64 KiB
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum number of upstream retry attempts per request.
const MAX_RETRIES: usize = 3;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Accept a single client TCP connection and serve HTTP proxy requests on it.
/// Supports keep-alive: loops until the client closes or sends `Connection: close`.
pub async fn handle_client(
    mut client: TcpStream,
    pool: Arc<UpstreamPool>,
    mode: BalanceMode,
    domain_policy: Arc<DomainPolicyConfig>,
    metrics: &Metrics,
    access_log: bool,
) {
    let peer = client
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    debug!(peer = %peer, "new client connection");

    loop {
        match read_headers(&mut client).await {
            Ok(Some(buf)) => {
                metrics.inc_requests_total();
                let start = Instant::now();
                match dispatch(&mut client, &buf, &pool, mode, &domain_policy, metrics, access_log, &peer, start).await {
                    Ok(true) => continue, // keep-alive: read next request
                    Ok(false) => break,
                    Err(e) => {
                        metrics.inc_requests_failed();
                        debug!(peer = %peer, error = %e, "request dispatch error");
                        break;
                    }
                }
            }
            Ok(None) => break, // client closed connection
            Err(e) => {
                debug!(peer = %peer, error = %e, "header read error");
                break;
            }
        }
    }

    debug!(peer = %peer, "client connection closed");
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

/// Returns `Ok(true)` if the client connection should be kept alive.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    client: &mut TcpStream,
    buf: &[u8],
    pool: &Arc<UpstreamPool>,
    mode: BalanceMode,
    domain_policy: &Arc<DomainPolicyConfig>,
    metrics: &Metrics,
    access_log: bool,
    peer: &str,
    start: Instant,
) -> Result<bool> {
    // --- parse request line + headers ---
    let mut raw_headers = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut raw_headers);
    let body_offset = match req.parse(buf)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => bail!("incomplete request headers"),
    };

    let method = req
        .method
        .ok_or_else(|| anyhow!("missing method"))?
        .to_string();
    let path = req.path.ok_or_else(|| anyhow!("missing path"))?.to_string();
    let version = req.version.unwrap_or(1);
    let headers = req.headers;

    // Content-Length / Transfer-Encoding of the *request* body
    let req_content_length: Option<u64> =
        get_header(headers, "content-length").and_then(|v| v.parse().ok());
    let req_is_chunked = get_header(headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    // Does the client want a persistent connection?
    let client_keep_alive = client_wants_keep_alive(headers, version);

    // --- CONNECT (HTTPS tunnel) ---
    if method.eq_ignore_ascii_case("CONNECT") {
        metrics.inc_connect();
        let (host, port) = parse_connect_target(&path)?;
        let use_proxy = should_use_proxy(&host, domain_policy);
        if !use_proxy {
            metrics.inc_direct();
        }
        let result = handle_connect(client, &host, port, pool, mode, use_proxy).await;
        let elapsed_ms = start.elapsed().as_millis();
        let status = if result.is_ok() { 200 } else { 502 };
        if result.is_ok() {
            metrics.inc_requests_success();
        } else {
            metrics.inc_requests_failed();
        }
        if access_log {
            info!(
                peer = %peer,
                method = "CONNECT",
                target = %path,
                status = status,
                elapsed_ms = elapsed_ms,
                direct = !use_proxy,
                "access"
            );
        }
        return result.map(|_| false);
    }

    // --- Plain HTTP ---
    metrics.inc_http();
    let (target_host, target_port) = parse_http_target(&path, headers)?;
    let use_proxy = should_use_proxy(&target_host, domain_policy);
    if !use_proxy {
        metrics.inc_direct();
    }
    let result = handle_http(
        client,
        &method,
        &path,
        buf,
        body_offset,
        req_content_length,
        req_is_chunked,
        client_keep_alive,
        pool,
        mode,
        use_proxy,
        (&target_host, target_port),
    )
    .await;
    let elapsed_ms = start.elapsed().as_millis();
    let status = if result.is_ok() { 200 } else { 502 };
    if result.is_ok() {
        metrics.inc_requests_success();
    } else {
        metrics.inc_requests_failed();
    }
    if access_log {
        info!(
            peer = %peer,
            method = %method,
            target = %path,
            status = status,
            elapsed_ms = elapsed_ms,
            direct = !use_proxy,
            "access"
        );
    }
    result
}

// ---------------------------------------------------------------------------
// CONNECT tunnel
// ---------------------------------------------------------------------------

async fn handle_connect(
    client: &mut TcpStream,
    host: &str,
    port: u16,
    pool: &Arc<UpstreamPool>,
    mode: BalanceMode,
    use_proxy: bool,
) -> Result<()> {
    if !use_proxy {
        let addr = format!("{host}:{port}");
        let up_stream = connect_target(&addr).await?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        return tunnel(client, up_stream).await;
    }

    let mut tried: Vec<usize> = Vec::new();
    let max_retries = pool.len().min(MAX_RETRIES);

    loop {
        let (idx, upstream) = match pool.select(mode, &tried) {
            Some(u) => u,
            None => {
                let _ = write_status(client, 502, "No upstream available").await;
                bail!("no upstream available for CONNECT");
            }
        };
        tried.push(idx);

        let (up_host, up_port) = upstream.host_port()?;
        let addr = format!("{up_host}:{up_port}");

        // Connect to upstream proxy
        let mut up_stream = match connect_upstream(&addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(upstream = %upstream.config.url, error = %e, "CONNECT: upstream TCP failed");
                upstream.mark_offline();
                upstream.record_failure();
                if tried.len() >= max_retries {
                    let _ = write_status(client, 502, "Bad Gateway").await;
                    bail!("all retries exhausted for CONNECT");
                }
                continue;
            }
        };

        // Send CONNECT to upstream proxy
        let connect_req = build_upstream_connect(host, port, upstream.config.proxy_auth_header());
        if let Err(e) = up_stream.write_all(connect_req.as_bytes()).await {
            warn!(upstream = %upstream.config.url, error = %e, "CONNECT: write to upstream failed");
            upstream.mark_offline();
            upstream.record_failure();
            if tried.len() >= max_retries {
                let _ = write_status(client, 502, "Bad Gateway").await;
                bail!("all retries exhausted for CONNECT write");
            }
            continue;
        }

        // Read upstream's response to our CONNECT
        match read_connect_response(&mut up_stream).await {
            Ok(200) => {
                // Success — tell client we're connected
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                upstream
                    .active_conns
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let t0 = Instant::now();
                let result = tunnel(client, up_stream).await;
                upstream
                    .active_conns
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                upstream.record_success(t0.elapsed().as_millis() as u64);
                return result;
            }
            Ok(status) => {
                // Upstream rejected CONNECT (e.g. auth required, forbidden)
                debug!(upstream = %upstream.config.url, status, "CONNECT rejected by upstream");
                let _ = write_status(client, status as u16, "Upstream rejected CONNECT").await;
                bail!("upstream rejected CONNECT with status {status}");
            }
            Err(e) => {
                warn!(upstream = %upstream.config.url, error = %e, "CONNECT: bad upstream response");
                upstream.mark_offline();
                upstream.record_failure();
                if tried.len() >= max_retries {
                    let _ = write_status(client, 502, "Bad Gateway").await;
                    bail!("all retries exhausted reading CONNECT response");
                }
                continue;
            }
        }
    }
}

/// Bidirectional copy between client and upstream until either side closes.
async fn tunnel(client: &mut TcpStream, mut upstream: TcpStream) -> Result<()> {
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = tokio::io::split(&mut upstream);
    tokio::select! {
        r = tokio::io::copy(&mut cr, &mut uw) => { r?; }
        r = tokio::io::copy(&mut ur, &mut cw) => { r?; }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plain HTTP forwarding
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn handle_http(
    client: &mut TcpStream,
    method: &str,
    path: &str,
    req_buf: &[u8],     // raw bytes: headers + any already-buffered body bytes
    body_offset: usize, // where headers end within req_buf
    req_content_length: Option<u64>,
    req_is_chunked: bool,
    client_keep_alive: bool,
    pool: &Arc<UpstreamPool>,
    mode: BalanceMode,
    use_proxy: bool,
    direct_target: (&str, u16),
) -> Result<bool> {
    if !use_proxy {
        let addr = format!("{}:{}", direct_target.0, direct_target.1);
        let mut direct_stream = match connect_target(&addr).await {
            Ok(s) => s,
            Err(_) => {
                let _ = write_status(client, 502, "Bad Gateway").await;
                return Ok(false);
            }
        };
        let no_proxy_auth = None;
        let direct_original_path = Some(path);
        let fwd_headers =
            rewrite_request_headers(req_buf, body_offset, no_proxy_auth, direct_original_path);
        direct_stream.write_all(&fwd_headers).await?;

        let already_in_buf = req_buf.len().saturating_sub(body_offset);
        if already_in_buf > 0 {
            direct_stream.write_all(&req_buf[body_offset..]).await?;
        }
        if forward_body(
            client,
            &mut direct_stream,
            req_content_length,
            req_is_chunked,
            already_in_buf as u64,
        )
        .await
        .is_err()
        {
            return Ok(false);
        }

        let upstream_keep_alive = match forward_response(&mut direct_stream, client, method).await {
            Ok(v) => v,
            Err(_) => return Ok(false),
        };
        return Ok(client_keep_alive && upstream_keep_alive);
    }

    let mut tried: Vec<usize> = Vec::new();
    let max_retries = pool.len().min(MAX_RETRIES);

    loop {
        let (idx, upstream) = match pool.select(mode, &tried) {
            Some(u) => u,
            None => {
                let _ = write_status(client, 502, "No upstream available").await;
                return Ok(false);
            }
        };
        tried.push(idx);

        let (up_host, up_port) = upstream.host_port()?;
        let addr = format!("{up_host}:{up_port}");

        // --- Connect to upstream proxy ---
        let mut up_stream = match connect_upstream(&addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(upstream = %upstream.config.url, error = %e, "HTTP: upstream TCP failed");
                upstream.mark_offline();
                upstream.record_failure();
                if tried.len() >= max_retries {
                    let _ = write_status(client, 502, "Bad Gateway").await;
                    return Ok(false);
                }
                continue;
            }
        };

        // --- Build and send request headers ---
        let fwd_headers = rewrite_request_headers(
            req_buf,
            body_offset,
            upstream.config.proxy_auth_header(),
            None,
        );
        if let Err(e) = up_stream.write_all(&fwd_headers).await {
            warn!(upstream = %upstream.config.url, error = %e, "HTTP: write headers failed");
            upstream.mark_offline();
            upstream.record_failure();
            if tried.len() >= max_retries {
                let _ = write_status(client, 502, "Bad Gateway").await;
                return Ok(false);
            }
            continue;
        }

        // --- Stream request body ---
        let already_in_buf = req_buf.len().saturating_sub(body_offset);
        if already_in_buf > 0 {
            // Body bytes that arrived in the same read as the headers
            if let Err(e) = up_stream.write_all(&req_buf[body_offset..]).await {
                warn!(upstream = %upstream.config.url, error = %e, "HTTP: write buffered body failed");
                upstream.mark_offline();
                upstream.record_failure();
                if tried.len() >= max_retries {
                    return Ok(false);
                }
                continue;
            }
        }
        if let Err(e) = forward_body(
            client,
            &mut up_stream,
            req_content_length,
            req_is_chunked,
            already_in_buf as u64,
        )
        .await
        {
            warn!(error = %e, "HTTP: body forward error");
            return Ok(false);
        }

        // --- Stream response back ---
        let t0 = Instant::now();
        upstream
            .active_conns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp_result = forward_response(&mut up_stream, client, method).await;
        upstream
            .active_conns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        match resp_result {
            Ok(upstream_keep_alive) => {
                upstream.record_success(t0.elapsed().as_millis() as u64);
                return Ok(client_keep_alive && upstream_keep_alive);
            }
            Err(e) => {
                warn!(upstream = %upstream.config.url, error = %e, "HTTP: response forward failed");
                upstream.mark_offline();
                upstream.record_failure();
                if tried.len() >= max_retries {
                    return Ok(false);
                }
                continue;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Body forwarding helpers
// ---------------------------------------------------------------------------

/// Forward remaining request body bytes (after headers) from `client` → `upstream`.
///
/// * If `content_length` is known, copy exactly that many bytes minus what was
///   already buffered (`already_sent`).
/// * If chunked, forward raw chunked stream until the terminal `0\r\n\r\n`.
/// * If neither, assume there is no body and return immediately.
async fn forward_body(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    content_length: Option<u64>,
    is_chunked: bool,
    already_sent: u64,
) -> Result<()> {
    if let Some(len) = content_length {
        let remaining = len.saturating_sub(already_sent);
        if remaining > 0 {
            copy_exact(client, upstream, remaining).await?;
        }
    } else if is_chunked {
        copy_chunked(client, upstream).await?;
    }
    // else: no body (GET / HEAD / etc.)
    Ok(())
}

/// Forward HTTP response from `upstream` → `client`.
/// Returns `Ok(true)` if the upstream connection is being kept alive
/// (i.e. the caller may be able to issue another request on the same upstream
/// socket — we don't reuse upstream sockets here, but the value is used to
/// determine whether the *client* connection should be kept alive).
async fn forward_response(
    upstream: &mut TcpStream,
    client: &mut TcpStream,
    req_method: &str,
) -> Result<bool> {
    // Read response headers
    let header_buf = read_headers(upstream)
        .await?
        .ok_or_else(|| anyhow!("upstream closed connection without response"))?;

    let mut raw_headers = [httparse::EMPTY_HEADER; 96];
    let mut resp = httparse::Response::new(&mut raw_headers);
    let body_offset = match resp.parse(&header_buf)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => bail!("incomplete response headers from upstream"),
    };

    let status = resp.code.unwrap_or(0);
    let version = resp.version.unwrap_or(1);
    let headers = resp.headers;

    let resp_content_length: Option<u64> =
        get_header(headers, "content-length").and_then(|v| v.parse().ok());
    let resp_is_chunked = get_header(headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let upstream_keep_alive = upstream_wants_keep_alive(headers, version);

    // Forward headers verbatim
    client.write_all(&header_buf[..body_offset]).await?;

    // Forward any body bytes that were buffered with the headers
    let already_in_buf = header_buf.len().saturating_sub(body_offset);
    if already_in_buf > 0 {
        client.write_all(&header_buf[body_offset..]).await?;
    }

    // Determine whether this response has a body
    let has_body = !req_method.eq_ignore_ascii_case("HEAD")
        && status != 204
        && status != 304
        && !(100..200).contains(&status);

    if has_body {
        if let Some(len) = resp_content_length {
            let remaining = len.saturating_sub(already_in_buf as u64);
            if remaining > 0 {
                copy_exact(upstream, client, remaining).await?;
            }
        } else if resp_is_chunked {
            copy_chunked(upstream, client).await?;
        } else {
            // No Content-Length and not chunked: read until upstream closes.
            // We must also close the client connection afterwards.
            tokio::io::copy(upstream, client).await?;
            return Ok(false);
        }
    }

    Ok(upstream_keep_alive)
}

// ---------------------------------------------------------------------------
// Low-level I/O helpers
// ---------------------------------------------------------------------------

/// Read a raw chunked stream from `src` and write to `dst` until the
/// terminal `0\r\n\r\n` chunk.
async fn copy_chunked(src: &mut TcpStream, dst: &mut TcpStream) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];
    // We look for the terminal chunk marker in what we forward.
    // Since this is a relay, we forward bytes verbatim and detect the end.
    let mut trailer = Vec::new();
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await?;
        // Accumulate last 8 bytes to detect "0\r\n\r\n"
        trailer.extend_from_slice(&buf[..n]);
        if trailer.len() > 8 {
            trailer.drain(..trailer.len() - 8);
        }
        if trailer.windows(5).any(|w| w == b"0\r\n\r\n") {
            break;
        }
    }
    Ok(())
}

/// Copy exactly `bytes` bytes from `src` to `dst`.
async fn copy_exact(src: &mut TcpStream, dst: &mut TcpStream, mut bytes: u64) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];
    while bytes > 0 {
        let to_read = (buf.len() as u64).min(bytes) as usize;
        let n = src.read(&mut buf[..to_read]).await?;
        if n == 0 {
            bail!("unexpected EOF: expected {bytes} more bytes");
        }
        dst.write_all(&buf[..n]).await?;
        bytes -= n as u64;
    }
    Ok(())
}

/// Read bytes from `stream` into a growing buffer until the HTTP header
/// terminator `\r\n\r\n` is found.
///
/// Returns `Ok(None)` when the connection is closed before any bytes are read
/// (clean EOF).
async fn read_headers(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let mut first_read = true;

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if first_read {
                return Ok(None); // clean EOF between requests
            }
            bail!("connection closed mid-headers");
        }
        first_read = false;
        buf.extend_from_slice(&tmp[..n]);

        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            // Read a bit more if there are body bytes that arrived with headers
            // (common for small POST bodies). We don't loop here because
            // `forward_body` will consume the rest from the socket.
            return Ok(Some(buf));
        }

        if buf.len() > MAX_HEADER_SIZE {
            bail!("request headers exceed {MAX_HEADER_SIZE} bytes");
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response header helpers
// ---------------------------------------------------------------------------

/// Rewrite request headers for forwarding through an upstream proxy:
/// * Keeps the request line verbatim (absolute URI is already correct for a
///   proxy-to-proxy hop).
/// * Removes `Proxy-Authorization` from the client to avoid leaking
///   the client's upstream credentials to the next hop.
/// * Injects our upstream's `Proxy-Authorization` if required.
fn rewrite_request_headers(
    raw: &[u8],
    body_offset: usize,
    proxy_auth: Option<String>,
    original_path: Option<&str>,
) -> Vec<u8> {
    let header_bytes = &raw[..body_offset];
    let header_str = String::from_utf8_lossy(header_bytes);

    let mut out = String::with_capacity(header_bytes.len() + 64);
    let mut first_line = true;

    let is_direct_mode = original_path.is_some();
    for line in header_str.split("\r\n") {
        if first_line {
            if let Some(path) = original_path {
                out.push_str(&rewrite_request_line_for_direct(line, path));
            } else {
                out.push_str(line);
            }
            out.push_str("\r\n");
            first_line = false;
            continue;
        }
        if line.is_empty() {
            // Header section ends — inject upstream Proxy-Authorization if needed
            if let Some(ref auth) = proxy_auth {
                out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
            }
            out.push_str("\r\n");
            break;
        }
        // Drop the *client*'s Proxy-Authorization header (we add ours instead)
        if line
            .to_ascii_lowercase()
            .starts_with("proxy-authorization:")
        {
            continue;
        }
        if is_direct_mode && line.to_ascii_lowercase().starts_with("proxy-connection:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }

    out.into_bytes()
}

// ---------------------------------------------------------------------------
// CONNECT helpers
// ---------------------------------------------------------------------------

fn build_upstream_connect(host: &str, port: u16, proxy_auth: Option<String>) -> String {
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = proxy_auth {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    req
}

/// Read the first response line from an upstream after sending CONNECT.
/// Returns the HTTP status code.
async fn read_connect_response(upstream: &mut TcpStream) -> Result<u32> {
    let buf = read_headers(upstream)
        .await?
        .ok_or_else(|| anyhow!("upstream closed connection"))?;
    let mut raw_headers = [httparse::EMPTY_HEADER; 16];
    let mut resp = httparse::Response::new(&mut raw_headers);
    resp.parse(&buf)?;
    resp.code
        .map(|c| c as u32)
        .ok_or_else(|| anyhow!("no status code in CONNECT response"))
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

async fn connect_upstream(addr: &str) -> Result<TcpStream> {
    connect_target(addr).await
}

async fn connect_target(addr: &str) -> Result<TcpStream> {
    timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connection to {addr} timed out"))?
        .map_err(|e| anyhow!("TCP connect to {addr} failed: {e}"))
}

fn parse_connect_target(authority: &str) -> Result<(String, u16)> {
    let mut parts = authority.splitn(2, ':');
    let host = parts
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("invalid CONNECT target: {authority}"))?
        .to_string();
    let port: u16 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(443);
    Ok((host, port))
}

fn get_header<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a str> {
    headers.iter().find_map(|h| {
        if h.name.eq_ignore_ascii_case(name) {
            std::str::from_utf8(h.value).ok()
        } else {
            None
        }
    })
}

fn should_use_proxy(host: &str, policy: &DomainPolicyConfig) -> bool {
    let host_lc = host.to_ascii_lowercase();
    let matched = policy.domains.iter().any(|d| domain_matches(&host_lc, d));
    match policy.mode {
        DomainPolicyMode::Off => true,
        DomainPolicyMode::Blacklist => !matched,
        DomainPolicyMode::Whitelist => matched,
    }
}

fn domain_matches(host_lc: &str, domain_expr: &str) -> bool {
    let expr = domain_expr.trim().to_ascii_lowercase();
    if expr.is_empty() {
        return false;
    }

    if let Some(domain) = expr.strip_prefix("domain:") {
        if domain.is_empty() {
            return false;
        }
        return host_lc == domain;
    }
    if let Some(suffix) = expr.strip_prefix("suffix:") {
        if suffix.is_empty() {
            return false;
        }
        return host_matches_suffix(host_lc, suffix);
    }
    if let Some(suffix) = expr.strip_prefix("*.") {
        if suffix.is_empty() {
            return false;
        }
        return host_matches_suffix(host_lc, suffix);
    }
    if let Some(suffix) = expr.strip_prefix('.') {
        if suffix.is_empty() {
            return false;
        }
        return host_matches_suffix(host_lc, suffix);
    }

    // Backward-compatible default behavior for plain values:
    // exact domain OR subdomain suffix.
    host_lc == expr || host_matches_suffix(host_lc, &expr)
}

fn host_matches_suffix(host_lc: &str, suffix: &str) -> bool {
    if suffix.is_empty() {
        return false;
    }
    host_lc == suffix
        || host_lc
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn parse_http_target(path: &str, headers: &[httparse::Header<'_>]) -> Result<(String, u16)> {
    if let Some(rest) = path
        .strip_prefix("http://")
        .or_else(|| path.strip_prefix("https://"))
    {
        let authority = rest.split('/').next().unwrap_or(rest);
        return parse_authority_host_port(authority, 80);
    }
    let host = get_header(headers, "host").ok_or_else(|| anyhow!("missing Host header"))?;
    parse_authority_host_port(host, 80)
}

fn parse_authority_host_port(authority: &str, default_port: u16) -> Result<(String, u16)> {
    let authority = authority.trim();
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed authority: {authority}"))?;
        let host = &rest[..end];
        if host.is_empty() {
            bail!("invalid authority: {authority}");
        }

        let port = match &rest[end + 1..] {
            "" => default_port,
            suffix if suffix.starts_with(':') => suffix[1..].parse::<u16>().unwrap_or(default_port),
            _ => bail!("invalid bracketed authority: {authority}"),
        };
        return Ok((host.to_string(), port));
    }

    let colon_count = authority.matches(':').count();
    if colon_count > 1 {
        // Unbracketed IPv6 literal; treat entire authority as host with default port.
        return Ok((authority.to_string(), default_port));
    }

    let mut parts = authority.splitn(2, ':');
    let host = parts
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("invalid authority: {authority}"))?
        .to_string();
    let port = match parts.next() {
        Some("") | None => default_port,
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| anyhow!("invalid port in authority: {authority}"))?,
    };
    Ok((host, port))
}

fn rewrite_request_line_for_direct(line: &str, original_path: &str) -> String {
    let mut p = line.split_whitespace();
    let method = p.next().unwrap_or("GET");
    let fallback_target = p.next().unwrap_or(original_path);
    let version = p.next().unwrap_or("HTTP/1.1");
    let target = absolute_to_origin_form(original_path).unwrap_or(fallback_target);
    format!("{method} {target} {version}")
}

fn absolute_to_origin_form(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix("http://")
        .or_else(|| path.strip_prefix("https://"))?;
    let slash = rest.find('/').unwrap_or(rest.len());
    if slash == rest.len() {
        return Some("/");
    }
    Some(&rest[slash..])
}

fn client_wants_keep_alive(headers: &[httparse::Header<'_>], version: u8) -> bool {
    match get_header(headers, "connection").map(|v| v.to_ascii_lowercase()) {
        Some(ref v) if v.contains("close") => false,
        Some(ref v) if v.contains("keep-alive") => true,
        _ => version == 1, // HTTP/1.1 default is keep-alive
    }
}

fn upstream_wants_keep_alive(headers: &[httparse::Header<'_>], version: u8) -> bool {
    match get_header(headers, "connection").map(|v| v.to_ascii_lowercase()) {
        Some(ref v) if v.contains("close") => false,
        Some(ref v) if v.contains("keep-alive") => true,
        _ => version == 1,
    }
}

async fn write_status(stream: &mut TcpStream, code: u16, msg: &str) -> Result<()> {
    let resp = format!("HTTP/1.1 {code} {msg}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_only_uses_proxy_for_listed_domains() {
        let p = DomainPolicyConfig {
            mode: DomainPolicyMode::Whitelist,
            domains: vec!["example.com".to_string()],
        };
        assert!(should_use_proxy("api.example.com", &p));
        assert!(!should_use_proxy("google.com", &p));
    }

    #[test]
    fn blacklist_skips_proxy_for_listed_domains() {
        let p = DomainPolicyConfig {
            mode: DomainPolicyMode::Blacklist,
            domains: vec!["example.com".to_string()],
        };
        assert!(!should_use_proxy("example.com", &p));
        assert!(should_use_proxy("google.com", &p));
    }

    #[test]
    fn absolute_uri_is_rewritten_to_origin_form() {
        assert_eq!(
            rewrite_request_line_for_direct(
                "GET http://example.com/a/b?q=1 HTTP/1.1",
                "http://example.com/a/b?q=1"
            ),
            "GET /a/b?q=1 HTTP/1.1"
        );
    }

    #[test]
    fn domain_expression_supports_exact_and_suffix_variants() {
        assert!(domain_matches("api.example.com", "suffix:example.com"));
        assert!(domain_matches("api.example.com", "*.example.com"));
        assert!(domain_matches("api.example.com", ".example.com"));
        assert!(domain_matches("example.com", "domain:example.com"));
        assert!(!domain_matches("api.example.com", "domain:example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("api.example.com", "example.com"));
        assert!(!domain_matches("example.com", ""));
        assert!(!domain_matches("example.com", "domain:"));
        assert!(!domain_matches("example.com", "suffix:"));
        assert!(!domain_matches("example.com", "*."));
        assert!(!domain_matches("example.com", "."));
    }

    #[test]
    fn empty_suffix_does_not_match() {
        assert!(!host_matches_suffix("example.com", ""));
    }
}
