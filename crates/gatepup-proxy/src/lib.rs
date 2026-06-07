//! Proxy runtime: immutable config snapshot, route matching, and the
//! hyper-based HTTP server that forwards requests to upstream targets.
//!
//! The runtime reads an `Arc<RuntimeConfig>` snapshot built once from a
//! validated [`gatepup_config::GatePupConfig`]. Hot-swapping the snapshot
//! (config reload) is a later concern; the structure is already immutable.

mod error;
mod health;
mod proxy;
mod router;
mod server;
mod snapshot;

pub use error::ProxyError;
pub use server::{run, serve};
pub use snapshot::{build_snapshot, RouteView, RuntimeConfig, TargetView, UpstreamView};

/// Boxed error used as the unified error type for response bodies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
