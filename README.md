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
| **Hot config reload** | Config file is re-read every `reload_interval_secs`; new/removed upstreams are applied without restart |

## Installation

```bash
git clone https://github.com/Team-Haruki/http-proxy-lb
cd http-proxy-lb
cargo build --release
# binary is at target/release/http-proxy-lb
```

## Quick start

```bash
cp config.example.yaml config.yaml
# edit config.yaml to point at your upstream proxies
./target/release/http-proxy-lb --config config.yaml
```

Set `RUST_LOG=debug` for verbose logging.

## Configuration

```yaml
# Local address to listen on
listen: "127.0.0.1:8080"

# Load-balancing mode: round_robin | best | priority
mode: round_robin

# How often to re-read the config file (seconds). 0 = disabled.
reload_interval_secs: 60

health_check:
  interval_secs: 30   # probe interval for offline upstreams
  timeout_secs: 5     # TCP-connect timeout per probe

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

## License

MIT
