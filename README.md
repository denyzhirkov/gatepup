# GatePup

Tiny watchdog for your web traffic.

GatePup is a lightweight, ultra-fast and resilient reverse proxy written in Rust.
It is designed for simple Docker-based deployments, self-hosted apps and small
production environments where traditional reverse proxies may feel too complex.

> **Status:** MVP in progress (v0.1). Working today: config model/loader/validator,
> CLI (`run` / `validate` / `print-config`), HTTP/1.1 reverse proxy with host/path
> routing, round-robin upstreams, active + passive health checks, structured JSON
> access logs, Prometheus metrics, and the admin API. Not yet: TLS, WebSocket,
> retries, hot reload (see roadmap).

## What it does

- HTTP/1.1 reverse proxy
- Host- and path-prefix routing
- Upstream groups with round-robin load balancing
- Active and passive health checks (opt-in per upstream)
- Structured JSON access logs, Prometheus metrics, admin API
- A single human-readable JSON config

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

## Docker

```bash
docker compose up --build
# proxy on http://localhost:80, admin/metrics on http://localhost:8080
curl localhost/            # proxied to the demo upstream
curl localhost:8080/health
```

The image is multi-stage, runs as a non-root user (granted
`cap_net_bind_service` so it can bind port 80), ships a default config at
`/etc/gatepup/config.json` (override by mounting your own), exposes ports 80 and
8080, and has a container `HEALTHCHECK` hitting the admin `/health`. `docker
stop` shuts down gracefully (SIGTERM drains in-flight requests).

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
`requestTimeoutMs`), and optional `admin` / `metrics` sections.

## Development

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Project conventions live in [`CLAUDE.md`](./CLAUDE.md); scope and roadmap in
[`gatepup_master_prompt.md`](./gatepup_master_prompt.md).

## Roadmap

- **v0.1** — core HTTP proxy, routing, upstreams, round-robin, health checks, metrics, JSON logs, Docker image.
- **v0.2** — hot reload, WebSocket, least-connections, retries, request IDs, header rewrite.
- **v0.3** — TLS termination (rustls), ACME, rate limiting, IP allow/deny, basic auth, compression.
- **v0.4** — admin REST API, UI dashboard, safe reload.

## License

MIT.
