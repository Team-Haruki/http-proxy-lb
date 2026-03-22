# http-proxy-lb

A high-availability HTTP relay proxy with upstream load balancing, health checking and hot config reload — written in Rust.

## Features

| Feature | Description |
|---|---|
| **Round-robin load balancing** | Weighted round-robin across all online upstream proxies |
| **Best-selection mode** | Always routes to the upstream with the lowest latency + connection score |
| **Priority mode** | Routes to the highest-priority online upstream and automatically falls back when it is offline |
| **Passive health detection** | Any upstream that fails to connect or respond is immediately marked offline and excluded from routing |
| **Active health checking** | Background task probes offline upstreams using a captive-style HTTP probe (`generate_204`) through the proxy; marks them online on HTTP 204 |
| **Automatic failover** | Failed requests are retried on a different upstream (up to `min(pool_size, 3)` retries) |
| **HTTP/1.1 keep-alive** | Client connections are reused across requests |
| **HTTPS tunneling** | `CONNECT` method is fully supported — traffic is tunneled through the upstream proxy |
| **Upstream authentication** | Per-upstream HTTP Proxy Basic Auth (`Proxy-Authorization`) |
| **Domain blacklist/whitelist routing** | Supports selective direct/proxy routing by domain (`domain_policy`) |
| **Hot config reload** | Config file is re-read every `reload_interval_secs`; new/removed upstreams are applied without restart |
| **Prometheus metrics** | `/metrics` endpoint for monitoring with Prometheus |
| **Admin API** | `/status` JSON endpoint and `/health` health check |
| **Graceful shutdown** | Handles SIGTERM/SIGINT, waits for active connections to complete |
| **Connection limiting** | Optional max concurrent connections and request timeout |
| **Access logging** | Optional structured access logs for each request |

## Installation

```bash
git clone https://github.com/Team-Haruki/http-proxy-lb
cd http-proxy-lb
cargo build --release
# binary is at target/release/http-proxy-lb
```

### Docker

```bash
# Build image
docker build -t http-proxy-lb .

# Run with config file
docker run -d \
  -p 8080:8080 \
  -p 9090:9090 \
  -v $(pwd)/config.yaml:/etc/http-proxy-lb/config.yaml:ro \
  http-proxy-lb
```

Or use Docker Compose:

```bash
cp config.example.yaml config.yaml
# edit config.yaml
docker compose up -d
```

## Quick start

```bash
cp config.example.yaml config.yaml
# edit config.yaml to point at your upstream proxies
./target/release/http-proxy-lb --config config.yaml
```

Set `RUST_LOG=debug` for verbose logging.

### Validate configuration

```bash
./target/release/http-proxy-lb --config config.yaml --check
```

## Validation

```bash
# unit + integration tests
cargo test

# lint
cargo clippy -- -D warnings

# formatting
cargo fmt -- --check

# container smoke test
./scripts/docker-smoke.sh
```

The integration test suite exercises:

* config validation failure paths
* direct HTTP forwarding with real response-status propagation
* request timeout handling (`504 Gateway Timeout`)
* connection limiting (`503 Service Unavailable`)
* admin metrics/status byte counters and request counters

## Configuration

```yaml
# Local address to listen on
listen: "127.0.0.1:8080"

# Admin server for /metrics and /status endpoints (optional)
admin_listen: "127.0.0.1:9090"

# Load-balancing mode: round_robin | best | priority
mode: round_robin

# How often to re-read the config file (seconds). 0 = disabled.
reload_interval_secs: 60

# Enable access logging
access_log: false

health_check:
  interval_secs: 30   # probe interval for offline upstreams
  timeout_secs: 5     # TCP-connect timeout per probe

# Resource limits (0 = unlimited)
limits:
  max_connections: 0        # max concurrent connections
  request_timeout_secs: 0   # request timeout
  shutdown_timeout_secs: 30 # graceful shutdown timeout

domain_policy:
  mode: off           # off | blacklist | whitelist
  domains: []         # supports domain:, suffix:, *., . shorthand

upstream:
  - url: "http://proxy1.example.com:8080"
    weight: 1
    priority: 10

  - url: "http://proxy2.example.com:8080"
    weight: 2
    priority: 20
    username: "user"
    password: "secret"
```

