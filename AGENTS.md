# Roused repository instructions

## Read first

Before editing, read `README.md`, `docs/design.md`, and `docs/mvp-plan.md` in
full. They are the authoritative handoff from discovery. Do not import an
older implementation or reactivate superseded Penny, Hyper, root-broker,
system-LaunchDaemon, container, or direct-process designs.

## Milestone discipline

- Implement only the milestone explicitly named by the user's task.
- Do not begin, partially scaffold, or “prepare for” a later milestone.
- A milestone ends when its named tests and quality gates pass, one milestone
  commit is created, and a concise report is delivered.
- Only a failure of a named acceptance test or a direct product constraint may
  add work to the active milestone. Record any other useful finding as one
  backlog sentence in the final report; it must not delay completion.
- Allow one implementation pass and one correction pass. If a third correction
  cycle would be needed, stop and report the concrete blocker and options.
- There is no general audit, broad refactor, performance study, security
  review, or documentation expansion phase. Review only code changed for the
  active milestone and its named tests.

## Platform and safety boundaries

- Target macOS only. Cross-platform abstractions are outside the MVP.
- Never use or request `sudo`, root access, `/Library/LaunchDaemons`, a
  privileged broker, or system-domain launchd operations.
- Milestone 1 must not invoke or modify launchd at all. Milestones 2 and 3 may
  use only disposable current-user LaunchAgents in `gui/$UID`.
- Never install, edit, repair, or discover a target service's plist. Roused
  consumes already-configured launchd labels, not plist paths.
- Do not inspect or operate real aria2 data, removable volumes, TCC settings,
  or the user's existing services. Use loopback fixtures.
- Do not add TLS, containers, direct child-process supervision, or raw
  one-off-command execution.

## Implementation rules

- Use Rust and the direct pinned Pingora integration described in the plan.
  Do not introduce a proxy-engine abstraction or parallel Hyper code path.
- Keep configuration strict and deterministic. Reject unknown fields and fail
  completely before listening when any entry is invalid.
- Treat request bodies and authentication material as opaque. Never log
  request or response headers at the supported INFO logging level, and never
  put credentials in source, fixtures, command arguments, or test output.
- Use dummy secrets supplied at test runtime when authentication fixtures need
  credentials.
- Preserve upstream-owned `Authorization`, cookies, challenges, and repeated
  response fields according to `docs/design.md`; explicitly remove
  `Proxy-Authorization` and the bounded hop-by-hop fields.
- A POST must result in exactly one upstream submission. Do not add automatic
  replay or retry behavior.
- Keep changes small and readable. Avoid speculative abstractions and options
  that are not in the current milestone.

## Verification and Git

- Add focused automated fixtures and tests for the active milestone.
- Run `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D
  warnings`, `cargo test --locked --all-targets`, and the named integration
  checks before completion.
- Keep `Cargo.lock` committed and dependency versions intentional.
- Preserve unrelated user changes. Do not rewrite history or use destructive
  Git commands.
- Make exactly one implementation commit for the active milestone after its
  tests pass. Do not continue into the next milestone afterward.
