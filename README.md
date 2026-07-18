# Roused

Roused is a macOS activation reverse proxy for lightly used web services. It
routes plain HTTP/1.1 requests by hostname, wakes an existing current-user
LaunchAgent when its service is unavailable, proxies requests while the service
is running, and requests shutdown after the service has been idle long enough.

The bounded MVP is implemented and usable. It is intentionally designed for a
trusted private network on one macOS host; it does not provide TLS or gateway
authentication.

## How it works

1. A client sends a request to a configured hostname on the Roused listener.
2. If the target's loopback port is ready, Roused proxies the request
   immediately.
3. If the target is not ready, Roused asks launchd to start its already-loaded
   current-user LaunchAgent and returns a temporary `503`. The client retries
   later; Roused never replays the original request.
4. Proxied requests keep the target active until their uploads, responses, or
   WebSocket connections finish.
5. Once the target is idle, an optional external check may veto shutdown.
   Otherwise Roused asks launchd to deliver `SIGTERM`.

## Requirements and security warning

- macOS with a logged-in GUI user session;
- Rust/Cargo with Rust 2024 edition support and CMake to build Roused;
- target services already defined as current-user LaunchAgents; and
- DNS or client host configuration that points each configured hostname to the
  Mac running Roused.

Roused serves unencrypted HTTP and has no authentication layer of its own.
Basic, Bearer, cookie, and body-token credentials pass through to the upstream
in cleartext on the network. Use it only on a deliberately trusted private
network and never expose it directly to the public internet.

## Quick start

Build the committed dependency graph:

```sh
cargo build --locked --release
```

Prepare the target service as a current-user LaunchAgent. The supplied template
is a starting point, not an installer:

```sh
cp packaging/launchd/roused-target.plist \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
# Edit the copy: set its label and absolute executable/argument paths.
/usr/bin/plutil -lint \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
/bin/launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/net.example.service.plist"
```

The target must use `RunAtLoad=false` and `KeepAlive=false`, listen on the
configured loopback address, and have a `Label` equal to `launchd_label` in the
Roused configuration. Bootstrapping loads its definition without starting it.
If the target is already correctly bootstrapped, do not bootstrap it again.

Create a configuration file such as `roused.toml`:

```toml
listen = "0.0.0.0:8080"

[[services]]
host = "service.apps.home.arpa"
upstream = "127.0.0.1:9000"
launchd_label = "net.example.service"
idle_timeout_seconds = 1800
can_stop_command = ["/absolute/path/to/can-stop", "--literal-argument"]
```

`0.0.0.0` accepts cleartext connections on every IPv4 interface. Use
`127.0.0.1` instead if only local clients should reach the gateway.

Run Roused in the foreground:

```sh
./target/release/roused /absolute/path/to/roused.toml
```

Roused validates the complete configuration before it binds the listener. It
has no validation-only command and does not reload configuration at runtime.

For a local smoke test without changing DNS:

```sh
curl -i --resolve service.apps.home.arpa:8080:127.0.0.1 \
  http://service.apps.home.arpa:8080/
```

A stopped target normally makes the first request return `503 Service
Unavailable` with `Retry-After: 5`. Retry after the target begins listening;
the later request should reach the service. Roused configures routing only—it
does not create DNS records.

## Configuration reference

| Field | Required | Meaning |
| --- | --- | --- |
| `listen` | yes | Literal IP socket for the gateway, using port 1024 or higher. |
| `services[].host` | yes | ASCII DNS hostname without a port. Matching is case-insensitive and accepts one terminal dot or the configured listener port on requests. |
| `services[].upstream` | yes | Literal loopback IP socket with a nonzero port. |
| `services[].launchd_label` | yes | Label of an already-bootstrapped job in `gui/$UID`. |
| `services[].idle_timeout_seconds` | no | Nonzero idle interval; defaults to 1,800 seconds. |
| `services[].can_stop_command` | no | Nonempty direct argv. The executable in element zero must be an absolute path. |

At least one service is required. Normalized hosts, upstreams, and launchd
labels must be unique. Unknown fields and any invalid entry reject the entire
configuration.

Roused accepts launchd labels, never plist paths. Startup validation does not
inspect a target's plist or prove that its job exists. A missing label, a job
that was not bootstrapped, or a service listening at the wrong address becomes
visible when wake attempts fail and clients continue receiving `503` responses.

## Request and wake behavior

Roused routes the HTTP `Host` header. A malformed or unconfigured host receives:

- `421 Misdirected Request`;
- `Content-Type: text/plain; charset=utf-8`;
- `Cache-Control: no-store`; and
- body `unknown host\n`.

For a ready service, Roused preserves methods, paths and queries, the original
`Host`, request and response streaming, upstream-owned authentication and
cookies, repeated response fields, and valid WebSocket upgrades. It removes
`Proxy-Authorization` and the bounded hop-by-hop headers described in
[`docs/design.md`](./docs/design.md). Each request gets only one total upstream
attempt, so a POST is never automatically resubmitted.

