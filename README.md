# Roused

Roused is a macOS-first activation reverse proxy for lightly used home-server
web services. It will route requests by hostname, wake an existing
current-user LaunchAgent when necessary, proxy the service while it is active,
and eventually stop it after a safe idle period.

The project is deliberately narrow: one Apple-silicon Mac, native launchd
jobs, plain HTTP on a trusted home network, and no container runtime. The
gateway does not authenticate clients, but it must transparently preserve
authentication owned by upstream services.

## Current status

Milestone 2 adds request-time loopback readiness checks, deduplicated wake-up
of configured current-user LaunchAgents, and deterministic loading responses
to the Milestone 1 reverse proxy. It does not implement idle accounting, stop
checks, stopping, or gateway LaunchAgent packaging.

Read these before changing code:

- [`AGENTS.md`](./AGENTS.md) — repository working rules and scope controls.
- [`docs/design.md`](./docs/design.md) — stable product and protocol decisions.
- [`docs/mvp-plan.md`](./docs/mvp-plan.md) — the three bounded milestones and
  their acceptance criteria.

## Selected stack

- Rust, with `Cargo.lock` committed.
- Pingora `0.8.1`, pinned exactly, with default features disabled and only the
  `proxy` feature enabled.
- HTTP/1.1 and Pingora-native WebSocket proxying; no TLS provider and no SSE
  support contract.
- Current-user LaunchAgents in `gui/$UID`; never a privileged broker or a
  system LaunchDaemon.

The discovery spike was last verified on macOS arm64 with Rust/Cargo 1.97.1
and CMake 4.4.0. Those are a recorded baseline, not a request to upgrade tools.

## Quality gates

Every implementation milestone must pass:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Milestone-specific integration tests are defined in
[`docs/mvp-plan.md`](./docs/mvp-plan.md).

## Run the Milestone 2 proxy

Pass exactly one static TOML configuration path:

```sh
cargo run --locked -- /absolute/path/to/roused.toml
```

The file uses the configuration shape in [`docs/design.md`](./docs/design.md).
Roused validates the complete file before binding its configured listener. An
unconfigured or malformed request `Host` receives this deterministic response:

- status: `421 Misdirected Request`
- `Content-Type: text/plain; charset=utf-8`
- `Cache-Control: no-store`
- body: `unknown host\n`

Logging is capped at INFO and does not include request or response headers.
Dependency DEBUG/TRACE logging is unsupported because it can expose those
headers. Plain HTTP credentials remain unencrypted on the network, and Roused
preserves (rather than weakens) upstream `Secure` cookie attributes.
