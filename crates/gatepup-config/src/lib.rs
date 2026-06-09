//! Config model, loading and validation for GatePup.
//!
//! `gatepup-config` is the single validated entry point for configuration.
//! It owns the canonical config types; other crates depend on it, never on
//! each other's internal config representation.

mod error;
mod loader;
mod model;
mod resolve;
mod validator;

pub use error::{ConfigError, ValidationError};
pub use loader::load_from_file;
pub use model::{
    AdminConfig, AppConfig, GatePupConfig, HeaderOpsConfig, HeaderRulesConfig, HealthCheckConfig,
    IpAccessConfig, LimitsConfig, ListenerConfig, LoadBalancing, MatchConfig, MetricsConfig,
    Protocol, RetryConfig, RouteConfig, TargetConfig, TimeoutConfig, TlsConfig, UpstreamConfig,
};
pub use resolve::{resolve_config, ConfigSource};
pub use validator::{parse_trusted_proxy, validate};
