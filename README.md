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

This repository contains only the clean Rust scaffold and the design handoff.
No product implementation has begun. The next task is **Milestone 1: Proxy**;
it must stop before any launchd wake/sleep work.

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
