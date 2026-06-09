//! Proxy runtime: an immutable config snapshot behind a lock-free swap cell,
//! route matching, and the hyper-based HTTP server that forwards requests to
//! upstream targets.
//!
//! The runtime reads an `Arc<RuntimeConfig>` snapshot built from a validated
//! [`gatepup_config::GatePupConfig`]. The snapshot lives in a [`SharedConfig`]
//! ([`arc_swap::ArcSwap`]) so it can be hot-swapped on config reload: handlers
//! load the current snapshot per request; in-flight requests keep the old one.

mod compress;
mod error;
mod health;
mod proxy;
mod rate_limit;
mod router;
mod server;
mod snapshot;
mod tls;
mod upstream_tls;

use std::sync::Arc;

pub use error::ProxyError;
pub use server::{run, serve, serve_shared};
pub use snapshot::{
    build_reload_snapshot, build_snapshot, RouteView, RuntimeConfig, TargetView, UpstreamView,
};

/// Boxed error used as the unified error type for response bodies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The runtime config snapshot behind a lock-free swap cell. Reload stores a new
/// snapshot; request handlers load the current one per request.
pub type SharedConfig = Arc<arc_swap::ArcSwap<RuntimeConfig>>;