For an unavailable service, Roused performs a short loopback readiness check
and deduplicates:

```text
/bin/launchctl kickstart gui/$UID/<launchd_label>
```

The launch command has a five-second timeout and a five-second retry cooldown.
An explicitly HTML-accepting GET or HEAD receives a small loading page; every
other cold request receives JSON. Both responses are `503`, carry
`Retry-After: 5` and `Cache-Control: no-store`, and close the downstream
connection. The cold request body is neither buffered nor replayed.

## Idle and stop behavior

Only a request that reaches a ready upstream holds a service lease. The lease
lasts through the full upload and response, a disconnect or fatal error, and a
WebSocket upgrade until that socket closes. Cold and unknown-host responses do
not hold leases.

For proxied work, the idle interval begins when the most recent request
completes. A configured request arrival also advances the grace period and
invalidates an obsolete pending stop decision. Shutdown is considered only
when no request is in flight. Each service receives a fresh idle grace period
when the gateway starts; lifecycle state is not persisted.

When `can_stop_command` is configured, Roused executes it directly without a
shell. Arguments are literal, and stdin, stdout, and stderr are discarded. Use
absolute paths for executables and any file arguments, especially when Roused
runs under launchd. Only exit status 0 permits shutdown. Failure, a nonzero
status, or the fixed five-second timeout keeps the target running; while it
remains idle, another check runs no sooner than 30 seconds later.

After an allowed check and an atomic activity recheck, Roused makes this
best-effort call:

```text
/bin/launchctl kill SIGTERM gui/$UID/<launchd_label>
```

This signals the job but does not unload it. If the target exits on `SIGTERM`
and has `KeepAlive=false`, it remains stopped until a later request wakes it.
Roused does not drain the target, escalate to `SIGKILL`, discover its PID, or
repair its restart policy.

## Run Roused at login

After the foreground smoke test succeeds, install the gateway as another
current-user LaunchAgent:

```sh
cp packaging/launchd/roused-gateway.plist \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
# Edit the copy to use absolute paths to the release binary and TOML file.
/usr/bin/plutil -lint \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
/bin/launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
```

The template uses `RunAtLoad=true` and `KeepAlive=true`, so launchd starts and
restarts the gateway. Restarting Roused does not restart a target that is
already listening; that target is proxied immediately and receives a fresh
in-memory idle grace period.

To unload the example jobs, address their current-user service targets:

```sh
/bin/launchctl bootout "gui/$(id -u)/net.example.roused"
/bin/launchctl bootout "gui/$(id -u)/net.example.service"
```

None of these operations needs `sudo` or writes under `/Library`.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Repeated `503` responses | Confirm the target job is bootstrapped in `gui/$UID`, its label matches the configuration, and it listens on the configured loopback socket after kickstart. |
| `421 Misdirected Request` | Confirm the request `Host` matches `services[].host`; an included port must equal the Roused listener port. |
| A configuration edit has no effect | Restart Roused; configuration is loaded only at startup. |
| A target never stops | Check for active requests or WebSockets, a vetoed/failed stop command, a target that ignores `SIGTERM`, or an incompatible launchd `KeepAlive` policy. |

Roused logs service labels and lifecycle outcomes at INFO/WARN, but not request
headers, response headers, credentials, or checker argv.

## Current limitations

- macOS and current-user `gui/$UID` LaunchAgents only;
- plain HTTP/1.1 only—no TLS, HTTP/2, HTTP/3, or public-access hardening;
- no gateway authentication, rate limits, body-size policy, or CORS policy;
- no configuration reload, persistent lifecycle state, host aliases,
  dashboard, admin API, or metrics;
- no target plist installation, discovery, inspection, editing, or repair;
- no direct target-process supervision or arbitrary target command execution;
- no `Forwarded`/`X-Forwarded-*` synthesis, cookie rewriting, or redirect
  rewriting; and
- no support guarantee for Server-Sent Events.

Roused preserves upstream `Secure` cookie attributes. Browsers generally do
not store or send those cookies over this cleartext gateway, so services that
require HTTPS or `Secure` cookies are not compatible. Dependency DEBUG/TRACE
logging is unsupported because it may expose authentication or other headers.

## Development and verification

Run the complete quality gates with:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

The full test suite requires macOS and a logged-in GUI user domain. It uses
uniquely labeled disposable current-user LaunchAgents and loopback fixtures;
it does not operate existing user services. One WebSocket integration test
intentionally runs for more than 60 seconds.

[`docs/design.md`](./docs/design.md) is the durable technical contract. The
[`docs/mvp-plan.md`](./docs/mvp-plan.md) file is retained as the historical
delivery and acceptance record, not as an active roadmap.
