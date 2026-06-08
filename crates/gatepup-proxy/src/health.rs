//! Health management for upstream targets.
//!
//! Health is **opt-in** per upstream via `healthCheck.enabled`. When enabled it
//! covers both directions through a single [`HealthState::observe`]:
//! - **Active:** a background task probes each target's health path on an
//!   interval and records the result.
//! - **Passive:** each proxied request records its outcome (success, or a
//!   connect error / timeout / 5xx as a failure).
//!
//! Transitions use consecutive-count thresholds. Recovery is always driven by
//! active probes, so a passively-ejected target can come back. When health is
//! unmanaged, every target stays healthy and `observe` is a no-op.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::uri::PathAndQuery;
use http::{Method, Request, Uri};
use http_body_util::Empty;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};

use crate::snapshot::UpstreamRuntime;
use crate::SharedConfig;

pub(crate) type HealthClient = Client<HttpConnector, Empty<Bytes>>;

/// Active-probe settings for one upstream, derived from an enabled health check.
#[derive(Clone)]
pub(crate) struct HealthCheckSettings {
    pub(crate) path: String,
    pub(crate) interval: Duration,
    pub(crate) timeout: Duration,
}

/// Per-target health: a healthy flag plus consecutive success/failure counters.
pub(crate) struct HealthState {
    healthy: AtomicBool,
    successes: AtomicU32,
    failures: AtomicU32,
    /// Unix epoch millis of the last observation; 0 means never observed.
    last_check_ms: AtomicU64,
    healthy_threshold: u32,
    unhealthy_threshold: u32,
    managed: bool,
}

