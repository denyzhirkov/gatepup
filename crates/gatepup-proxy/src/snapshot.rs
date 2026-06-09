use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gatepup_config::{parse_trusted_proxy, GatePupConfig, RetryConfig};
use http::Method;
use ipnet::IpNet;
use serde::Serialize;
use tokio_rustls::TlsAcceptor;

use crate::error::ProxyError;
use crate::health::{HealthCheckSettings, HealthState};
use crate::router::Router;

/// Parsed retry policy for an upstream (only present when retries are enabled).
pub(crate) struct RetryPolicy {
    /// Max total tries (>= 1).
    pub(crate) max_attempts: u32,
    methods: HashSet<Method>,
    pub(crate) on_connect_failure: bool,
    pub(crate) on_5xx: bool,
}

impl RetryPolicy {
    fn from_config(cfg: &RetryConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let methods = cfg
            .methods
            .iter()
            .filter_map(|m| Method::from_bytes(m.as_bytes()).ok())
            .collect();
        let on_connect_failure = cfg
            .retry_on
            .iter()
            .any(|c| c == "connect_error" || c == "connect_timeout");
        let on_5xx = cfg.retry_on.iter().any(|c| c == "upstream_5xx");
        Some(Self {
            max_attempts: cfg.attempts.max(1),
            methods,
            on_connect_failure,
            on_5xx,
        })
    }

    /// Whether this method is eligible for retries (so its body may be buffered).
    pub(crate) fn allows_method(&self, method: &Method) -> bool {
        self.methods.contains(method)
    }
}

/// Immutable runtime view of the configuration. Built once and shared via
/// `Arc`; request handling never mutates it (target health uses interior
/// atomics so the snapshot itself stays immutable).
pub struct RuntimeConfig {
    pub(crate) listeners: Vec<Arc<ListenerRuntime>>,
    pub(crate) upstreams: HashMap<String, Arc<UpstreamRuntime>>,
    /// Timeout for establishing the upstream connection (set on the client).
    pub(crate) connect_timeout: Duration,
    /// Timeout for the whole upstream round-trip (mapped to 504 on expiry).
    pub(crate) request_timeout: Duration,
    /// Max request body bytes; `0` = unlimited. A larger body is rejected (413).
    pub(crate) max_body_bytes: u64,
    /// Header-read timeout (slowloris guard); `None` = disabled. Connection-level.
    pub(crate) header_read_timeout: Option<Duration>,
    /// Max connection read-buffer bytes (bounds the header section); `None` keeps
    /// hyper's default. Connection-level.
    pub(crate) max_header_bytes: Option<usize>,
    /// Trusted upstream proxy networks: when the direct peer is in one of these,
    /// the client IP is resolved from `X-Forwarded-For`. Empty = peer is client.
    pub(crate) trusted_proxies: Arc<[IpNet]>,
}

pub(crate) struct ListenerRuntime {
    pub(crate) name: String,
    pub(crate) bind: SocketAddr,
    pub(crate) router: Router,
    /// TLS acceptor when this is an `https` listener; `None` for plain HTTP.
    pub(crate) tls: Option<TlsAcceptor>,
}

pub(crate) struct UpstreamRuntime {
    pub(crate) targets: Vec<TargetRuntime>,
    /// Active-health settings when health management is enabled for this upstream.
    pub(crate) health: Option<HealthCheckSettings>,
    /// Retry policy when retries are enabled for this upstream.
    pub(crate) retry: Option<RetryPolicy>,
    /// Precomputed weighted schedule: target indices, each appearing in
    /// proportion to its weight (gcd-reduced, interleaved). Equal weights reduce
    /// to plain round-robin. Indexed lock-free via `next`.
    schedule: Vec<usize>,
    next: AtomicUsize,
}

pub(crate) struct TargetRuntime {
    pub(crate) url: String,
    pub(crate) state: HealthState,
}

