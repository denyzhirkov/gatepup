//! Observability: structured logging setup and Prometheus metrics.
//!
//! This crate is a leaf — it knows nothing about the proxy runtime. The metrics
//! it exposes are plain counters/gauges; whoever holds the runtime snapshot
//! (the admin server) sets the per-upstream health gauge at scrape time.

mod logging;
mod metrics;

pub use logging::init_logging;
pub use metrics::{Metrics, MetricsError};
