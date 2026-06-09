//! Per-client-IP, per-route token-bucket rate limiter.
//!
//! State lives outside the config snapshot (owned by the server, shared across
//! reloads) so a hot reload doesn't reset clients' budgets. The map is sharded
//! by key hash to keep lock contention low on the request path; each request
//! locks exactly one shard briefly. Idle (fully refilled) buckets are evicted
//! lazily when a shard grows past a threshold, so memory stays bounded even
//! under a flood of distinct client IPs.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

const SHARDS: usize = 32;
/// Per-shard entry count above which a sweep drops idle buckets.
const SWEEP_THRESHOLD: usize = 4096;

struct Bucket {
    tokens: f64,
    last: Instant,
    rate: f64,
    capacity: f64,
}

impl Bucket {
    /// Refill to the present, clamped to capacity.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = now;
    }

    /// True once enough time has passed that this bucket is back at capacity
    /// (i.e. equivalent to a fresh bucket — safe to evict).
    fn is_full_at(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + elapsed * self.rate) >= self.capacity
    }
}

pub(crate) struct RateLimiter {
    shards: Vec<Mutex<HashMap<(String, IpAddr), Bucket>>>,
}

impl RateLimiter {
    pub(crate) fn new() -> Self {
        let shards = (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect();
        Self { shards }
    }

    fn shard(&self, route: &str, ip: &IpAddr) -> &Mutex<HashMap<(String, IpAddr), Bucket>> {
        let mut h = DefaultHasher::new();
        route.hash(&mut h);
        ip.hash(&mut h);
        &self.shards[(h.finish() as usize) % SHARDS]
    }

    /// Consume one token for `(route, ip)`. Returns `true` if allowed, `false`
    /// if the bucket is empty (rate-limited). `rate`/`capacity` are read from the
    /// current snapshot each call, so a reload that changes them takes effect.
    pub(crate) fn check(
        &self,
        route: &str,
        ip: IpAddr,
        rate: f64,
        capacity: f64,
        now: Instant,
    ) -> bool {
        let shard = self.shard(route, &ip);
        // A poisoned lock only happens if a holder panicked; our critical section
        // is panic-free, so recover the guard rather than propagate a panic.
        let mut map = shard.lock().unwrap_or_else(PoisonError::into_inner);

        let bucket = map
            .entry((route.to_string(), ip))
            .or_insert_with(|| Bucket {
                tokens: capacity,
                last: now,
                rate,
                capacity,
            });
        // Keep params current (reload may have changed them).
        bucket.rate = rate;
        bucket.capacity = capacity;
        bucket.refill(now);

        let allowed = if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        };

        if map.len() > SWEEP_THRESHOLD {
            map.retain(|_, b| !b.is_full_at(now));
        }

        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn allows_up_to_burst_then_blocks() {
        let rl = RateLimiter::new();
        let t = Instant::now();
        // capacity 3, rate 1/s: first 3 pass at the same instant, 4th blocked.
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 3.0, t));
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 3.0, t));
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 3.0, t));
        assert!(!rl.check("r", ip("1.1.1.1"), 1.0, 3.0, t));
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new();
        let t = Instant::now();
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 1.0, t));
        assert!(!rl.check("r", ip("1.1.1.1"), 1.0, 1.0, t));
        // One second later, one token has refilled.
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 1.0, t + Duration::from_secs(1)));
    }

    #[test]
    fn buckets_are_independent_per_ip_and_route() {
        let rl = RateLimiter::new();
        let t = Instant::now();
        assert!(rl.check("r", ip("1.1.1.1"), 1.0, 1.0, t));
        // Different IP: own bucket.
        assert!(rl.check("r", ip("2.2.2.2"), 1.0, 1.0, t));
        // Different route, same IP: own bucket.
        assert!(rl.check("other", ip("1.1.1.1"), 1.0, 1.0, t));
        // First key is now exhausted.
        assert!(!rl.check("r", ip("1.1.1.1"), 1.0, 1.0, t));
    }
}