impl UpstreamRuntime {
    /// Weighted round-robin over the healthy targets. Walks the schedule from the
    /// next position, skipping unhealthy targets; returns `None` only when every
    /// target is unhealthy. Unhealthy targets are simply skipped, so traffic
    /// redistributes across the remaining targets in proportion to their weights.
    pub(crate) fn pick_target(&self) -> Option<&TargetRuntime> {
        let len = self.schedule.len();
        if len == 0 {
            return None;
        }
        for _ in 0..len {
            let slot = self.next.fetch_add(1, Ordering::Relaxed) % len;
            let target = &self.targets[self.schedule[slot]];
            if target.state.is_healthy() {
                return Some(target);
            }
        }
        None
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Build the weighted schedule: each target index repeated `weight / gcd` times,
/// interleaved so picks rotate smoothly rather than bursting on one target.
/// Equal weights yield `[0, 1, .., n-1]` (plain round-robin).
fn build_schedule(weights: &[u32]) -> Vec<usize> {
    if weights.is_empty() {
        return Vec::new();
    }
    let divisor = weights
        .iter()
        .copied()
        .filter(|&w| w > 0)
        .fold(0, gcd)
        .max(1);
    let mut remaining: Vec<u32> = weights.iter().map(|&w| w / divisor).collect();
    let total: u32 = remaining.iter().sum();
    let mut schedule = Vec::with_capacity(total as usize);
    while (schedule.len() as u32) < total {
        for (idx, rem) in remaining.iter_mut().enumerate() {
            if *rem > 0 {
                schedule.push(idx);
                *rem -= 1;
            }
        }
    }
    schedule
}

/// Read-only route view for the admin API.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteView {
    pub listener: String,
    pub name: String,
    pub host: Option<String>,
    pub path_prefix: String,
    pub upstream: String,
}

/// Read-only upstream view for the admin API.
#[derive(Serialize)]
pub struct UpstreamView {
    pub name: String,
    pub targets: Vec<TargetView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetView {
    pub url: String,
    pub healthy: bool,
    /// Unix epoch millis of the last health observation; 0 if never checked.
    pub last_check_epoch_ms: u64,
}

impl RuntimeConfig {
    /// All routes across all listeners, for `GET /routes`.
    pub fn routes(&self) -> Vec<RouteView> {
        self.listeners
            .iter()
            .flat_map(|listener| {
                let listener_name = listener.name.clone();
                listener
                    .router
                    .summaries()
                    .into_iter()
                    .map(move |s| RouteView {
                        listener: listener_name.clone(),
                        name: s.name,
                        host: s.host,
                        path_prefix: s.path_prefix,
                        upstream: s.upstream,
                    })
            })
            .collect()
    }

    /// All upstreams with per-target health, for `GET /upstreams`. Sorted by name.
    pub fn upstreams_view(&self) -> Vec<UpstreamView> {
        let mut views: Vec<UpstreamView> = self
            .upstreams
            .iter()
            .map(|(name, upstream)| UpstreamView {
                name: name.clone(),
                targets: upstream
                    .targets
                    .iter()
                    .map(|t| TargetView {
                        url: t.url.clone(),
                        healthy: t.state.is_healthy(),
                        last_check_epoch_ms: t.state.last_check_ms(),
                    })
                    .collect(),
            })
            .collect();
        views.sort_by(|a, b| a.name.cmp(&b.name));
        views
    }

