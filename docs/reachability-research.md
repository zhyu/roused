# Reachability research: localhost-only clients and HTTPS

Status: non-normative research record.

This document captures an investigation into two reachability gaps in Roused:
clients that accept only loopback hosts, and clients that require HTTPS. It is
not part of the product contract, not an approved milestone, and not a roadmap.
Where it records a selected direction, that direction is a tentative preference
to guide later work, not a commitment. Any implementation that changes the
product contract requires corresponding updates to `docs/design.md`, the
README, and focused tests.

Verification claims below were checked against the cited sources at the time of
writing. They are noted so later work does not have to re-derive them.

## 1. The two blockers are orthogonal

The reachability gaps are frequently conflated, but they are independent:

- **Scheme.** A client that requires `https://` opens a TLS handshake. Roused
  closes that connection before request parsing (`src/plain_http.rs`). A custom
  header or a path prefix cannot change this outcome, because the connection
  never becomes HTTP.
- **Host.** A client that requires a literal loopback host (`localhost`,
  `127.0.0.1`, or `[::1]`) reaches the Roused listener over TCP without issue.
  The only missing piece is a route target.

Header-based and path-based selection address only the host blocker. This
distinction determines which approaches are viable for which class of client.

## 2. Approaches for localhost-only clients

### 2.1 Opt-in per-service loopback port binding — selected direction

Bind an additional `127.0.0.1:<port>` listener per service, so a client that
insists on `127.0.0.1:<fixed-port>` reaches the correct target by listener
rather than by `Host`.

- This is the only approach that satisfies exact-host and exact-port
  expectations. It is also the only approach that can satisfy exact-match
  redirect URIs, such as an OAuth callback registered as
  `http://localhost:1234/callback`, where neither headers nor a path prefix can
  be adjusted on the redirect.
- It is transport-agnostic and composes with any TLS layer.
- Cost: `RousedProxy` currently holds a single `listener_port` and one routing
  table (`src/proxy.rs`), so per-socket routing is a real refactor. The feature
  should be opt-in so the single-listener model remains the default.
- Selected direction: implement as opt-in per-service loopback port binding
  when localhost-only clients must be supported.

### 2.2 `.localhost` subdomains — rejected

macOS resolves any `*.localhost` name to loopback. Verified with `ping`:
`foo.bar.localhost`, `totally-random-xyz.localhost`, and `localhost.` all
resolve to `127.0.0.1`.

This would allow pure `Host` routing with almost no proxy code. It is rejected
because it depends on a client accepting a subdomain of `localhost` rather than
checking for the literal name. That behavior is not controllable and varies by
client.

### 2.3 Header-based routing — insufficient as a foundation

A custom header such as `X-Roused-Service: aria2` would let a cooperative client
select a target.

- Advantages: small change, preserves absolute paths, redirects, cookies, and
  WebSockets, and keeps the semantic-transparency contract intact.
- Limitation: it requires client-side cooperation. GUI applications and
  browsers cannot set it, which is often the audience for loopback-only
  restrictions. It also still requires the client to reach Roused's port.
- Not selected as the general mechanism.

### 2.4 Path-based routing — rejected

Routing on a URL prefix such as `http://localhost:8080/aria2/...` works only for
simple API clients configured with a base URL.

It is a poor general approach because it breaks on absolute paths produced by
the application itself. `Location` headers, HTML and JavaScript references, and
cookies scoped to `Path=/` do not carry the prefix. Making it correct requires
stripping the prefix upstream and rewriting `Location`, `Set-Cookie Path`, and
possibly response bodies, all of which are explicitly out of scope and a
well-known source of defects. It also collides with services that already use
the same prefix.

## 3. HTTPS options

### 3.1 Self-signed or private CA — limited

Trust is established per device and per trust store: the macOS System keychain,
an iOS profile plus full trust, an Android user CA (which some applications
ignore), Firefox's own store, and so on.

A client that bundles its own CA list or pins a CA or leaf rejects a self-signed
or private-CA certificate even when the operating system trusts it. Self-signed
certificates are therefore not a general solution.

### 3.2 Tailscale plain `tailscale serve` — free real HTTPS, one hostname