### Balance modes

* **`round_robin`** — Iterates through online upstreams in weighted order.  Upstreams with a higher `weight` receive proportionally more connections.
* **`best`** — Selects the online upstream with the lowest *score*, where:
  `score = latency_ema_ms + active_connections × 50`
  Latency is an exponential moving average (α = 0.25) of observed response times.
* **`priority`** — Selects the online upstream with the lowest `priority` value. When the current highest-priority upstream becomes offline, routing automatically switches to the next online priority level; recovered upstreams are reused after active health checks mark them online.

### Hot reload

Edit `config.yaml` while the proxy is running.  Within `reload_interval_secs` seconds the proxy will pick up the new upstream list.  Existing upstreams (matched by URL) keep their online/offline state and statistics.

### Domain policy

`domain_policy` controls whether specific domains should use upstream proxy or direct connection:

* **`off`** (default): all domains use upstream proxy.
* **`blacklist`**: domains listed in `domains` bypass upstream proxy and connect directly; other domains still use upstream proxy.
* **`whitelist`**: only domains listed in `domains` use upstream proxy; other domains connect directly.

Expression formats in `domains`:

* `domain:example.com` — exact domain match
* `suffix:example.com` — suffix match (`example.com` and `*.example.com`)
* `*.example.com` / `.example.com` — suffix shorthand
* `example.com` — backward-compatible exact-or-suffix match

## Monitoring

When `admin_listen` is configured, the following endpoints are available:

### GET /metrics

Prometheus-compatible metrics:

```
http_proxy_lb_uptime_seconds 3600
http_proxy_lb_requests_total 150000
http_proxy_lb_requests_success 149500
http_proxy_lb_requests_failed 500
http_proxy_lb_active_connections 42
http_proxy_lb_upstream_online{url="http://proxy1:8080"} 1
http_proxy_lb_upstream_latency_ms{url="http://proxy1:8080"} 23
```

### GET /status

JSON status of all upstreams:

```json
{
  "uptime_seconds": 3600,
  "requests_total": 150000,
  "requests_success": 149500,
  "active_connections": 42,
  "upstreams": [
    {
      "url": "http://proxy1:8080",
      "online": true,
      "active_connections": 20,
      "latency_ms": 23,
      "weight": 1,
      "priority": 10
    }
  ]
}
```

### GET /health

Simple health check endpoint (returns `{"status":"ok"}`).

## Usage with curl

```bash
# Plain HTTP
curl -x http://127.0.0.1:8080 http://example.com/

# HTTPS (CONNECT tunnel)
curl -x http://127.0.0.1:8080 https://example.com/
```

## Architecture

```
Client ──► [http-proxy-lb listener]
                │
                ▼
         [UpstreamPool]  ◄── [Active health checker]
         (round-robin /
           best select)
                │
       ┌────────┼────────┐
       ▼        ▼        ▼
  Upstream1  Upstream2  Upstream3
  (online)  (offline)  (online)
```

Each client connection is handled in its own Tokio task.  Upstream connections are established fresh per request (no upstream connection pooling).

## Deployment

### Systemd

Copy the binary and service file:

```bash
sudo cp target/release/http-proxy-lb /usr/local/bin/
sudo cp http-proxy-lb.service /etc/systemd/system/
sudo mkdir -p /etc/http-proxy-lb
sudo cp config.yaml /etc/http-proxy-lb/
sudo useradd -r -s /bin/false proxy
sudo systemctl daemon-reload
sudo systemctl enable --now http-proxy-lb
```

### Docker Compose with Prometheus

```bash
docker compose --profile monitoring up -d
```

For a local container smoke test without touching your own config, run:

```bash
./scripts/docker-smoke.sh
```

## License

MIT
