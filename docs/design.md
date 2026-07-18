# Roused MVP design

Date fixed: 2026-07-18

## Product contract

Roused is an HTTP-aware activation proxy for native services on one macOS home
server. A request to a configured hostname will eventually:

1. reach a small always-available gateway;
2. wake an already-bootstrapped current-user LaunchAgent if its loopback port
   is unavailable;
3. receive a temporary loading response during startup;
4. be proxied once the target accepts TCP connections; and
5. allow the target to receive SIGTERM after the service is idle and an
   optional external command says stopping is safe.

The three implementation milestones intentionally build this vertical slice
in order. Milestone 1 is only the proxy for already-running fixtures.

## Fixed technology decision

Use one direct implementation against exactly Pingora 0.8.1:

```toml
pingora = { version = "=0.8.1", default-features = false, features = ["proxy"] }
```

Commit `Cargo.lock`. Do not build a proxy-engine abstraction or maintain a
parallel Hyper implementation. The completed macOS spike passed ordinary
HTTP, streaming responses, client aborts, and a WebSocket echo/lifetime check.
Pingora also passed the local SSE experiment, but an upstream macOS SSE report
remains open; SSE is therefore not supported or investigated by this MVP.

The gateway exposes configurable, unprivileged plain HTTP/1.1. There is no
port 80 requirement, TLS provider, certificate handling, HTTP/2, or HTTP/3.

## Configuration contract

Configuration is static TOML and is validated atomically before the listener
starts. The target shape is:

```toml
listen = "0.0.0.0:8080"

[[services]]
host = "aria2.apps.home.arpa"
upstream = "127.0.0.1:6800"
launchd_label = "net.example.aria2"
idle_timeout_seconds = 1800
can_stop_command = ["/absolute/path/to/aria2-can-stop"]
```

`can_stop_command` is optional and the idle timeout defaults to 1,800 seconds.
Reject unknown keys, duplicate normalized hosts, duplicate labels or
upstreams, non-loopback upstreams, invalid launchd labels, zero idle timeouts,
and an empty command or non-absolute command executable. One service entry
represents one host, launchd job, and upstream; aliases are not supported.

The lifecycle fields are parsed in Milestone 1 so the configuration format is
stable, but they must not trigger launchd or idle behavior until their named
milestones.

## Routing and proxy semantics

Route by the configured DNS host after ASCII case folding and removal of one
optional listener port and a terminal dot. Preserve the request's original
`Host` value upstream. All upstreams are static loopback HTTP addresses.
Unknown hosts receive one deterministic documented response.

The transparency contract is semantic HTTP transparency, not byte-for-byte
wire identity:

- preserve request method, URI, query, origin-facing `Authorization`, `Cookie`,
  original `Host`, other end-to-end fields, and streaming body;
- preserve upstream status, `WWW-Authenticate`, `Authentication-Info`,
  repeated `Set-Cookie` fields, `Location`, other end-to-end fields, and
  streaming body;
- remove `Proxy-Authorization`, because it authenticates to an intermediary;
- for ordinary HTTP, remove `Connection`, every field it names,
  `Proxy-Connection`, `Keep-Alive`, `Proxy-Authenticate`, `TE`, `Trailer`,
  `Transfer-Encoding`, and `Upgrade`, allowing Pingora to regenerate framing;
- for a valid WebSocket upgrade, retain the `Connection` and `Upgrade` fields
  required by Pingora's native upgrade path; and
- configure and regression-test exactly one total upstream attempt so a POST
  or JSON-RPC call is never submitted twice.

The gateway has no authentication layer of its own. Basic, Bearer, cookie, and
body-token authentication belong to the upstream and must pass through. Never
log request or response headers at supported INFO logging. Pingora dependency
DEBUG/TRACE may expose headers and must not be enabled with real credentials.

Plain HTTP leaves credentials unencrypted on the LAN. Preserve `Secure` cookie
attributes rather than weakening or rewriting them; services requiring HTTPS
or Secure cookies are outside the MVP.

## WebSocket contract

Use Pingora's native WebSocket path. For a successful upgrade, remove the
ordinary downstream read timeout so an otherwise idle connection survives
longer than 60 seconds. The MVP adds no response-body, streaming, or WebSocket
idle timeout. SSE has no support guarantee.

## Launchd and lifecycle contract

Lifecycle work starts only in Milestone 2. Targets are existing,
already-bootstrapped jobs in `gui/$UID`, with `RunAtLoad=false` and
`KeepAlive=false`. Configuration contains their labels, never plist paths.
Roused does not install, edit, inspect, or repair target jobs.

Milestone 2 uses a short loopback TCP connect for readiness and a deduplicated
`/bin/launchctl kickstart gui/$UID/<label>` without `-k`. Milestone 3 accounts
for in-flight work and sends best-effort
`/bin/launchctl kill SIGTERM gui/$UID/<label>` only after idle policy and an
optional external check permit it. The external check is argv executed
directly, never through a shell; Roused contains no aria2-specific logic.

The gateway itself is eventually packaged as a current-user LaunchAgent with
`RunAtLoad=true` and `KeepAlive=true`. The MVP never uses root, sudo, a
privileged broker, `/Library/LaunchDaemons`, or pre-login operation.

## Explicit non-goals

- containers, Apple container support, direct process supervision, and
  one-off commands;
- real aria2, removable-volume, TCC, or disk-throttling validation;
- installing, editing, discovering, or repairing target plists;
- native HTTP/JSON-RPC stop-check adapters;
- SSE support or investigation;
- config reload, persistence, aliases, dashboard, admin API, or metrics;
- gateway-owned authentication, TLS, certificate management, or public access;
- rate limits, body-size or CORS policy, `Forwarded`/`X-Forwarded-*`
  synthesis, and cookie or redirect rewriting;
- graceful drain protocols, SIGKILL escalation, PID discovery, and repair of
  a target's restart policy; and
- broad launchd, security, performance, or proxy-conformance audits.

## Research notes

The Pingora spike was verified on Apple-silicon macOS with Rust/Cargo 1.97.1
and CMake 4.4.0. CMake is a build-time requirement caused by Pingora's native
dependency graph; it is not required merely to run the resulting executable.
Relevant upstream references include the
[Pingora repository](https://github.com/cloudflare/pingora) and its open
[macOS SSE report](https://github.com/cloudflare/pingora/issues/841).
