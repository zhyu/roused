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
6. When Roused itself receives `SIGTERM` or `SIGINT`, it performs one bounded,
   best-effort cleanup pass over quiescent configured targets before exiting.

## Requirements and security warning

- macOS with a logged-in GUI user session;
- a Roused executable in a stable location;
- target services already defined and bootstrapped as current-user
  LaunchAgents in `gui/$UID`; and
- DNS or client host configuration that points each configured hostname to the
  Mac running Roused.

Building Roused from source additionally requires Rust/Cargo with Rust 2024
edition support and CMake. These tools are not required to run a downloaded
executable.

Roused serves unencrypted HTTP and has no authentication layer of its own.
Basic, Bearer, cookie, and body-token credentials pass through to the upstream
in cleartext on the network. Use it only on a deliberately trusted private
network and never expose it directly to the public internet.

## Install a nightly build

Each successful `main` build publishes an Apple-silicon archive and SHA-256
checksum to the moving
[`nightly` prerelease](https://github.com/zhyu/roused/releases/tag/nightly):

```sh
curl --fail --location --remote-name \
  https://github.com/zhyu/roused/releases/download/nightly/roused-macos-arm64.tar.gz
curl --fail --location --remote-name \
  https://github.com/zhyu/roused/releases/download/nightly/roused-macos-arm64.tar.gz.sha256
shasum -a 256 --check roused-macos-arm64.tar.gz.sha256
tar -xzf roused-macos-arm64.tar.gz
./roused-macos-arm64/roused --help
```

Move the executable to a stable, user-writable absolute path before generating
a gateway LaunchAgent. Nightly binaries are not code-signed or notarized.

## Quick start

The normal setup path needs only the Roused executable. If you are building
from a source checkout instead, build the committed dependency graph and put
the resulting executable at a stable absolute path:

```sh
cargo build --locked --release
```

Each target must already use `RunAtLoad=false` and `KeepAlive=false`, listen on
its configured loopback address, and have a `Label` equal to the corresponding
`launchd_label` in the Roused configuration. Bootstrapping the target plist in
`gui/$UID` loads its definition without starting it. Prefer the target
application's own LaunchAgent setup when it provides one. Otherwise, macOS's
built-in `plutil` can construct a minimal definition at a new path. The
`-create` operation replaces an existing file, so do not run this sequence over
a plist you already manage:

```sh
/bin/mkdir -p "$HOME/Library/LaunchAgents"
target_plist="$HOME/Library/LaunchAgents/net.example.service.plist"
/usr/bin/plutil -create xml1 "$target_plist"
/usr/bin/plutil -insert Label -string net.example.service "$target_plist"
/usr/bin/plutil -insert ProgramArguments -array "$target_plist"
/usr/bin/plutil -insert ProgramArguments.0 -string \
  "/absolute/path/to/target-service" "$target_plist"
/usr/bin/plutil -insert RunAtLoad -bool false "$target_plist"
/usr/bin/plutil -insert KeepAlive -bool false "$target_plist"
/usr/bin/plutil -lint "$target_plist"
/bin/launchctl bootstrap "gui/$(id -u)" "$target_plist"
```

Add service-specific arguments at `ProgramArguments.1`,
`ProgramArguments.2`, and so on before linting. Other keys such as environment,
working-directory, or logging settings belong to the target and should follow
its documentation or `man 5 launchd.plist`. Roused does not generate or manage
this plist.

Generate a starter configuration at a new path:

```sh
roused init-config /absolute/path/to/roused.toml
```

The generated file contains two distinct, valid `[[services]]` entries to show
how multiple services are configured. Edit every retained entry for a real
target, add another complete `[[services]]` block for each additional target,
and delete an unused starter block. At least one service is required. These
entries configure routing; they do not create or install target services.

Set `listen` to `0.0.0.0:<port>` to accept cleartext connections on every IPv4
interface, or use `127.0.0.1:<port>` if only local clients should reach the
gateway. Then validate the complete file without starting Roused or touching
launchd:

```sh
roused check-config /absolute/path/to/roused.toml
```

Run Roused in the foreground:

```sh
roused /absolute/path/to/roused.toml
```

Foreground startup uses the same complete validation as `check-config` before
binding the listener. Roused does not reload configuration at runtime.

For a local smoke test without changing DNS, substitute one configured host
and the configured listener port in both places:

```sh
curl -i --resolve service.apps.home.arpa:8080:127.0.0.1 \
  http://service.apps.home.arpa:8080/
```

A stopped target normally makes the first request return `503 Service
Unavailable` with `Retry-After: 5`. Retry after the target begins listening;
the later request should reach the service. Roused configures routing only—it
does not create DNS records.

The command interfaces are:

```text
roused <config.toml>
roused init-config OUTPUT
roused check-config CONFIG
roused init-gateway-plist --label LABEL --config CONFIG --output OUTPUT \
  [--log-dir DIRECTORY] [--program PROGRAM]
```

Use `roused --help` or a command's `--help` for concise usage information.
Argument and configuration errors are reported as `roused: ...` and exit with
status 2. Both generation commands refuse to overwrite an existing output.

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

Roused accepts launchd labels, never target plist paths. Neither startup nor
`check-config` inspects a target's plist or proves that its job exists. A
missing label, a job that was not bootstrapped, or a service listening at the
wrong address becomes visible when wake attempts fail and clients continue
receiving `503` responses.

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

After an allowed check and an atomic activity recheck, Roused starts and awaits
this best-effort call, with a fixed five-second timeout:

```text
/bin/launchctl kill SIGTERM gui/$UID/<launchd_label>
```

This signals the job but does not unload it. If the target exits on `SIGTERM`
and has `KeepAlive=false`, it remains stopped until a later request wakes it.
Roused does not drain the target, escalate to `SIGKILL`, discover its PID, or
repair its restart policy.

### Gateway shutdown and restart

`SIGTERM` and `SIGINT` start a coordinated gateway shutdown. Roused closes its
request-admission gate, gives already-admitted requests and launch attempts up
to five seconds to transfer into a service lease or finish, and gives each
service lease the same deadline to drain. Parsed request headers that reach
proxy handling after shutdown has started receive `503 Service Unavailable`,
`Cache-Control: no-store`, nominal body `gateway shutting down\n` (omitted for
HEAD), and a closed connection; connections the listener has already closed may
instead fail before a response.

Each configured service that becomes quiescent within the drain window gets one
shutdown stop decision immediately, without waiting for its ordinary idle
timeout. The services are handled concurrently. The optional
`can_stop_command` still has its normal five-second timeout, and an allowed
decision still receives an atomic activity and launch-state recheck before
Roused awaits the five-second `launchctl kill SIGTERM` call. A stop attempt that
was already running when gateway shutdown began is awaited rather than
duplicated.

This cleanup is intentionally conservative. A target remains running when its
request or wake attempt does not drain in five seconds, its check vetoes, fails,
or times out, an existing stop decision is invalidated by activity, or
`launchctl` fails. Roused does not track which process first woke a job, so the
cleanup candidate set is every configured label, including a target that was
already running before this gateway process started.

Roused waits for lifecycle cleanup to finish before handing `SIGTERM` or
`SIGINT` to Pingora, subject to a 20-second coordination ceiling. Pingora then
uses a one-second graceful period and a zero-second final runtime timeout. An
empty or prompt cleanup therefore normally exits after the actual cleanup time
plus about one second; a five-second request-drain or checker timeout normally
exits after about six seconds. For `SIGTERM` or `SIGINT`, the longest legitimate
sequential path for one service can approach 17 seconds including Pingora
teardown, while services are still handled concurrently. If cleanup has not
completed after 20 seconds, Roused proceeds with Pingora shutdown and the
unfinished best-effort cleanup may be interrupted. A request that remains
active past the five-second drain deadline may likewise be cut off during
Pingora's following one-second grace. Generated plists allow 30 seconds as a
launchd safety ceiling; that ceiling does not delay an earlier process exit.

`SIGKILL`, a crash, or an unhandled terminating signal cannot run cleanup. If
launchd restarts Roused after such an exit, the replacement does not scan
processes or recover an old deadline; it creates fresh in-memory lifecycle
state for every configured label. A surviving target is therefore managed
under a fresh idle grace period, and later requests reset that new deadline
normally.

A service restart is indistinguishable from a permanent stop to the old
gateway. In particular, `brew services restart roused` is a stop followed by a
start, not a configuration reload signal. The old gateway cleans up quiescent
targets; the replacement does not eagerly start them again. A target left
running by conservative cleanup is adopted under the replacement gateway's
fresh lifecycle state. Roused still does not reload configuration in place.
`SIGQUIT` closes request admission and waits for the same cleanup before
entering Pingora's graceful-upgrade signal path. Pingora adds a fixed
five-second upgrade delay before its one-second graceful period, so a prompt
`SIGQUIT` takes about six seconds after cleanup. It is not a Roused
configuration-reload interface.

## Run Roused at login

After the foreground smoke test succeeds, generate the gateway's current-user
LaunchAgent plist at a new, absolute output path:

```sh
/bin/mkdir -p "$HOME/Library/Logs"
roused init-gateway-plist \
  --label net.example.roused \
  --config /absolute/path/to/roused.toml \
  --output "$HOME/Library/LaunchAgents/net.example.roused.plist" \
  --program /stable/absolute/path/to/roused
/usr/bin/plutil -lint \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
/bin/launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/net.example.roused.plist"
```

`--config`, `--output`, and an explicit `--program` or `--log-dir` must be
written as lexical absolute paths. With no `--log-dir`, Roused reads `HOME`
from its command environment and uses `$HOME/Library/Logs`, the macOS
user-domain log location appropriate for an unprivileged LaunchAgent.
`/var/log` is a system location and is not used by this workflow. The selected
directory must already exist. If `HOME` is unavailable or the default is
missing or not a directory, the command reports the problem; pass `--log-dir`
to select another existing directory. Explicit program-path spelling is
preserved, so `--program` can deliberately name a stable installed or Homebrew
symlink. If `--program` is omitted, Roused derives the absolute path of its
current executable. The command validates the configuration, safely escapes
the generated XML, and refuses to overwrite an existing output.

The generated plist sets `ExitTimeOut=30`, giving the coordinated shutdown a
launchd safety window before forced termination; it is not a fixed wait when
Roused exits sooner. It sends stdout to
`<label>.stdout.log` and stderr to `<label>.stderr.log` inside the selected
directory. Deriving the names from the launchd label keeps separately labeled
gateway instances from sharing log files. Roused's INFO/WARN lifecycle logs—including wake attempts and results,
stop-check outcomes and timeouts, and target-stop attempts and results—and
startup diagnostics are written to stderr; stdout is captured separately.
launchd creates missing log files when it starts the job. The generator
creates only the selected plist: it does not create directories or log files,
probe directory writability, bootstrap the job, invoke `launchctl`, or inspect
or change target plists. Ensure the directory is writable before bootstrapping
the plist. No automatic log rotation or size cap is configured. Follow the
logs with:

```sh
/usr/bin/tail -F "$HOME/Library/Logs/net.example.roused.stderr.log"
```

When Roused runs in the foreground, its logs continue to appear on the
terminal's stderr rather than in these launchd-selected files.

The generated plist uses `RunAtLoad=true` and `KeepAlive=true`, so launchd
starts and restarts the gateway after you bootstrap it manually. A deliberate
stop or restart first invokes the bounded cleanup described above. After a
crash, any surviving configured target is adopted with a fresh in-memory idle
grace period.

To unload the generated gateway job, address its current-user service target:

```sh
/bin/launchctl bootout "gui/$(id -u)/net.example.roused"
```

Manage target jobs separately according to their own setup. None of these
gateway operations needs `sudo` or writes under `/Library`.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Repeated `503` responses | Confirm the target job is bootstrapped in `gui/$UID`, its label matches the configuration, and it listens on the configured loopback socket after kickstart. |
| `421 Misdirected Request` | Confirm the request `Host` matches `services[].host`; an included port must equal the Roused listener port. |
| A configuration edit has no effect | Restart Roused; configuration is loaded only at startup. A restart performs the normal bounded target cleanup first. |
| A target never stops | Check for active requests or WebSockets, a vetoed/failed stop command, a target that ignores `SIGTERM`, or an incompatible launchd `KeepAlive` policy. |
| The gateway does not start under launchd | Inspect `$HOME/Library/Logs/<label>.stderr.log` (or the selected override) and confirm that the log directory existed and was writable before bootstrapping the plist. |

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
