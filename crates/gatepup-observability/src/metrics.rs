use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("failed to build metrics: {0}")]
pub struct MetricsError(#[from] prometheus::Error);

/// Prometheus metrics for the proxy. Counters are incremented from the request
/// path; the per-upstream health gauge is set by the metrics endpoint at scrape
/// time (this crate stays unaware of the runtime snapshot).
pub struct Metrics {
    registry: Registry,
    requests_total: IntCounter,
    request_duration: Histogram,
    upstream_requests_total: IntCounter,
    upstream_errors_total: IntCounter,
    upstream_retries_total: IntCounter,
    route_not_found_total: IntCounter,
    upstream_healthy: IntGaugeVec,
    config_reloads_total: IntCounterVec,
    websocket_connections_total: IntCounter,
}

impl Metrics {
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        let requests_total = IntCounter::new("gatepup_requests_total", "Total requests received")?;
        let request_duration = Histogram::with_opts(HistogramOpts::new(
            "gatepup_request_duration_seconds",
            "Request duration in seconds",
        ))?;
        let upstream_requests_total = IntCounter::new(
            "gatepup_upstream_requests_total",
            "Requests forwarded to an upstream",
        )?;
        let upstream_errors_total = IntCounter::new(
            "gatepup_upstream_errors_total",
            "Upstream failures (connect/timeout/5xx mapped to gateway errors)",
        )?;
        let upstream_retries_total = IntCounter::new(
            "gatepup_upstream_retries_total",
            "Upstream attempts that were retried onto another target",
        )?;
        let route_not_found_total = IntCounter::new(
            "gatepup_route_not_found_total",
            "Requests that matched no route",
        )?;
        let upstream_healthy = IntGaugeVec::new(
            Opts::new(
                "gatepup_upstream_healthy",
                "Number of healthy targets per upstream",
            ),
            &["upstream"],
        )?;
        let config_reloads_total = IntCounterVec::new(
            Opts::new("gatepup_config_reloads_total", "Config reload attempts"),
            &["result"],
        )?;
        let websocket_connections_total = IntCounter::new(
            "gatepup_websocket_connections_total",
            "Upgraded (WebSocket) connections tunneled",
        )?;

        registry.register(Box::new(requests_total.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(upstream_requests_total.clone()))?;
        registry.register(Box::new(upstream_errors_total.clone()))?;
        registry.register(Box::new(upstream_retries_total.clone()))?;
        registry.register(Box::new(route_not_found_total.clone()))?;
        registry.register(Box::new(upstream_healthy.clone()))?;
        registry.register(Box::new(config_reloads_total.clone()))?;
        registry.register(Box::new(websocket_connections_total.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            request_duration,
            upstream_requests_total,
            upstream_errors_total,
            upstream_retries_total,
            route_not_found_total,
            upstream_healthy,
            config_reloads_total,
            websocket_connections_total,
        })
    }

    pub fn inc_requests(&self) {
        self.requests_total.inc();
    }

    pub fn observe_duration(&self, seconds: f64) {
        self.request_duration.observe(seconds);
    }

    pub fn inc_upstream_requests(&self) {
        self.upstream_requests_total.inc();
    }

    pub fn inc_upstream_errors(&self) {
        self.upstream_errors_total.inc();
    }

    pub fn inc_upstream_retries(&self) {
        self.upstream_retries_total.inc();
    }

    pub fn inc_route_not_found(&self) {
        self.route_not_found_total.inc();
    }

    pub fn inc_websocket(&self) {
        self.websocket_connections_total.inc();
    }

    pub fn inc_config_reload(&self, success: bool) {
        let result = if success { "success" } else { "failure" };
        self.config_reloads_total.with_label_values(&[result]).inc();
    }

    pub fn set_upstream_healthy(&self, upstream: &str, healthy: i64) {
        self.upstream_healthy
            .with_label_values(&[upstream])
            .set(healthy);
    }

    /// Encode all metrics in Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        let _ = encoder.encode(&self.registry.gather(), &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_registered_metrics() {
        let m = Metrics::new().unwrap();
        m.inc_requests();
        m.inc_upstream_requests();
        m.set_upstream_healthy("api", 2);
        let text = m.encode();
        assert!(text.contains("gatepup_requests_total 1"));
        assert!(text.contains("gatepup_upstream_requests_total 1"));
        assert!(text.contains("gatepup_upstream_healthy{upstream=\"api\"} 2"));
    }

    #[test]
    fn duration_histogram_is_exposed() {
        let m = Metrics::new().unwrap();
        m.observe_duration(0.012);
        let text = m.encode();
        assert!(text.contains("gatepup_request_duration_seconds_count 1"));
    }
}
