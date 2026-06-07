# GatePup

Tiny watchdog for your web traffic.

GatePup is a lightweight, ultra-fast and resilient reverse proxy written in Rust.
It is designed for simple Docker-based deployments, self-hosted apps and small
production environments where traditional reverse proxies may feel too complex.

> **Status:** MVP in progress (v0.1). The config model, loader, validator and
> CLI (`validate` / `print-config`) are implemented. The proxy server itself is
> not wired up yet — `gatepup run` is a placeholder.

## What it does

- HTTP/1.1 reverse proxy (planned)
- Host- and path-prefix routing
- Upstream groups with round-robin load balancing
- Active and passive health checks
- Structured JSON logs, Prometheus metrics, admin health endpoint
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
gatepup run          --config ./config.json   # not implemented yet
gatepup validate     --config ./config.json
gatepup print-config --config ./config.json
```

Logging is controlled by the `GATEPUP_LOG` env var (defaults to `info`) and is
written to stderr.

## Config

See [`config.example.json`](./config.example.json) for a full example. A config
declares `listeners` (with routes that match on host + path prefix), `upstreams`
(target groups with a load-balancing strategy and optional health checks), and
optional `admin` / `metrics` sections.

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
