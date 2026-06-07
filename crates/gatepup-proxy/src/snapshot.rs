use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gatepup_config::GatePupConfig;
use serde::Serialize;

use crate::error::ProxyError;
use crate::health::{HealthCheckSettings, HealthState};
use crate::router::Router;

/// Immutable runtime view of the configuration. Built once and shared via
/// `Arc`; request handling never mutates it (target health uses interior
/// atomics so the snapshot itself stays immutable).
pub struct RuntimeConfig {
    pub(crate) listeners: Vec<Arc<ListenerRuntime>>,
    pub(crate) upstreams: HashMap<String, Arc<UpstreamRuntime>>,
}

pub(crate) struct ListenerRuntime {
    pub(crate) name: String,
    pub(crate) bind: SocketAddr,
    pub(crate) router: Router,
}

pub(crate) struct UpstreamRuntime {
    pub(crate) targets: Vec<TargetRuntime>,
    /// Active-health settings when health management is enabled for this upstream.
    pub(crate) health: Option<HealthCheckSettings>,
    next: AtomicUsize,
}

pub(crate) struct TargetRuntime {
    pub(crate) url: String,
    pub(crate) state: HealthState,
}

impl UpstreamRuntime {
    /// Round-robin over the healthy targets. Returns `None` when every target
    /// is unhealthy.
    pub(crate) fn pick_target(&self) -> Option<&TargetRuntime> {
        let n = self.targets.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let idx = self.next.fetch_add(1, Ordering::Relaxed) % n;
            let target = &self.targets[idx];
            if target.state.is_healthy() {
                return Some(target);
            }
        }
        None
    }
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

/// Build the runtime snapshot from a validated config. The config is assumed to
/// have passed [`gatepup_config::validate`]; bind parsing is re-checked
/// defensively and surfaced as a typed error rather than a panic.
pub fn build_snapshot(config: &GatePupConfig) -> Result<RuntimeConfig, ProxyError> {
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
        listeners.push(Arc::new(ListenerRuntime {
            name: listener.name.clone(),
            bind,
            router: Router::build(&listener.routes),
        }));
    }

    Ok(RuntimeConfig {
        listeners,
        upstreams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Managed health with threshold 1 so a single `observe(false)` ejects.
    fn upstream(urls: &[&str]) -> UpstreamRuntime {
        UpstreamRuntime {
            targets: urls
                .iter()
                .map(|u| TargetRuntime {
                    url: u.to_string(),
                    state: HealthState::new(true, 1, 1),
                })
                .collect(),
            health: None,
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
}