Tailscale provisions a real, publicly-trusted Let's Encrypt certificate for a
machine's MagicDNS name, `<machine>.<tailnet>.ts.net`, at no cost on all plans,
using DNS-01 handled by Tailscale. Because the certificate is publicly trusted,
no per-device CA installation is needed.

Setup enables MagicDNS and HTTPS Certificates in the admin console, then runs
`tailscale serve --bg --https=443 http://127.0.0.1:8080`.

The structural limitation is one trusted hostname per node. Multiplexing
multiple services onto that name has two forms:

- **Paths** (`--set-path`). Tailscale matches mounts Go-`ServeMux`-style and
  strips the mount prefix before proxying (confirmed in `serve.go`:
  `http.StripPrefix(...)`). This has the same fragility as path routing for web
  interfaces, though it is acceptable for API clients.
- **Ports.** This works, but `tailscale serve` preserves the client's original
  `Host` header to the backend (confirmed in `serve.go`: for non-unix backends,
  `r.Out.Host = r.In.Host`). A request to `machine.ts.net:8443` therefore
  presents `Host: machine.ts.net:8443` to Roused, which currently accepts only a
  port equal to its own listener port and would answer `421` unless the contract
  changes.

Additional notes: machine and tailnet names are published in the public
Certificate Transparency log, and this approach does not help literal-`localhost`
clients.

### 3.3 Tailscale Services — per-service names, at the cost of tagging

Tailscale Services gives each service a stable MagicDNS name and its own
automatically provisioned certificate, for example
`https://aria2.<tailnet>.ts.net`, configured with
`tailscale serve --service=svc:aria2 --https=443 127.0.0.1:8080`.

Because `tailscale serve` preserves the original `Host` header, Roused routes by
`Host` unchanged. This is the one Tailscale variant that solves multi-service
host-based routing without path or port multiplexing.

The cost is that the host device must use a tag-based identity, which has real
consequences:

- Applying a tag removes user-based authentication. A device has either a user
  or tags, never both. The device becomes a tailnet service identity.
- Taildrop cannot be used to or from tagged nodes.
- Tailscale SSH from a tagged device reaches only other tagged devices.
- Sharing semantics are affected because sharing strips tags, and user-based
  access control policies no longer apply to the device.
- The identity is authenticated by auth keys; all tags cannot be removed, and
  changing tags on an auth-key device requires a new auth key. Untagging
  requires re-authenticating as a user.
- Tailscale's own guidance warns against tagging user devices.

Services also require Tailscale v1.86 or later, a tag-based host identity, and
admin approval. The approach suits a dedicated always-on server host and is a
poor fit for a daily-driver machine.

### 3.4 Purchased domain with DNS-01 — selected direction

The DNS-01 ACME challenge proves domain control through a public TXT record. It
requires no inbound ports and no public reachability of the server. Only
outbound calls to the ACME server and the DNS provider API are needed.

- A real domain is required. Publicly-trusted certificates cannot be issued for
  `.home.arpa`, `.internal`, or `.local`.
- An A record, or a wildcard, points at a private address: a LAN address
  (`192.168.x.x`) or a Tailscale address (`100.x.x.x`). These ranges are
  non-routable publicly, so the service stays off the public internet.
- Wildcard certificates require DNS-01. A single wildcard certificate covers all
  service subdomains.

The combination that avoids both per-device trust and Tailscale tagging is to
point `*.home.example.com` at the Mac's Tailscale address, run a TLS terminator
such as Caddy with DNS-01 for a wildcard certificate, and reverse-proxy to
Roused over plain HTTP. This yields real per-service hostnames with real
certificates, no tagging, and no public exposure. Split-horizon DNS can keep the
A records internal while leaving only `_acme-challenge` records public.

Selected direction: purchase a domain, then evaluate Caddy first and decide
whether to use it. Caddy is the low-risk default because it owns TLS, DNS-01,
and renewal, and hot-reloads renewed certificates, leaving Roused's plain-HTTP
contract untouched.

### 3.5 Native TLS in Roused — possible, but not the certificate manager

Pingora supports server-side TLS through `add_tls`,
`TlsSettings::intermediate(cert_path, key_path)`, `with_callbacks`, and
`set_alpn(ALPN::H1)`. It provides no ACME support; a search of the Pingora
repository found none. Roused terminating TLS therefore still requires an
external ACME client (`certbot`, `lego`, `acme.sh`, or Caddy as an issuer).

