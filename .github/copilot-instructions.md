# Copilot Instructions

## Project coding notes

- Language: Rust (edition 2021)
- Runtime: Tokio
- Proxy protocol handling is in `src/proxy.rs`
- Configuration is YAML via `yaml_serde`
- Admin endpoints and shared metrics are in `src/admin.rs`
- Integration coverage lives in `tests/integration_smoke.rs`
- Container smoke coverage lives in `scripts/docker-smoke.sh`

## Project status

- This project is in a completed-v1 / release-ready state.
- Default to preserving current runtime behavior unless a task explicitly asks for a change.
- Regressions in proxy forwarding, response codes, admin APIs, metrics, graceful shutdown, request timeouts, connection limiting, and Docker deployment should be treated as important.

## Development expectations

1. Make surgical changes that directly address the request.
2. Add/adjust tests for changed behavior.
3. Keep docs aligned with user-facing configuration changes.
4. Validate with:
   - `cargo test`
   - `cargo clippy -- -D warnings`
   - `cargo fmt -- --check`
   - `./scripts/docker-smoke.sh` when Docker-related or deployment behavior changes
   - `./target/debug/http-proxy-lb --config config.yaml --check` when config behavior changes

## Validation focus

- Prefer integration coverage for:
  - config validation
  - real HTTP status propagation
  - request timeout behavior
  - connection limiting behavior
  - admin metrics/status counters
- Keep README, `AGENTS.md`, and this file aligned when the project’s validation workflow changes.

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

## Domain policy expressions

`domain_policy.domains` currently supports:

- `domain:example.com` (exact match)
- `suffix:example.com` (domain suffix match)
- `*.example.com` (suffix shorthand)
- `.example.com` (suffix shorthand)
- `example.com` (backward-compatible exact-or-suffix behavior)
