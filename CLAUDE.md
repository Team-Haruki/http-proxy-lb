# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

`http-proxy-lb` is an HTTP relay proxy that load-balances traffic across a pool of upstream HTTP proxies. It supports CONNECT tunneling (HTTPS), plain HTTP forwarding, passive/active health checking, hot config reload, domain-based routing policies, Prometheus metrics, and graceful shutdown. Written in Rust (edition 2021) on the Tokio async runtime.

The project is in a completed-v1 / release-ready state. Preserve existing behavior unless a task explicitly requires a change. Regressions in proxy forwarding, admin APIs, metrics, timeouts, connection limiting, or Docker deployment are high priority.

## Build and validation commands

```bash
cargo build                          # debug build
cargo build --release                # release build
cargo test                           # unit + integration tests
cargo clippy -- -D warnings          # lint (treats warnings as errors)
cargo fmt -- --check                 # format check
./scripts/docker-smoke.sh            # container smoke test
./target/debug/http-proxy-lb --config config.yaml --check  # validate config file
```

Run a single test: `cargo test <test_name>` (e.g. `cargo test round_robin_cycles_through_online`).

Integration tests (`tests/integration_smoke.rs`) spawn the actual binary and use a `TEST_MUTEX` for serialization -- they require the binary to be built first (`cargo build`).

## Architecture

All source lives in `src/` as a single binary crate. Each client connection runs in its own Tokio task. Upstream connections are established fresh per request (no upstream connection pooling).

- **`main.rs`** -- CLI (clap), startup, TCP accept loop, signal-based graceful shutdown, hot-reload loop (polls config file mtime on an interval)
- **`config.rs`** -- YAML config model (via `yaml_serde`/`serde`), loading, validation. Defines `Config`, `BalanceMode`, `DomainPolicyConfig`, `LimitsConfig`, `UpstreamConfig`
- **`upstream.rs`** -- `UpstreamPool` (mutex-guarded vec of `Arc<UpstreamEntry>`) with selection strategies (weighted round-robin, best-score, priority). `UpstreamEntry` tracks per-upstream state: online/offline (atomic), latency EMA, active connections, consecutive failures. `reload()` preserves state for URL-matched entries
- **`proxy.rs`** -- Request handler: parses HTTP/1.x via `httparse`, dispatches CONNECT tunnels or plain HTTP forwards, handles retry logic (up to `min(pool_size, 3)` attempts), passive health detection (marks upstream offline on failure), domain policy routing, keep-alive loop, body forwarding (content-length and chunked)
- **`health.rs`** -- Background active health checker: probes offline upstreams via captive HTTP request (`generate_204` through the proxy), marks them online on HTTP 204 response
- **`admin.rs`** -- Admin HTTP server with hand-rolled request parsing: `/metrics` (Prometheus text format), `/status` (JSON), `/health`. Global `Metrics` struct with atomic counters shared across the application

Key data flow: `main` accept loop -> per-connection Tokio task -> `proxy::handle_client` (keep-alive loop) -> `dispatch` -> `handle_connect` or `handle_http` -> upstream selection via `UpstreamPool::select` -> retry loop with passive health marking.

## Git commit format

All commits **must** follow `[Type] Short description` format:

- **`[Feat]`** -- New feature or capability
- **`[Fix]`** -- Bug fix
- **`[Chore]`** -- Maintenance, refactoring, dependency or build changes
- **`[Docs]`** -- Documentation-only changes

Rules: capital letter after type, imperative mood, no trailing period, ≤ ~70 chars. Agent commits must include a `Co-Authored-By` trailer.

## Development guidelines

- Make surgical, focused changes. Update tests and docs for user-visible behavior changes.
- Keep `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt -- --check` green.
- Run `./scripts/docker-smoke.sh` when touching Docker or deployment-related code.
- Integration tests cover: config validation failures, direct HTTP forwarding with real status propagation, request timeout (504), connection limiting (503), admin metrics/status counters.
- YAML config uses `yaml_serde` (not `serde_yaml`). The crate is imported as `yaml_serde` in Cargo.toml.
- No external HTTP framework -- admin server and proxy protocol handling are hand-rolled over raw TCP with `httparse` for parsing.
