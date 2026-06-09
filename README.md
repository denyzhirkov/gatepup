# GatePup

```text
        / \__
       (    @\___        GatePup
       /         O       tiny watchdog at your gate
      /   (_____/        fast · resilient · JSON-configured
     /_____/   U
                         client  ──▶  [ gatepup ]  ──▶  backend
```

[![CI](https://github.com/denyzhirkov/gatepup/actions/workflows/docker.yml/badge.svg)](https://github.com/denyzhirkov/gatepup/actions/workflows/docker.yml)
[![Docker Hub](https://img.shields.io/docker/v/denyzhirkov/gatepup?sort=semver&logo=docker&label=docker%20hub)](https://hub.docker.com/r/denyzhirkov/gatepup)
[![Docker Pulls](https://img.shields.io/docker/pulls/denyzhirkov/gatepup?logo=docker)](https://hub.docker.com/r/denyzhirkov/gatepup)
[![Image Size](https://img.shields.io/docker/image-size/denyzhirkov/gatepup?sort=semver&logo=docker)](https://hub.docker.com/r/denyzhirkov/gatepup)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Tiny watchdog for your web traffic.

GatePup is a lightweight, ultra-fast and resilient reverse proxy written in Rust.
It is designed for simple Docker-based deployments, self-hosted apps and small
production environments where traditional reverse proxies may feel too complex.

> **Status:** `v1.0.0` released and load-tested (~70k rps, no leak over a 10-min
> soak). Working today: HTTP/1.1 reverse proxy with host/path routing and path
> rewrite, weighted round-robin upstreams, active + passive health checks, retries,
> TLS termination (rustls), WebSocket tunneling, hot config reload (SIGHUP),
> env-based / file-less config, request-body & client-header DoS limits, structured
> JSON access logs, Prometheus metrics, and a read-only admin API. See the
> [roadmap](ROADMAP.md).

## What it does

- HTTP/1.1 reverse proxy with streaming bodies and WebSocket tunneling
- Host- and path-prefix routing (incl. wildcard hosts) with optional prefix strip
- Upstream groups with weighted round-robin, retries, and connection reuse
- Active and passive health checks (opt-in per upstream)
- TLS termination (rustls), hot config reload (SIGHUP), graceful drain on shutdown
- DoS limits: request body size + client header timeouts
- Structured JSON access logs, Prometheus metrics, read-only admin API
- A single human-readable JSON config — or run file-less from `GATEPUP_*` env vars

## What it is *not*

GatePup is deliberately small. It does not try to be nginx, a WAF, a service
mesh, a static file server, or a Kubernetes-first ingress. **Do less, but do it
extremely well.**

## Quick start

Validate the example config:

```bash
cargo run -p gatepup-cli -- validate --config ./config.example.json
```

Print the effective, validated config as JSON:

```bash
cargo run -p gatepup-cli -- print-config --config ./config.example.json
```

A failing config lists every problem at once, by name:

```text
Config is invalid (2 problem(s)):
  - listener "public-http" has invalid bind address "nope"
  - route "api" references unknown upstream "api-service"
```

## CLI

```bash
gatepup run          --config ./config.json
gatepup validate     --config ./config.json
gatepup print-config --config ./config.json
```

`run` serves the proxy (and the admin server if enabled) until Ctrl-C. Logs are
structured JSON on stdout; the level comes from `GATEPUP_LOG`, then the config
`logLevel`, then `info`.

## Admin API & metrics

When `admin.enabled`, GatePup serves read-only management endpoints on
`admin.bind` (default `127.0.0.1:8080`):

```bash
curl 127.0.0.1:8080/health             # {"status":"ok","version":"..."}
curl 127.0.0.1:8080/routes             # routes across all listeners
curl 127.0.0.1:8080/upstreams          # upstreams with per-target health
curl 127.0.0.1:8080/config/effective   # the validated effective config
curl 127.0.0.1:8080/metrics            # Prometheus metrics (when metrics.enabled)
```

## WebSocket

WebSocket (and other HTTP `Upgrade`) requests are proxied automatically — no
config needed. When a request carries `Connection: upgrade` + `Upgrade:`, GatePup
forwards the handshake to the chosen upstream target and, on `101 Switching
Protocols`, tunnels raw bytes bidirectionally for the life of the connection.

- The upstream is reached over plain HTTP (TLS to the upstream is not supported).
- Upgrade requests are not retried.
- Each upgraded connection increments `gatepup_websocket_connections_total`.

## Hot reload

Send `SIGHUP` to reload the config file without dropping connections:

```bash
kill -HUP $(pidof gatepup)   # or: docker kill --signal=HUP <container>
```

- The config is re-read, validated, and the routing snapshot is **atomically
  swapped**. New requests use the new config; in-flight requests finish on the
  old one.
- If the new config is invalid (parse or validation error), the **old config is
  kept** and the proxy keeps serving — a bad reload never takes you down.
- **What reloads:** routes, upstreams (targets/weights/health/retries), and
  timeouts. Active health checks are re-spawned for the new upstreams.
- **What needs a restart:** listener bind addresses and TLS certificates.
- Each reload increments `gatepup_config_reloads_total{result="success|failure"}`.

## TLS

GatePup can terminate TLS at a listener (rustls, ring provider). Upstreams stay
plain HTTP — TLS is terminated at the edge.

```json
{
  "name": "public-https",
  "bind": "0.0.0.0:443",
  "protocol": "https",
  "tls": { "cert": "/etc/gatepup/tls/cert.pem", "key": "/etc/gatepup/tls/key.pem" },
  "routes": [ { "name": "app", "match": { "pathPrefix": "/" }, "upstream": "app" } ]
}
```

- `cert` / `key` are PEM file paths (cert chain + private key). They are read at
  startup; a load failure stops the proxy with a clear error.
- Requests forwarded from a TLS listener carry `X-Forwarded-Proto: https`.
- ACME / Let's Encrypt is not included yet (planned).

## Retries

Retries are opt-in per upstream. When enabled, a failed attempt is retried onto a
*different* target (weighted round-robin advances):

```json
"retries": {
  "enabled": true,
  "attempts": 2,
  "methods": ["GET", "HEAD", "OPTIONS"],
  "retryOn": ["connect_error", "upstream_5xx"]
}
```

- `attempts` is the max **total** tries (≥ 1).
- Only `methods` are retried — default is idempotent only. Listing non-idempotent
  methods (POST/PUT/PATCH/DELETE) is at your own risk.
- `retryOn` accepts `connect_error`, `connect_timeout`, `upstream_5xx`. An overall
  request timeout (`requestTimeoutMs`) is **never** retried — the backend may have
  already processed it.
- To retry, the request body is buffered (bounded to 64 KiB). Larger bodies (and
  non-retryable methods) stream through with a single attempt.
- Retries increment the `gatepup_upstream_retries_total` metric.

## Path rewrite

By default the original request path is forwarded to the upstream as-is. Set
`stripPrefix: true` on a route to remove the matched `pathPrefix` before
forwarding — handy for mounting a service under a sub-path:

```json
{
  "name": "api",
  "match": { "host": "api.example.com", "pathPrefix": "/api" },
  "upstream": "api",
  "stripPrefix": true
}
```

- `/api/users` → upstream sees `/users`; `/api` → `/`. The query string is
  preserved.
- The result is always normalized to start with `/`.
- The prefix is matched as a plain string (same as routing), with no path-label
  boundary: with `pathPrefix: "/api"`, a request to `/apidocs` strips to
  `/docs`. Use a trailing slash in `pathPrefix` if you want a boundary.
- Default is `false` — the path is forwarded unchanged.

## Header manipulation

A route can rewrite headers on the **request** (before forwarding upstream) and
on the **response** (before returning to the client) — e.g. inject security
headers, strip `Server`, or add a marker:

```json
{
  "name": "api",
  "match": { "pathPrefix": "/api" },
  "upstream": "api",
  "headers": {
    "request":  { "set": { "X-Proxied-By": "gatepup" }, "remove": ["X-Debug"] },
    "response": { "set": { "X-Frame-Options": "DENY" }, "remove": ["Server"] }
  }
}
```

- `set` inserts or overwrites a header; `remove` deletes it. For each direction
  `remove` runs first, then `set`, so an explicit `set` always wins.
- Request rules apply **after** GatePup's own `X-Forwarded-*` rewrite, so you can
  override or strip those too.
- Names and values are validated (a bad name or a CR/LF-injecting value is a
  config error).
- Header rules apply to normal proxied HTTP requests; WebSocket upgrade requests
  are forwarded with their handshake headers intact.

## Limits (DoS hardening)

The optional `limits` block bounds what a single client can consume. Unlike
`timeouts` (which bound the *upstream* round-trip), these protect the proxy from
slow or oversized *inbound* requests:

```json
"limits": {
  "maxBodyBytes": 10485760,
  "headerReadTimeoutMs": 15000,
  "maxHeaderBytes": 65536
}
```

- **`maxBodyBytes`** — reject a request body larger than this with `413
  payload_too_large`. A declared `Content-Length` over the cap is rejected up
  front; an undeclared (chunked) body is cut mid-stream once it exceeds the cap.
  `0` (default) means unlimited.
- **`headerReadTimeoutMs`** — max time to receive the full request header from a
  client (slowloris guard). A client that stalls mid-header has its connection
  dropped. Default `15000`; `0` disables it.
- **`maxHeaderBytes`** — bound the connection read buffer, which caps the request
  header section. `0` (default) keeps hyper's built-in ~400 KB bound; when set it
  must be at least `8192`.

These are connection-level settings: a change takes effect on restart, not on a
hot reload.

## Client IP & trusted proxies

GatePup logs the real client IP (`client_ip` in the access log) and appends the
direct peer to `X-Forwarded-For`. When GatePup runs behind a CDN or another
proxy, list those hops in `trustedProxies` so the real client is read from
`X-Forwarded-For` instead of seeing the CDN's address:

```json
"trustedProxies": ["10.0.0.0/8", "192.168.0.0/16"]
```

- Entries are CIDRs or bare IPs (a bare IP is a single host).
- The client IP is resolved by walking `X-Forwarded-For` right-to-left and taking
  the first hop that is **not** a trusted proxy.
- If the direct peer is **not** in `trustedProxies`, inbound `X-Forwarded-For` is
  ignored entirely — a spoofed header can never override the source address.
- Empty (default) means the direct TCP peer is always the client.

This resolved client IP is what upcoming IP allow/deny and rate-limiting features
will key on, so it must be correct behind your edge.

## Docker

Pull the published image from [Docker Hub](https://hub.docker.com/r/denyzhirkov/gatepup)
(`:latest` = newest release, `:edge` = latest `main`):

```bash
docker pull denyzhirkov/gatepup:latest
```

Or run the bundled compose demo:

```bash
docker compose up --build
# proxy on http://localhost:80, admin/metrics on http://localhost:8080
curl localhost/            # proxied to the demo upstream
curl localhost:8080/health
```

The image is a multi-stage **Alpine** build (static musl binary) — **~25 MB**. It
runs as a non-root user (granted `cap_net_bind_service` so it can bind port 80),
ships a default config at `/etc/gatepup/config.json` (override by mounting your
own, or run config-free via `GATEPUP_*` env vars — see *Configuration via
environment*), exposes ports 80 and 8080, and has a container `HEALTHCHECK`
(busybox `wget`) hitting the admin `/health`. `docker stop` shuts down gracefully
(SIGTERM drains in-flight requests).

```bash
# config-free container (no mounted file)
docker run -p 80:80 -p 8080:8080 \
  -e GATEPUP_LISTEN=0.0.0.0:80 -e GATEPUP_UPSTREAM=http://api:4000 \
  -e GATEPUP_ADMIN=0.0.0.0:8080 denyzhirkov/gatepup:latest
```

> The compose demo binds the admin API to `0.0.0.0:8080` so it's reachable from
> the host. The secure default is `127.0.0.1`; don't expose the admin port
> publicly without auth.

## Configuration via environment

GatePup can run with **no config file** — handy for Docker. The effective config
is resolved with this precedence (first match wins for the base), then scalar
overrides are layered on top:

1. `GATEPUP_CONFIG=<path>` — load this file
2. `GATEPUP_CONFIG_JSON='{...}'` — inline full config JSON
3. `--config <path>` — CLI flag
4. **simple mode** — build a single-listener config from env (below)

```bash
# config-free single listener, round-robins over two upstreams
GATEPUP_LISTEN=0.0.0.0:8080 \
GATEPUP_UPSTREAM=http://api-1:4000,http://api-2:4000 \
GATEPUP_ADMIN=0.0.0.0:9090 GATEPUP_LOG=info \
gatepup run
```

| Variable | Effect |
|---|---|
| `GATEPUP_CONFIG` | Path to a JSON config file (highest precedence) |
| `GATEPUP_CONFIG_JSON` | Inline full config JSON |
| `GATEPUP_LISTEN` | simple mode: listener bind (default `0.0.0.0:8080`) |
| `GATEPUP_UPSTREAM` | simple mode: comma-separated target URLs (round-robin) |
| `GATEPUP_ROUTE_HOST` | simple mode: optional host match |
| `GATEPUP_TLS_CERT` / `GATEPUP_TLS_KEY` | simple mode: enable HTTPS on the listener |
| `GATEPUP_HEALTHCHECK_PATH` | simple mode: enable active health checks |
| `GATEPUP_LOG` | override `app.logLevel` |
| `GATEPUP_ADMIN` | enable admin on this bind |
| `GATEPUP_METRICS_PATH` | enable metrics at this path |
| `GATEPUP_TIMEOUT_CONNECT_MS` / `GATEPUP_TIMEOUT_REQUEST_MS` | override timeouts |

Scalar overrides (`GATEPUP_LOG`, `GATEPUP_ADMIN`, `GATEPUP_METRICS_PATH`,
`GATEPUP_TIMEOUT_*`) apply on top of **any** base source. Malformed values fail
loud with a named error. The resolved config goes through the same validation as
a file. Routing topology with multiple listeners/routes stays in the file or
`GATEPUP_CONFIG_JSON`.

## Config

See [`config.example.json`](./config.example.json) for a full example. A config
declares `listeners` (with routes that match on host + path prefix), `upstreams`
(target groups with weighted round-robin — each target has an optional `weight`,
default 1 — and optional health checks), `timeouts` (`connectTimeoutMs` /
`requestTimeoutMs`), `limits` (request body size + client header timeouts),
`trustedProxies` (CIDRs whose `X-Forwarded-For` is trusted for the real client
IP), and optional `admin` / `metrics` sections.

## Development

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The roadmap and standing engineering principles live in [`ROADMAP.md`](./ROADMAP.md).

## Roadmap

Shipped: HTTP/1.1 proxy, host/path routing (+ wildcard hosts, prefix strip),
weighted round-robin, health checks, retries, TLS termination, WebSocket, hot
reload, env/file-less config, Alpine image, observability, request/header DoS
limits, graceful drain.

Next: header manipulation, rate limiting, admin API auth, ACME/Let's Encrypt,
more balancing strategies (least-connections / ip-hash), compression, HTTP/2. Full
list in [`ROADMAP.md`](./ROADMAP.md).

## License

[MIT](LICENSE).