    /// `(upstream, healthy target count)` pairs, for the metrics gauge.
    pub fn healthy_counts(&self) -> Vec<(String, i64)> {
        self.upstreams
            .iter()
            .map(|(name, upstream)| {
                let healthy = upstream
                    .targets
                    .iter()
                    .filter(|t| t.state.is_healthy())
                    .count() as i64;
                (name.clone(), healthy)
            })
            .collect()
    }
}

/// Build the runtime snapshot from a validated config (TLS acceptors included).
/// The config is assumed to have passed [`gatepup_config::validate`].
pub fn build_snapshot(config: &GatePupConfig) -> Result<RuntimeConfig, ProxyError> {
    build_snapshot_inner(config, true)
}

/// Build a snapshot for a hot reload: identical, but WITHOUT building TLS
/// acceptors. Listener bindings/acceptors are fixed at startup, so reload only
/// swaps routing/upstreams and must not depend on (or re-read) cert files.
pub fn build_reload_snapshot(config: &GatePupConfig) -> Result<RuntimeConfig, ProxyError> {
    build_snapshot_inner(config, false)
}

fn build_snapshot_inner(
    config: &GatePupConfig,
    with_tls: bool,
) -> Result<RuntimeConfig, ProxyError> {
    let mut upstreams = HashMap::with_capacity(config.upstreams.len());
    for upstream in &config.upstreams {
        let hc = upstream.health_check.as_ref();
        // Health management is opt-in via an enabled health check. When off,
        // targets stay healthy and observations are no-ops (see `health` module).
        let managed = hc.map(|h| h.enabled).unwrap_or(false);
        let (healthy_threshold, unhealthy_threshold) = hc
            .map(|h| (h.healthy_threshold, h.unhealthy_threshold))
            .unwrap_or((1, 3));
        let settings = if managed {
            hc.map(|h| HealthCheckSettings {
                path: h.path.clone(),
                interval: Duration::from_secs(h.interval_seconds),
                timeout: Duration::from_millis(h.timeout_ms),
            })
        } else {
            None
        };

        let weights: Vec<u32> = upstream.targets.iter().map(|t| t.weight).collect();
        let mut schedule = build_schedule(&weights);
        // Defensive: a validated config has weight >= 1, but never serve an
        // upstream with targets yet an empty schedule.
        if schedule.is_empty() && !upstream.targets.is_empty() {
            schedule = (0..upstream.targets.len()).collect();
        }

        let targets = upstream
            .targets
            .iter()
            .map(|t| TargetRuntime {
                url: t.url.clone(),
                state: HealthState::new(managed, healthy_threshold, unhealthy_threshold),
            })
            .collect();
        upstreams.insert(
            upstream.name.clone(),
            Arc::new(UpstreamRuntime {
                targets,
                health: settings,
                retry: upstream.retries.as_ref().and_then(RetryPolicy::from_config),
                schedule,
                next: AtomicUsize::new(0),
            }),
        );
    }

    let mut listeners = Vec::with_capacity(config.listeners.len());
    for listener in &config.listeners {
        let bind: SocketAddr = listener.bind.parse().map_err(|_| ProxyError::InvalidBind {
            name: listener.name.clone(),
            bind: listener.bind.clone(),
        })?;
        let tls = match (&listener.tls, with_tls) {
            (Some(cfg), true) => {
                Some(
                    crate::tls::build_acceptor(cfg).map_err(|e| ProxyError::Tls {
                        listener: listener.name.clone(),
                        message: e.to_string(),
                    })?,
                )
            }
            _ => None,
        };
        listeners.push(Arc::new(ListenerRuntime {
            name: listener.name.clone(),
            bind,
            router: Router::build(&listener.routes),
            tls,
        }));
    }

    let limits = &config.limits;
    // Entries are validated before a snapshot is built; parse defensively and
    // drop any that don't resolve (validation would have already flagged them).
    let trusted_proxies: Arc<[IpNet]> = config
        .trusted_proxies
        .iter()
        .filter_map(|s| parse_trusted_proxy(s))
        .collect();
    Ok(RuntimeConfig {
        listeners,
        upstreams,
        connect_timeout: Duration::from_millis(config.timeouts.connect_timeout_ms),
        request_timeout: Duration::from_millis(config.timeouts.request_timeout_ms),
        max_body_bytes: limits.max_body_bytes,
        header_read_timeout: (limits.header_read_timeout_ms > 0)
            .then(|| Duration::from_millis(limits.header_read_timeout_ms)),
        max_header_bytes: (limits.max_header_bytes > 0).then_some(limits.max_header_bytes),
        trusted_proxies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Managed health with threshold 1 so a single `observe(false)` ejects.
    fn upstream(urls: &[&str]) -> UpstreamRuntime {
        weighted_upstream(urls, &vec![1; urls.len()])
    }

    fn weighted_upstream(urls: &[&str], weights: &[u32]) -> UpstreamRuntime {
        UpstreamRuntime {
            targets: urls
                .iter()
                .map(|u| TargetRuntime {
                    url: u.to_string(),
                    state: HealthState::new(true, 1, 1),
                })
                .collect(),
            health: None,
            retry: None,
            schedule: build_schedule(weights),
            next: AtomicUsize::new(0),
        }
    }

    fn pick(u: &UpstreamRuntime) -> Option<String> {
        u.pick_target().map(|t| t.url.clone())
    }

    #[test]
    fn round_robin_cycles_through_targets() {
        let u = upstream(&["a", "b", "c"]);
        let seq: Vec<String> = (0..6).filter_map(|_| pick(&u)).collect();
        assert_eq!(seq, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn pick_target_skips_unhealthy() {
        let u = upstream(&["a", "b", "c"]);
        u.targets[1].state.observe(false); // eject "b"
        let seq: Vec<String> = (0..4).filter_map(|_| pick(&u)).collect();
        assert!(seq.iter().all(|url| url != "b"), "got {seq:?}");
        assert!(seq.contains(&"a".to_string()) && seq.contains(&"c".to_string()));
    }

    #[test]
    fn pick_target_none_when_all_unhealthy() {
        let u = upstream(&["a", "b"]);
        for t in &u.targets {
            t.state.observe(false);
        }
        assert!(u.pick_target().is_none());
    }

    #[test]
    fn pick_target_none_when_no_targets() {
        let u = upstream(&[]);
        assert!(u.pick_target().is_none());
    }

    #[test]
    fn build_schedule_equal_weights_is_plain_round_robin() {
        assert_eq!(build_schedule(&[1, 1, 1]), vec![0, 1, 2]);
    }

    #[test]
    fn build_schedule_reduces_by_gcd() {
        // 2:4 -> 1:2 : one slot for target 0, two for target 1.
        let s = build_schedule(&[2, 4]);
        assert_eq!(s.iter().filter(|&&i| i == 0).count(), 1);
        assert_eq!(s.iter().filter(|&&i| i == 1).count(), 2);
    }

    #[test]
    fn build_schedule_interleaves_rather_than_bursting() {
        // 3:1 should not place all of target 0 before target 1.
        let s = build_schedule(&[3, 1]);
        assert_eq!(s.iter().filter(|&&i| i == 0).count(), 3);
        assert_eq!(s.iter().filter(|&&i| i == 1).count(), 1);
        assert_ne!(s, vec![0, 0, 0, 1], "schedule should be interleaved");
    }

    #[test]
    fn weighted_pick_honors_weights() {
        let u = weighted_upstream(&["a", "b"], &[3, 1]);
        let mut a = 0;
        let mut b = 0;
        for _ in 0..400 {
            match pick(&u).as_deref() {
                Some("a") => a += 1,
                Some("b") => b += 1,
                _ => {}
            }
        }
        assert_eq!(a + b, 400);
        assert_eq!(a, 300, "target a should get 3/4 of traffic");
        assert_eq!(b, 100, "target b should get 1/4 of traffic");
    }

    #[test]
    fn retry_policy_parses_methods_and_conditions() {
        let cfg = RetryConfig {
            enabled: true,
            attempts: 3,
            methods: vec!["GET".to_string(), "POST".to_string()],
            retry_on: vec!["connect_timeout".to_string(), "upstream_5xx".to_string()],
        };
        let policy = RetryPolicy::from_config(&cfg).unwrap();
        assert_eq!(policy.max_attempts, 3);
        assert!(policy.allows_method(&Method::GET));
        assert!(policy.allows_method(&Method::POST));
        assert!(!policy.allows_method(&Method::PUT));
        assert!(policy.on_connect_failure); // connect_timeout counts
        assert!(policy.on_5xx);
    }

    #[test]
    fn retry_policy_none_when_disabled() {
        let cfg = RetryConfig {
            enabled: false,
            attempts: 2,
            methods: vec!["GET".to_string()],
            retry_on: vec!["connect_error".to_string()],
        };
        assert!(RetryPolicy::from_config(&cfg).is_none());
    }

    #[test]
    fn weighted_pick_redistributes_when_a_target_is_unhealthy() {
        let u = weighted_upstream(&["a", "b"], &[3, 1]);
        u.targets[1].state.observe(false); // eject "b" (the weight-1 target)
        let all_a = (0..100).filter_map(|_| pick(&u)).all(|url| url == "a");
        assert!(all_a, "all traffic should go to the only healthy target");
    }
}
