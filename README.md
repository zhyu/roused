# Roused

Roused is a macOS activation reverse proxy for lightly used home-server web
services. It routes plain HTTP/1.1 requests by hostname, wakes an existing
current-user LaunchAgent when needed, proxies the service while it is active,
and stops it after a safe idle period.

The three planned milestones are complete. This is the complete bounded MVP;
there is no Milestone 4.

Read these before changing code:

- [`AGENTS.md`](./AGENTS.md) — repository working rules and scope controls.
- [`docs/design.md`](./docs/design.md) — stable product and protocol decisions.
- [`docs/mvp-plan.md`](./docs/mvp-plan.md) — the three bounded milestones and
  their acceptance criteria.

## Build and run

Roused targets macOS and uses Rust with exactly Pingora 0.8.1. Build the
committed dependency graph and run the foreground gateway with one static TOML
path:

```sh
cargo build --locked --release
./target/release/roused /absolute/path/to/roused.toml
```

For a development build, the equivalent command is:

```sh
cargo run --locked -- /absolute/path/to/roused.toml
```

CMake is needed to build Pingora's native dependencies, but is not needed
merely to run the resulting executable. The recorded discovery baseline was
macOS arm64, Rust/Cargo 1.97.1, and CMake 4.4.0; it is not an upgrade request.

Roused validates the entire configuration before binding the listener. The
configuration is not reloaded while the process is running.

## Static configuration

```toml
listen = "0.0.0.0:8080"

[[services]]
host = "service.apps.home.arpa"
upstream = "127.0.0.1:9000"
launchd_label = "net.example.service"
idle_timeout_seconds = 1800
can_stop_command = ["/absolute/path/to/can-stop", "--literal-argument"]
```

`listen` must be a fixed unprivileged address. Each service maps one DNS host
to a distinct loopback HTTP upstream and current-user launchd label.
`idle_timeout_seconds` defaults to 1,800 and must be nonzero.
`can_stop_command` is optional. Unknown fields, duplicate normalized hosts,
labels or upstreams, non-loopback upstreams, invalid labels, and malformed
stop commands reject the complete configuration before listening.

Every target must already be configured and bootstrapped as a LaunchAgent in
`gui/$UID`. Its `Label` must equal `launchd_label`, it must listen on the
configured loopback `upstream`, and it must use `RunAtLoad=false` and
`KeepAlive=false`. Roused consumes labels only: it never installs, edits,
discovers, inspects, repairs, or accepts the path of a target plist.

## Current-user LaunchAgent templates

The templates in [`packaging/launchd`](./packaging/launchd) are starting
points, not an installer:

- [`roused-gateway.plist`](./packaging/launchd/roused-gateway.plist) has
  `RunAtLoad=true` and `KeepAlive=true`.
- [`roused-target.plist`](./packaging/launchd/roused-target.plist) has
  `RunAtLoad=false` and `KeepAlive=false`.

Copy each template to the current user's `~/Library/LaunchAgents`, replace its
example label and every `/ABSOLUTE/PATH/...` value, and validate it before
bootstrap. launchd does not perform shell, variable, or `~` expansion inside
`ProgramArguments`; the executable and configuration paths must be absolute,
and each argument is a separate array element.

For a target that is not already bootstrapped:

```sh
cp packaging/launchd/roused-target.plist \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
# Edit the copy, then check that Label matches launchd_label in roused.toml.
/usr/bin/plutil -lint \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
/bin/launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
```

Bootstrap loads the target definition but does not start it. Roused later
uses the configured label to start it.

Install the gateway after building the release executable and validating its
configuration interactively:

```sh
cp packaging/launchd/roused-gateway.plist \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
# Edit the copy to contain the absolute release-binary and TOML paths.
/usr/bin/plutil -lint \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
/bin/launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
```

`RunAtLoad` starts the gateway and `KeepAlive` restarts it if it exits. To
unload the example jobs, address only their current-user service targets:

```sh
/bin/launchctl bootout "gui/$(id -u)/net.example.roused"
/bin/launchctl bootout "gui/$(id -u)/net.example.service"
```

These operations require neither `sudo` nor a write under `/Library`. Roused
does not support a system LaunchDaemon, root operation, or pre-login service.

## Request and lifecycle behavior

For each configured request, Roused first checks whether the loopback upstream
accepts TCP connections. A ready service is proxied immediately, including a
service that was already running when the gateway started; no kickstart is
issued. Proxying preserves upstream-owned authentication, cookies, challenges,
repeated response fields, request and response streaming, and WebSocket
upgrades, while permitting only one total upstream attempt.

