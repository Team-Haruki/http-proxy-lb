# AGENTS.md

## Purpose

This repository contains `http-proxy-lb`, a Rust HTTP relay proxy with upstream load balancing, health checks, hot reload, domain policy routing, admin endpoints, Prometheus metrics, graceful shutdown, request limits, and container deployment assets.

## Status

- The repository is in a near-release / completed-v1 state.
- Prefer preserving the current behavior and validation baseline unless the task explicitly requires a change.
- Treat regressions in proxy behavior, admin endpoints, metrics, timeout handling, connection limiting, or Docker deployment as high priority.

## Scope

- Keep code changes minimal and focused.
- Prefer targeted tests first, then full validation.
- Preserve existing behavior unless the task explicitly requires a change.

## Local validation

- Run tests: `cargo test`
- Run lint checks: `cargo clippy -- -D warnings`
- Format code: `cargo fmt -- --check`
- Run container smoke test: `./scripts/docker-smoke.sh`
- Validate config file: `./target/debug/http-proxy-lb --config config.yaml --check`

## Code structure

- `src/config.rs`: configuration model and YAML loading
- `src/admin.rs`: admin server, `/metrics`, `/status`, `/health`, and shared metrics
- `src/upstream.rs`: upstream entry state + pool selection/reload
- `src/health.rs`: active health checking logic
- `src/proxy.rs`: CONNECT and HTTP forwarding logic
- `src/main.rs`: startup, accept loop, background tasks
- `tests/integration_smoke.rs`: end-to-end integration tests for config validation, timeouts, metrics, and connection limits
- `scripts/docker-smoke.sh`: local Docker smoke test helper

## Git commit format

All commits **must** follow:

```
[Type] Short description starting with capital letter
```

| Type      | Usage                                                 |
|-----------|-------------------------------------------------------|
| `[Feat]`  | New feature or capability                             |
| `[Fix]`   | Bug fix                                               |
| `[Chore]` | Maintenance, refactoring, dependency or build changes |
| `[Docs]`  | Documentation-only changes                            |

Rules:

- Description starts with a **capital letter**.
- Imperative mood (`Add ...`, not `Added ...`).
- No trailing period.
- Keep subject ≤ ~70 chars.
- Agent commits must include a `Co-Authored-By` trailer to attribute the agent.

## Expectations

- Keep changes minimal and targeted.
- Update tests and docs for any user-visible behavior change.
- Prefer keeping `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`, and `./scripts/docker-smoke.sh` green before considering work complete.
