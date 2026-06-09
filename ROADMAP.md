# GatePup — Roadmap & Topics

Cross-session topics index. Source of truth for *scope* is `gatepup_master_prompt.md`;
machine task state is in `tsk`; design decisions/gotchas are in kungfu memory.
This file is the human-readable map. Guiding philosophy: **do less, but do it
extremely well** — don't grow scope beyond the master prompt without an explicit
decision.

Status: **v1.1.0 released** (Docker Hub `denyzhirkov/gatepup`). Done: MVP (v0.1) +
Reliability (v0.2: retries, TLS, WebSocket, hot reload, wildcard host) + Edge
config (v0.3: env config, Alpine image) + v1.1 (path rewrite + DoS hardening:
body size + client header timeouts).

## A. Roadmap features — milestone plan

Complexity S/M/L/XL · priority P0 (blocks completeness) / P1 (baseline) / P2 (polish).
Ordering rationale: dependency-correctness (real-IP before rate-limit/ACL),
cheap-and-valuable first, isolate big/risky epics (ACME, HTTP/2).

**Quick win (parallel, now)**
- `5zyeu3` — Multi-arch Docker image amd64+arm64 — S, P0. CI only; widens adoption. NEW.

**v1.2 — Safe at the edge**
- `rncdxs` — Trusted-proxy / real client IP — S–M, P0 (prereq for rate-limit & IP-ACL). NEW.
- `5m1881` — Header manipulation (request/response) — M, P0. Security headers / CORS.
- `g9zer2` — IP allow/deny (CIDR) — S–M, P0. Needs `rncdxs`.
- `tja7bh` — Rate limiting — L, P0. Needs `rncdxs`.
- fold in: `jjmfcn` (chunked→413, S), `olvvf5` (idle timeout, M).

**v1.3 — HTTPS everywhere**
- `f3kite` — TLS to upstream (HTTPS backends) — M, P0. Plain HttpConnector today. NEW.
- `epk1t4` — Auto-TLS (ACME / Let's Encrypt) — XL, P0. Flagship; own focused epic.

**v1.4 — Modern + manageable**
- `ubmeqp` — Admin API authentication — S–M, P1. Prereq for UI / remote.
- `8mockr` — HTTP/2 (downstream + optional upstream) — L, P1. Touches core server.
- `3znddq` — Response compression (gzip/br) — M, P1.
- `nbcfic` — Basic auth on routes — S–M, P1.

**v1.5 / later — Ops polish**
- `um7vor` — LB strategies (least_connections / random / ip_hash; lock-free) — M, P2.
- `2ngyq1` — Admin UI dashboard — M, P2. Needs `ubmeqp`.
- `4gbgcc` — Admin config diff + safe-reload preview — M, P2.
- `qo06n7` — Deterministic slow-handshake-doesn't-block-accepts test — S, P2.
- `uaa1p7` — v0.5+ umbrella: canary, blue/green, sticky, WASM, k8s, static — XL, P2.

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
