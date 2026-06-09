# GatePup — Roadmap & Topics

Cross-session topics index. Source of truth for *scope* is `gatepup_master_prompt.md`;
machine task state is in `tsk`; design decisions/gotchas are in kungfu memory.
This file is the human-readable map. Guiding philosophy: **do less, but do it
extremely well** — don't grow scope beyond the master prompt without an explicit
decision.

Status as of v1.0.0 released. MVP (v0.1) + Reliability (v0.2: retries, TLS,
WebSocket, hot reload, wildcard host) + Edge config (v0.3: env config, Alpine
image) + DoS hardening (body size + client timeouts) are **done**.

## A. Roadmap features

### Tracked in `tsk` (pending)
- `5m1881` — Header manipulation (request/response)
- `tja7bh` — Rate limiting
- `ubmeqp` — Admin API authentication
- `epk1t4` — Auto-TLS (ACME / Let's Encrypt)
- `um7vor` — Load-balancing strategies (least_connections / random / ip_hash; keep lock-free)
- `g9zer2` — IP allow/deny list (CIDR)
- `nbcfic` — Basic auth on routes
- `3znddq` — Response compression (gzip/br)
- `8mockr` — HTTP/2 support (downstream + optional upstream)
- `2ngyq1` — Admin UI dashboard
- `4gbgcc` — Admin config diff + safe-reload preview
- `uaa1p7` — v0.5+ backlog umbrella: canary, blue/green, sticky sessions, WASM hooks, k8s ingress, static files

## B. Standing cross-cutting topics (disciplines & invariants)

These don't "complete" — they apply to every change. Keep them honored.

- **Test вдоль и поперёк** — every feature + every failure branch, same change. Code without tests isn't done. (kungfu mem_0001)
- **Quality gate before commit** — `cargo test --workspace` + `cargo clippy --workspace --all-targets -D warnings` + `cargo fmt --all -- --check`, all green.
- **Performance / resilience** — lock-free hot path, streaming bodies, bounded everything, connection reuse; periodic load + soak before prod. (kungfu mem_0004, mem_0012)
- **Architecture** — immutable `Arc<ConfigSnapshot>` + atomic swap; domain→core→adapters; respect crate boundaries; no `unwrap()`/`expect()` in the request path. (kungfu mem_0003)
- **Security defaults** — admin binds `127.0.0.1` only; secrets/sensitive headers never logged; strict config validation.
- **Observability** — JSON access logs, Prometheus metrics at `/metrics`, read-only admin API.
- **Config validation completeness** — every new option gets named, actionable validation errors.
- **Scope discipline** — "do less, but well"; don't exceed the master prompt without an explicit decision.

## C. Known gaps / tech debt

- `olvvf5` — Idle/keep-alive connection timeout (hyper http1 has no knob; needs WS-safe manual impl). (kungfu mem_0014)
- `jjmfcn` — Chunked-body overflow returns 502, not a precise 413 (only Content-Length gives exact 413). (kungfu mem_0014)
- `qo06n7` — No deterministic test that a slow TLS handshake doesn't block other accepts. (kungfu mem_0013)

## Where state lives (session continuity)

1. `tsk` — machine task state (`tsk list`, `tsk list --inprogress`, `tsk show <id>`).
2. kungfu `memory_*` — design decisions, gotchas, conventions (the "why").
3. This file — the readable topics map; update it when a topic lands or a new one appears.
