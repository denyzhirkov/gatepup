//! Config model, loading and validation for GatePup.
//!
//! `gatepup-config` is the single validated entry point for configuration.
//! It owns the canonical config types; other crates depend on it, never on
//! each other's internal config representation.

mod error;
mod loader;
mod model;
mod validator;

pub use error::{ConfigError, ValidationError};
pub use loader::load_from_file;
pub use model::{
    AdminConfig, AppConfig, GatePupConfig, HealthCheckConfig, ListenerConfig, LoadBalancing,
    MatchConfig, MetricsConfig, Protocol, RetryConfig, RouteConfig, TargetConfig, TimeoutConfig,
    UpstreamConfig,
};
pub use validator::validate;
