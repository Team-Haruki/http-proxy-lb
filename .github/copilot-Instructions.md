# copilot-Instructions.md

## Project coding notes

- Language: Rust (edition 2021)
- Runtime: Tokio
- Proxy protocol handling is in `src/proxy.rs`
- Configuration is YAML via `yaml_serde`

## Development expectations

1. Make surgical changes that directly address the request.
2. Add/adjust tests for changed behavior.
3. Keep docs aligned with user-facing configuration changes.
4. Validate with:
   - `cargo test`
   - `cargo clippy -- -D warnings`

## Domain policy expressions

`domain_policy.domains` currently supports:

- `domain:example.com` (exact match)
- `suffix:example.com` (domain suffix match)
- `*.example.com` (suffix shorthand)
- `.example.com` (suffix shorthand)
- `example.com` (backward-compatible exact-or-suffix behavior)
