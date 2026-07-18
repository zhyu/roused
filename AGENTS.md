# Roused repository instructions

## Sources of truth

- `README.md` describes the current product, operator setup, and supported
  behavior.
- `docs/design.md` is the durable protocol, configuration, lifecycle, and
  security contract. Read it in full before changing any of those areas.
- `docs/mvp-plan.md` is the historical delivery and acceptance record. It is
  not an active milestone queue or a roadmap for more work.

Before editing, read the README and the parts of the implementation and tests
relevant to the request. Do not revive superseded proxy engines, privileged
brokers, system LaunchDaemons, containers, or direct target-process designs.

## Scope discipline

- Implement only the requested change and the focused tests or documentation
  needed to establish it.
- Do not add speculative features, future scaffolding, unrelated refactors, or
  broad audits unless the user explicitly requests them.
- Preserve established behavior unless the task deliberately changes the
  product contract.
- Unsupported features may be introduced only by an explicit product task,
  not incidentally. Update `docs/design.md`, the README, and focused tests when
  a contract or boundary changes.
- Record useful unrelated findings without expanding the active task.

## Platform and test safety

- Target macOS only. Cross-platform abstractions are outside the current
  product contract.
- Never use or request `sudo`, root access, `/Library/LaunchDaemons`, a
  privileged broker, or system-domain launchd operations.
- Runtime lifecycle operations and integration tests may address only
  current-user jobs in `gui/$UID`.
- Use loopback fixtures and uniquely labeled disposable LaunchAgents in tests.
  Never inspect or operate the user's existing services, plists, application
  data, volumes, or privacy settings.
- Roused consumes already-configured launchd labels. Do not add target plist
  paths, discovery, installation, inspection, editing, or repair behavior.

## Implementation invariants

- Use Rust and the direct pinned Pingora 0.8.1 integration. Do not add a proxy
  engine abstraction or parallel implementation without an explicit redesign.
- Keep configuration strict, atomic, and deterministic. Unknown fields or any
  invalid service must reject the complete configuration before listening.
- Treat request bodies and authentication material as opaque. Never log
  request or response headers at the supported INFO level, and never put
  credentials in source, fixtures, command arguments, or test output.
- Supply dummy authentication values at test runtime when fixtures need them.
- Preserve upstream-owned `Authorization`, cookies, challenges, and repeated
  response fields according to `docs/design.md`. Explicitly remove
  `Proxy-Authorization` and the bounded hop-by-hop fields.
- Keep exactly one total upstream attempt. A POST must produce exactly one
  upstream submission; do not add automatic replay or retry behavior.
- Keep launchd commands in the current-user domain and label-based. Target
  shutdown remains best-effort `SIGTERM`, without PID discovery, drain logic,
  or escalation.
- Execute `can_stop_command` as direct literal argv, never through a shell, and
  never log its arguments.
- Keep changes small and readable. Avoid abstractions or options that the task
  does not require.

## Verification, documentation, and Git

- Add focused automated regression coverage for behavior changes. Reuse the
  loopback and disposable-current-user fixtures in `tests/`.
- Run all of these before completion:

  ```sh
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets -- -D warnings
  cargo test --locked --all-targets
  ```

- The full integration suite requires macOS with a logged-in GUI user domain
  and includes a WebSocket check lasting more than 60 seconds.
- Keep `Cargo.lock` committed and dependency versions intentional.
- Keep operator-visible behavior and limitations in the README synchronized
  with code; update `docs/design.md` only for deliberate contract changes.
- Preserve unrelated user changes. Do not rewrite history or use destructive
  Git commands.
- If the task calls for a commit, create one focused commit only after the
  required checks pass, and do not include unrelated changes.
