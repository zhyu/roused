# Bounded MVP plan

The MVP has exactly three implementation milestones. Each milestone is a
separate Codex task and ends with its named tests, one commit, and a report.
It must not silently continue into the next milestone.

## Milestone 1 — Proxy

Build only a reverse proxy for already-running loopback fixtures.

### Required implementation

1. Pin Pingora exactly as specified in `docs/design.md` and commit
   `Cargo.lock`.
2. Parse the static TOML configuration and reject the invalid forms in the
   configuration contract before binding the listener.
3. Listen on a configurable unprivileged HTTP/1.1 address.
4. Normalize the routing host case-insensitively, removing an optional port
   and terminal dot, while forwarding the original `Host` value upstream.
5. Permit only static loopback HTTP upstreams and route at least two configured
   hosts to distinct fixtures.
6. Proxy GET, POST, path/query, status, end-to-end headers, and streaming
   request/response bodies according to the bounded header policy in
   `docs/design.md`.
7. Configure exactly one total upstream attempt and prove one POST produces
   exactly one upstream submission.
8. Add an authentication fixture covering Basic and Bearer `Authorization`,
   `Cookie`, `401` plus `WWW-Authenticate`, `Authentication-Info`, repeated
   `Set-Cookie`, `Location`, original `Host`, and removal of
   `Proxy-Authorization`.
9. Support Pingora's native WebSocket upgrade. A cookie-authenticated echo
   socket must remain open while otherwise idle for more than 60 seconds.
10. Keep supported dependency logging at INFO or less verbose and do not log
    request or response headers.
11. Return one deterministic documented response for an unknown hostname.

### Milestone 1 acceptance

Milestone 1 is complete only when automated or reproducible integration checks
prove all of the following:

- two configured hostnames reach two distinct already-running fixtures;
- mixed-case hosts, a terminal dot, and a listener port route correctly while
  the upstream observes the unmodified original `Host`;
- an unknown host receives the documented deterministic response;
- GET and POST methods, path/query, status, headers, and bodies survive;
- request and response bodies are streamed rather than fully buffered;
- the complete authentication-transparency fixture passes, including separate
  repeated `Set-Cookie` fields and removal of `Proxy-Authorization`;
- one POST causes exactly one upstream submission;
- a WebSocket handshake requiring a cookie succeeds, echoes data, and stays
  connected for more than 60 idle seconds;
- invalid configurations fail before the listener starts; and
- formatting, Clippy with warnings denied, locked tests, and all Milestone 1
  integration checks pass.

### Hard stop after Milestone 1

Do not add TCP readiness checks, loading responses, launchctl calls,
LaunchAgent fixtures or plists, startup deduplication/cooldowns, request
leases, idle timers, stop-check commands, signals, gateway packaging, or any
Milestone 2/3 scaffolding. Stop after the proxy tests pass against
already-running fixtures, create the single Milestone 1 commit, and report.

## Milestone 2 — Wake

Only after a separate user request:

- add short TCP readiness checks;
- deduplicate per-service current-user `launchctl kickstart` calls;
- use fixed five-second launch-command timeout and attempt cooldown;
- return small no-store `503` HTML with refresh for safe HTML GET/HEAD and
  fixed no-store `503` JSON for other requests;
- never buffer or replay the cold request, and prevent connection reuse when
  an unread cold body remains; and
- verify twenty concurrent cold requests plus refreshes yield one kickstart
  and a later retry reaches one disposable user LaunchAgent, without sudo.

## Milestone 3 — Sleep and package

Only after a separate user request:

- track in-flight requests and last-completed activity through response EOF,
  disconnect, and WebSocket close;
- enforce configurable idle timeout;
- run optional absolute argv `can_stop_command` directly with a fixed
  five-second timeout, where only exit 0 permits stopping and vetoes retry no
  more often than every 30 seconds;
- atomically recheck activity before best-effort user-domain SIGTERM;
- add gateway and target current-user LaunchAgent templates, restart behavior,
  and concise operating documentation; and
- finish the complete MVP acceptance suite, commit, report, and stop. There is
  no fourth milestone.

## Complete MVP acceptance suite

The later milestones extend, rather than replace, Milestone 1 tests. The final
suite must establish:

1. Host routing, normalization, original Host preservation, and deterministic
   unknown-host behavior for two services.
2. GET/POST, query, status, body, authentication transparency, repeated
   response fields, removal of `Proxy-Authorization`, and exactly one upstream
   POST submission.
3. Twenty concurrent cold requests and repeated refreshes cause one kickstart;
   failed/timed-out launch is deterministic and retryable after cooldown.
4. HTML navigation and API/cold-POST clients receive their respective `503`
   loading responses with `Retry-After` and `Cache-Control: no-store`; a cold
   body is neither replayed nor left on a reusable connection.
5. A later retry reaches the target after TCP readiness.
6. A response longer than idle timeout prevents stop until EOF or disconnect.
7. A cookie-authenticated WebSocket remains open beyond 60 idle seconds and
   prevents stop until close.
8. Idle expiry delivers SIGTERM, and a later request wakes and reaches the
   fixture again.
9. Stop-check exit 1 vetoes stopping and later exit 0 permits it.
10. A timed-out, missing, or failing stop check keeps the service running.
11. Gateway and target run in `gui/$UID` without sudo or `/Library` writes;
    their launchd properties and gateway/target restart behavior match the
    design.
12. Invalid config, format, Clippy, locked tests, operating documentation, and
    clear warnings about plain-HTTP credentials, Secure cookies, and unsafe
    dependency DEBUG/TRACE header logging all pass.

## Runaway-prevention rules

1. Only a named acceptance failure or explicit MVP constraint adds work to the
   active milestone.
2. Any other finding becomes one backlog sentence and cannot delay the commit.
3. After one implementation pass and one correction pass, a further required
   correction stops the task for a concrete blocker/options report.
4. Root, sudo, a system LaunchDaemon, or asking the user to run privileged
   commands is an immediate scope violation.
5. Review is limited to touched code and named tests; no broad audit or
   refactor phase exists.
6. Each milestone stops after its commit and report. Never continue
   automatically.