impl HealthState {
    pub(crate) fn new(managed: bool, healthy_threshold: u32, unhealthy_threshold: u32) -> Self {
        Self {
            healthy: AtomicBool::new(true),
            successes: AtomicU32::new(0),
            failures: AtomicU32::new(0),
            last_check_ms: AtomicU64::new(0),
            healthy_threshold: healthy_threshold.max(1),
            unhealthy_threshold: unhealthy_threshold.max(1),
            managed,
        }
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub(crate) fn last_check_ms(&self) -> u64 {
        self.last_check_ms.load(Ordering::Relaxed)
    }

    /// Record one observation and apply consecutive-threshold transitions.
    /// No-op when health is unmanaged. Concurrency-safe but best-effort: counts
    /// may race slightly at the boundary, which is acceptable for health.
    pub(crate) fn observe(&self, success: bool) {
        if !self.managed {
            return;
        }
        self.last_check_ms.store(now_ms(), Ordering::Relaxed);
        if success {
            self.failures.store(0, Ordering::Relaxed);
            let streak = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.is_healthy() && streak >= self.healthy_threshold {
                self.healthy.store(true, Ordering::Relaxed);
            }
        } else {
            self.successes.store(0, Ordering::Relaxed);
            let streak = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
            if self.is_healthy() && streak >= self.unhealthy_threshold {
                self.healthy.store(false, Ordering::Relaxed);
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn build_health_client(connect_timeout: Duration) -> HealthClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(connect_timeout));
    Client::builder(TokioExecutor::new()).build(connector)
}

/// Background active-health loop for one upstream. Probes every target each
/// interval and feeds the result into its [`HealthState`]. Stops on the global
/// `shutdown` or this generation's `gen_stop` (fired when config is reloaded).
async fn run_health_checks(
    upstream_name: String,
    upstream: Arc<UpstreamRuntime>,
    settings: HealthCheckSettings,
    client: HealthClient,
    mut shutdown: watch::Receiver<bool>,
    mut gen_stop: watch::Receiver<bool>,
) {
    let mut ticker = interval(settings.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = gen_stop.changed() => break,
            _ = ticker.tick() => {
                for target in &upstream.targets {
                    let ok = probe(&client, &target.url, &settings.path, settings.timeout).await;
                    target.state.observe(ok);
                    tracing::debug!(
                        upstream = %upstream_name,
                        target = %target.url,
                        healthy = ok,
                        "health probe"
                    );
                }
            }
        }
    }
}

/// Supervise active health checks across config reloads. Spawns a health loop
/// per opted-in upstream for the current snapshot; on `reload`, stops the old
/// generation and respawns from the new snapshot. Stops all on `shutdown`.
pub(crate) async fn run_health_supervisor(
    shared: SharedConfig,
    connect_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut reload: watch::Receiver<u64>,
) {
    let client = build_health_client(connect_timeout);
    let mut gen_stop: Option<watch::Sender<bool>> = None;

    loop {
        // Stop the previous generation, then (re)spawn for the current snapshot.
        if let Some(stop) = gen_stop.take() {
            let _ = stop.send(true);
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let snapshot = shared.load_full();
        for (name, upstream) in &snapshot.upstreams {
            if let Some(settings) = upstream.health.clone() {
                tracing::info!(upstream = %name, "active health checks enabled");
                tokio::spawn(run_health_checks(
                    name.clone(),
                    upstream.clone(),
                    settings,
                    client.clone(),
                    shutdown.clone(),
                    stop_rx.clone(),
                ));
            }
        }
        gen_stop = Some(stop_tx);

        tokio::select! {
            _ = shutdown.changed() => break,
            result = reload.changed() => {
                if result.is_err() {
                    break; // reload sender dropped
                }
            }
        }
    }
    if let Some(stop) = gen_stop.take() {
        let _ = stop.send(true);
    }
}

async fn probe(client: &HealthClient, base_url: &str, path: &str, timeout: Duration) -> bool {
    let Ok(uri) = probe_uri(base_url, path) else {
        return false;
    };
    let Ok(req) = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Empty::<Bytes>::new())
    else {
        return false;
    };
    matches!(
        tokio::time::timeout(timeout, client.request(req)).await,
        Ok(Ok(resp)) if resp.status().is_success()
    )
}

fn probe_uri(base_url: &str, path: &str) -> Result<Uri, ()> {
    let base: Uri = base_url.parse().map_err(|_| ())?;
    let mut parts = base.into_parts();
    let path = if path.is_empty() { "/" } else { path };
    parts.path_and_query = Some(PathAndQuery::try_from(path).map_err(|_| ())?);
    Uri::from_parts(parts).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_healthy() {
        let s = HealthState::new(true, 2, 3);
        assert!(s.is_healthy());
    }

    #[test]
    fn unmanaged_never_changes() {
        let s = HealthState::new(false, 1, 1);
        s.observe(false);
        s.observe(false);
        assert!(s.is_healthy());
    }

    #[test]
    fn goes_unhealthy_after_consecutive_failures() {
        let s = HealthState::new(true, 2, 3);
        s.observe(false);
        s.observe(false);
        assert!(s.is_healthy(), "still healthy below threshold");
        s.observe(false);
        assert!(!s.is_healthy(), "unhealthy at threshold");
    }

    #[test]
    fn recovers_after_consecutive_successes() {
        let s = HealthState::new(true, 2, 1);
        s.observe(false);
        assert!(!s.is_healthy());
        s.observe(true);
        assert!(!s.is_healthy(), "one success below recover threshold");
        s.observe(true);
        assert!(s.is_healthy(), "recovered at threshold");
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let s = HealthState::new(true, 1, 3);
        s.observe(false);
        s.observe(false);
        s.observe(true); // resets failure streak
        s.observe(false);
        s.observe(false);
        assert!(
            s.is_healthy(),
            "two failures after reset is below threshold of 3"
        );
    }

    #[test]
    fn probe_uri_appends_health_path() {
        let uri = probe_uri("http://backend:4000", "/health").unwrap();
        assert_eq!(uri.to_string(), "http://backend:4000/health");
    }

    #[test]
    fn probe_uri_defaults_empty_path() {
        let uri = probe_uri("http://backend:4000", "").unwrap();
        assert_eq!(uri.to_string(), "http://backend:4000/");
    }

    #[test]
    fn probe_uri_rejects_bad_base() {
        assert!(probe_uri("not a url", "/health").is_err());
    }
}