Work implied by native TLS in Roused:

- Enable the `openssl` or `rustls` Pingora feature. The current dependency uses
  `default-features = false, features = ["proxy"]`, so TLS is not compiled in,
  and enabling it pulls in the OpenSSL/BoringSSL toolchain and its CMake build
  requirement.
- Add TLS configuration and a TLS listener.
- Keep ALPN constrained to H1, since the project is HTTP/1.1-only by design.
- Reconcile the existing TLS ClientHello rejection in `src/plain_http.rs`.
- Handle certificate rotation.
- Update `docs/design.md`, the README, and tests, because TLS and certificate
  management are currently explicit non-goals.

## 4. Certificate renewal

- Let's Encrypt default certificates are valid for 90 days, with renewal
  recommended every 60 days. An opt-in six-day short-lived profile exists, with
  renewal recommended every three days.
- CA/Browser Forum Ballot SC-081v3 caps public TLS certificate validity at 200
  days from March 2026, 100 days from March 2027, and 47 days from March 2029.

The renewal interval is not the important variable; automation is. With Caddy,
renewal is a background task and the new certificate is loaded in memory with no
restart. With static certificates read by Roused, every renewal requires a
restart, which:

- should be triggered only when the certificate actually changed, for example by
  comparing a fingerprint before and after renewal; and
- triggers Roused's coordinated shutdown cleanup, which sends `SIGTERM` to every
  idle configured target. A restart is a stop-then-start by design.

If an external renewer is used, the following apply:

- Prefer launchd over cron. cron skips jobs while the machine is asleep, and a
  LaunchAgent is consistent with Roused's `gui/$UID` model.
- Run the renewer frequently and idempotently, rather than on a fixed
  lifetime-matching interval, so the ACME client decides whether renewal is due
  and failed runs are retried.
- Keep the DNS API token in a mode-600 file, never in command arguments,
  property lists, or logs.

## 5. Comparison

| Approach | Localhost-only clients | HTTPS-only clients | Multi-service hostnames | Main cost |
| --- | --- | --- | --- | --- |
| Per-service loopback ports | yes | no | yes (by listener) | multi-listener refactor |
| Header routing | partial | no | yes | requires client cooperation |
| Path routing | partial | no | yes | breaks absolute paths, redirects, cookies |
| `.localhost` aliases | partial | no | yes | client must accept subdomains |
| Self-signed or private CA | no | yes | yes | per-device trust; pinning fails |
| Tailscale plain serve | no | yes (real cert) | paths or ports only | one hostname per node |
| Tailscale Services | no | yes (real cert) | yes | tag the host, approvals |
| Domain and DNS-01, with Caddy | no | yes | yes | domain and DNS API token |

## 6. Selected directions

These are recorded preferences for later work, not implemented behavior and not
commitments:

1. **Localhost-only clients:** opt-in per-service loopback port binding. It is
   the only general mechanism for exact-host and exact-port expectations.
2. **HTTPS:** purchase a domain and evaluate Caddy before considering native TLS
   in Roused. Caddy owns TLS, DNS-01, and renewal, and hot-reloads certificates,
   leaving Roused's plain-HTTP contract untouched.

## 7. Process note

Any of the selected directions that becomes real work is a deliberate contract
change. Per `AGENTS.md` and `docs/design.md`, the README, the design document,
and focused tests must be updated together. The rejected approaches, especially
path-based routing, are worth recording as explicit non-goals if this work is
formalized.

## 8. Sources consulted

- Roused sources: `src/plain_http.rs`, `src/proxy.rs`, `src/config.rs`,
  `Cargo.toml`, `docs/design.md`, `README.md`.
- macOS resolver behavior for `*.localhost`: local `ping` and `dscacheutil`.
- Pingora: `pingora-core/src/listeners/mod.rs`,
  `pingora-core/src/listeners/tls/boringssl_openssl/mod.rs`.
- Tailscale: tags documentation, Tailscale Services documentation, and
  `ipn/ipnlocal/serve.go` (mount-prefix stripping and `Host` preservation).
- Caddy: automatic HTTPS documentation (DNS-01 requirements and local CA).
- Let's Encrypt: FAQ (90-day default and six-day short-lived profile).
- CA/Browser Forum Ballot SC-081v3 (certificate validity schedule).
