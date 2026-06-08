# gatepup-bench

Load-testing tools and results for GatePup. `bench-backend` is a minimal, fast
HTTP backend used as the upstream under test; load is generated with
[`oha`](https://github.com/hatoo/oha).

## Reproduce

```bash
cargo build --release -p gatepup-cli -p gatepup-bench
brew install oha   # or cargo install oha

# fast upstream (use GATEPUP_BENCH_DELAY_MS=<n> to add per-request latency)
./target/release/bench-backend 127.0.0.1:9000 A &

# proxy in front of it (logLevel "warn" so per-request access logs don't
# dominate stdout at high rps)
GATEPUP_LOG=warn ./target/release/gatepup run --config bench-baseline.json &

oha --no-tui -z 12s -c 100 http://127.0.0.1:8088/   # through GatePup
oha --no-tui -z 12s -c 100 http://127.0.0.1:9000/   # direct (ceiling)
```

## Results

Machine: 10-core (backend, proxy, and `oha` all share the same cores, so
absolute numbers are conservative — they reflect the proxy *plus* contention).

### Throughput / latency (`-c 100`, 12s)

| Path             | Requests/sec | Avg latency | Success |
|------------------|--------------|-------------|---------|
| Direct → backend | ~147,000     | 0.68 ms     | 100%    |
| Through GatePup  | ~70,000      | 1.43 ms     | 100%    |

GatePup adds ~0.75 ms average overhead and sustains ~70k rps while sharing
cores with a 147k-capable backend and the load generator. Zero errors.

### Concurrency ramp (single upstream)

| Concurrency | Requests/sec | Avg     | p99      | Success |
|-------------|--------------|---------|----------|---------|
| 50          | ~67,600      | 0.74 ms | 1.04 ms  | 100%    |
| 200         | ~70,800      | 2.82 ms | 3.74 ms  | 100%    |
| 500         | ~71,000      | 7.04 ms | 9.53 ms  | 100%    |
| 1000        | ~67,500      | 14.8 ms | 18.1 ms  | 100%    |

Throughput holds flat from c=50 to c=1000 (no collapse); latency degrades
linearly with concurrency (Little's law). No panics, 100% success throughout.

### Resilience — kill a target under load

Two targets, health checks on (`unhealthyThreshold: 1`), one killed mid-load:
of **1,311,936** requests at ~65k rps, only **134** failed (**0.010%**) before
the dead target was ejected; the target recovered after it was restarted.

### Memory / file descriptors

Under sustained 60s load (c=200, ~70k rps, 4.24M requests): RSS held a flat
plateau (~19.8 MB) for the first ~40s; FDs stayed bounded (~415) and were
reclaimed after load (→157).

A **10-minute soak** (c=200, ~60k rps, **36.1M requests**) confirmed no leak:
100% success, 0 panics; RSS reached a bounded ~50 MB ceiling and then oscillated
**up and down** (~42–49 MB) rather than growing monotonically — allocator
high-water, not a leak. FDs stayed bounded. Memory behavior under sustained load
is healthy.

### Graceful shutdown under load

In-flight requests are drained on Ctrl-C: a SIGINT mid-flight lets outstanding
requests finish (100% success) before the process exits, bounded by a 15s drain
timeout. (Before draining was added, the same scenario severed every in-flight
request — see the `graceful_shutdown_drains_in_flight_request` regression test.)

## Findings & tuning notes

- **Health thresholds under saturation.** With `unhealthyThreshold: 1`, an
  active probe to a *healthy-but-saturated* target can time out and briefly
  eject it (transient 503 flapping). In production use `unhealthyThreshold ≥ 3`
  and a generous `timeoutMs`.
- **Access logging cost.** At tens of thousands of rps, per-request access logs
  to stdout become a bottleneck; benchmark with `logLevel: warn`.
