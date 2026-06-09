# GatePup — Killer feature ideas

Differentiators for GatePup's niche (self-hosted / small-prod / Docker-Compose,
"nginx feels too complex"). Each must stay true to **do less, but do it
extremely well** — delight the target user, leverage the existing architecture
(immutable snapshot + atomic hot-swap, admin API, structured access logs, health
checks), and *not* drift into nginx/Envoy/k8s territory. Brand: a small, fast,
attentive watchdog at the gate.

These are candidate ideas, not committed scope — promote to the roadmap with an
explicit decision.

## 1. Docker label auto-discovery (`provider: docker`)

Watch the Docker socket and build routes + upstreams automatically from container
labels — expose a new service in `docker-compose.yml` with **zero config edits**:

```yaml
labels:
  gatepup.host: api.example.com
  gatepup.port: "3000"
  gatepup.path: /api
```

On container start/stop GatePup rebuilds the snapshot and atomically hot-swaps it
(the hot-reload machinery already exists). Opt-in provider; the JSON file stays
the source of truth for everything else, and file + label routes compose.

**Why it's a killer:** this is Traefik's headline feature — but delivered by a
single ~25 MB static binary with dead-simple labels and a readable JSON model,
not Traefik's complexity. For the Docker-Compose self-hoster it's *the* adoption
magnet: nginx makes you hand-edit + reload; GatePup just notices.

## 2. Live request tap (`gatepup tap` + admin stream)

A live, filterable view of requests flowing through the proxy — `tail -f` for your
traffic, without grepping JSON logs:

```bash
gatepup tap --host api.example.com --status 5xx
# 12:01:03  503  api.example.com  GET /api/users  -> api (no_healthy_upstream)  8ms  203.0.113.7
```

Backed by an admin SSE/ndjson endpoint (`GET /tap`) that streams the structured
access-log events GatePup already produces; the CLI pretty-prints + filters by
host/path/status/route/upstream.

**Why it's a killer:** "why is this route 502 right now?" is the #1 self-hosted
debugging pain, and nginx/Caddy answer it with log-grepping. GatePup *shows* you,
live. Pure delight, on-brand (the watchdog tells you what it sees), and it reuses
the access-log fields we already emit.

## 3. `gatepup doctor` — live preflight diagnostics

`validate` checks the config statically; `doctor` checks **reality** and prints
named, actionable findings:

- DNS resolves for each route host
- every upstream target is reachable (TCP / health-path probe)
- static TLS certs: load + expiry warning (e.g. "<14 days")
- listener bind conflicts / privileged-port capability
- route shadowing ("route B can never match — A already covers it")
- common footguns (admin on 0.0.0.0 without auth, `unhealthyThreshold: 1`, …)

**Why it's a killer:** it embodies the brand — the watchdog sniffs out problems
*before* they bite — and turns "it doesn't work and the logs are cryptic" into a
checklist with fixes. It's already sanctioned as a future CLI command in the
master prompt, and it builds directly on the config model + health probes.

## Honorable mention

- **Maintenance mode** — per-route friendly "be right back" page (served as 503)
  on a flip or when all upstreams are down, instead of a raw error. Small,
  delightful, on-brand ("the puppy holds the door").
