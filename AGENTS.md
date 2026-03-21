# AGENTS.md

## Purpose

This repository contains `http-proxy-lb`, a Rust HTTP relay proxy with upstream load balancing, health checks, hot reload, and domain policy routing.

## Scope

- Keep code changes minimal and focused.
- Prefer targeted tests first, then full validation.
- Preserve existing behavior unless the task explicitly requires a change.

## Local validation

- Run tests: `cargo test`
- Run lint checks: `cargo clippy -- -D warnings`
- Format code: `cargo fmt`

## Code structure

- `src/config.rs`: configuration model and YAML loading
- `src/upstream.rs`: upstream entry state + pool selection/reload
- `src/health.rs`: active health checking logic
- `src/proxy.rs`: CONNECT and HTTP forwarding logic
- `src/main.rs`: startup, accept loop, background tasks
