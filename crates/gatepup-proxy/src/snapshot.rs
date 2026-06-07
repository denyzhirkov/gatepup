use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use gatepup_config::GatePupConfig;

use crate::error::ProxyError;
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
    next: AtomicUsize,
}

pub(crate) struct TargetRuntime {
    pub(crate) url: String,
    healthy: AtomicBool,
}

impl TargetRuntime {
    fn new(url: String) -> Self {
        Self {
            url,
            healthy: AtomicBool::new(true),
        }
    }
}

impl UpstreamRuntime {
    /// Round-robin over the healthy targets. Returns `None` when every target
    /// is unhealthy. Health checks (a later step) flip the `healthy` flag.
    pub(crate) fn pick_target(&self) -> Option<&TargetRuntime> {
        let n = self.targets.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let idx = self.next.fetch_add(1, Ordering::Relaxed) % n;
            let target = &self.targets[idx];
            if target.healthy.load(Ordering::Relaxed) {
                return Some(target);
            }
        }
        None
    }
}

/// Build the runtime snapshot from a validated config. The config is assumed to
/// have passed [`gatepup_config::validate`]; bind parsing is re-checked
/// defensively and surfaced as a typed error rather than a panic.
pub fn build_snapshot(config: &GatePupConfig) -> Result<RuntimeConfig, ProxyError> {
    let mut upstreams = HashMap::with_capacity(config.upstreams.len());
    for upstream in &config.upstreams {
        let targets = upstream
            .targets
            .iter()
            .map(|t| TargetRuntime::new(t.url.clone()))
            .collect();
        upstreams.insert(
            upstream.name.clone(),
            Arc::new(UpstreamRuntime {
                targets,
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

    fn upstream(urls: &[&str]) -> UpstreamRuntime {
        UpstreamRuntime {
            targets: urls
                .iter()
                .map(|u| TargetRuntime::new(u.to_string()))
                .collect(),
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
        u.targets[1].healthy.store(false, Ordering::Relaxed);
        let seq: Vec<String> = (0..4).filter_map(|_| pick(&u)).collect();
        assert!(seq.iter().all(|url| url != "b"), "got {seq:?}");
        assert!(seq.contains(&"a".to_string()) && seq.contains(&"c".to_string()));
    }

    #[test]
    fn pick_target_none_when_all_unhealthy() {
        let u = upstream(&["a", "b"]);
        for t in &u.targets {
            t.healthy.store(false, Ordering::Relaxed);
        }
        assert!(u.pick_target().is_none());
    }

    #[test]
    fn pick_target_none_when_no_targets() {
        let u = upstream(&[]);
        assert!(u.pick_target().is_none());
    }
}