If the upstream is not ready, Roused deduplicates
`/bin/launchctl kickstart gui/$UID/<launchd_label>` attempts. The cold request
is never buffered or replayed. Safe HTML GET or HEAD navigation receives a
small no-store `503` loading page; other requests receive a no-store JSON
`503`. Both carry `Retry-After: 5`, and a connection with an unread cold body
is not reused. A later client request is proxied once readiness succeeds.

An unknown or malformed `Host` receives this deterministic response:

- status: `421 Misdirected Request`
- `Content-Type: text/plain; charset=utf-8`
- `Cache-Control: no-store`
- body: `unknown host\n`

Only a ready service request that will actually be proxied acquires a service
lease. Loading and unknown-host responses do not. The lease covers the full
upload and response body and remains held until response EOF, a fatal error,
client disconnect, or an upgraded WebSocket closing. Long uploads, downloads,
responses, and WebSockets therefore prevent shutdown while active.

Releasing the lease records the last completed activity and advances that
service's idle generation. Roused considers a stop only after the configured
idle interval has elapsed since the last completed proxied request and the
service has no in-flight requests. A wake request or newly proxied request
invalidates an obsolete pending stop.

Each service's idle clock starts at the current time when the gateway starts.
Lifecycle state is not persisted. An already-running target consequently gets
a fresh idle grace period after a gateway start or restart and remains
immediately reachable without a new kickstart.

## External stop checks and shutdown

When `can_stop_command` is configured, Roused executes its first element as
the executable and passes the remaining elements as literal argv using Rust's
direct `Command` interface. It never invokes a shell: expansion, quoting,
pipelines, redirection, and shell operators are not interpreted. The
executable must be an absolute path.

The command has a fixed five-second timeout. Only exit status 0 permits a
stop. A nonzero exit, timeout, missing executable, spawn error, or any other
execution failure conservatively keeps the target running. While the service
otherwise remains idle, a vetoed or failed check is retried no more often than
once every 30 seconds. With no configured check, the in-flight and idle rules
alone permit stopping. Checker argv is never logged.

After a successful check, Roused atomically rechecks the service's in-flight
count and idle generation. Activity that arrived while the check ran cancels
that stop attempt. If the recheck still permits shutdown, Roused makes the
best-effort call:

```text
/bin/launchctl kill SIGTERM gui/$UID/<launchd_label>
```

This signals but does not unload the target LaunchAgent. With
`KeepAlive=false`, the target remains stopped until a later request causes a
new kickstart. There is no graceful-drain protocol, SIGKILL escalation, PID
discovery, or restart-policy repair.

The gateway template's `KeepAlive=true` makes launchd restart a killed
gateway. Restarting the gateway does not restart an already-running target;
the target is reached directly with a new in-memory idle grace period and no
kickstart. No request, wake, idle, or stop-check state survives the restart.

## Security limits

Roused provides no gateway authentication and no TLS. Plain HTTP exposes
Basic, Bearer, cookie, and body-token credentials in cleartext to the LAN, so
the gateway is suitable only for a deliberately trusted private network and
must not be exposed publicly.

Roused preserves upstream `Secure` cookie attributes rather than weakening or
rewriting them. Browsers generally will not store or send a `Secure` cookie
over this cleartext HTTP gateway, so services that require HTTPS or Secure
cookies are outside the MVP.

Supported logging is capped at INFO and does not include request headers,
response headers, credentials, or checker argv. Enabling Pingora dependency
DEBUG or TRACE logging is unsupported and risks exposing authentication and
other header values.

## Explicit MVP non-goals

The MVP intentionally does not include:

- containers, Apple container support, direct child-process supervision, or
  raw one-off-command execution;
- installing, editing, discovering, inspecting, or repairing target plists;
- native aria2 logic, an aria2-specific adapter or checker, generalized
  health adapters, or other HTTP/JSON-RPC stop-check adapters (an operator's
  external argv program may apply application-specific policy);
- real aria2, removable-volume, TCC, or disk-throttling validation;
- SSE support or investigation;
- configuration reload, persistence, aliases, a dashboard, an admin API, or
  metrics;
- gateway authentication, TLS, certificate handling, public access, HTTP/2,
  or HTTP/3;
- rate limits, body-size or CORS policy, `Forwarded`/`X-Forwarded-*`
  synthesis, or cookie and redirect rewriting;
- graceful drain protocols, SIGKILL escalation, PID discovery, or target
  restart-policy repair; or
- broad launchd, security, performance, or proxy-conformance audits.

There are no post-MVP milestones in this plan.

## Quality gates

The complete MVP is checked with:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```
