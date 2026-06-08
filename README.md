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
