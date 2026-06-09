use std::path::PathBuf;

use thiserror::Error;

/// Failure while reading or parsing a config file. Validation failures are
/// reported separately via [`ValidationError`].
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse GATEPUP_CONFIG_JSON: {source}")]
    ParseInline {
        #[source]
        source: serde_json::Error,
    },

    #[error("no config source: set --config, GATEPUP_CONFIG, GATEPUP_CONFIG_JSON, or GATEPUP_LISTEN/GATEPUP_UPSTREAM")]
    NoConfigSource,

    #[error("environment variable {var} has an invalid value {value:?}: {reason}")]
    BadEnvValue {
        var: &'static str,
        value: String,
        reason: &'static str,
    },
}

/// A single, user-facing config validation problem. Messages name the
/// offending listener / route / upstream so the user can fix it directly.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("duplicate listener name {0:?}")]
    DuplicateListenerName(String),

    #[error("duplicate upstream name {0:?}")]
    DuplicateUpstreamName(String),

    #[error("duplicate route name {route:?} in listener {listener:?}")]
    DuplicateRouteName { listener: String, route: String },

    #[error("route {route:?} references unknown upstream {upstream:?}")]
    UnknownUpstream { route: String, upstream: String },

    #[error("listener {listener:?} has invalid bind address {bind:?}")]
    InvalidBind { listener: String, bind: String },

    #[error("admin has invalid bind address {bind:?}")]
    InvalidAdminBind { bind: String },

    #[error("upstream {upstream:?} has invalid target url {url:?}")]
    InvalidTargetUrl { upstream: String, url: String },

    #[error("upstream {upstream:?} has no targets")]
    EmptyTargets { upstream: String },

    #[error("route {route:?} in listener {listener:?} has an empty match")]
    EmptyRouteMatch { listener: String, route: String },

    #[error(
        "upstream {upstream:?} health check timeout ({timeout_ms}ms) must be less than interval ({interval_ms}ms)"
    )]
    HealthTimeoutNotLessThanInterval {
        upstream: String,
        timeout_ms: u64,
        interval_ms: u64,
    },

    #[error("timeout {field:?} must be greater than zero")]
    ZeroTimeout { field: &'static str },

    #[error("limits maxHeaderBytes {value} must be 0 (hyper default) or at least 8192")]
    MaxHeaderBytesTooSmall { value: usize },

    #[error(
        "trustedProxies entry {value:?} is not a valid IP or CIDR (e.g. 10.0.0.0/8 or 203.0.113.7)"
    )]
    InvalidTrustedProxy { value: String },

    #[error("upstream {upstream:?} target {url:?} has weight 0 (must be >= 1)")]
    ZeroWeight { upstream: String, url: String },

    #[error("invalid logLevel {value:?} (expected one of: trace, debug, info, warn, error, off)")]
    InvalidLogLevel { value: String },

    #[error("route {route:?} in listener {listener:?} has pathPrefix {prefix:?} that must start with '/'")]
    InvalidPathPrefix {
        listener: String,
        route: String,
        prefix: String,
    },

    #[error("routes {first:?} and {second:?} in listener {listener:?} have the same match (host + path prefix)")]
    ConflictingRoutes {
        listener: String,
        first: String,
        second: String,
    },

    #[error("route {route:?} in listener {listener:?} has invalid wildcard host {host:?} (only a leading '*.' wildcard is allowed, e.g. *.example.com)")]
    InvalidWildcardHost {
        listener: String,
        route: String,
        host: String,
    },

    #[error("upstream {upstream:?} retry attempts must be >= 1")]
    InvalidRetryAttempts { upstream: String },

    #[error("upstream {upstream:?} has an empty retry {field:?} list")]
    EmptyRetryList {
        upstream: String,
        field: &'static str,
    },

    #[error("upstream {upstream:?} has invalid retry method {method:?}")]
    InvalidRetryMethod { upstream: String, method: String },

    #[error("upstream {upstream:?} has invalid retryOn condition {value:?} (expected: connect_error, connect_timeout, upstream_5xx)")]
    InvalidRetryOn { upstream: String, value: String },

    #[error("listener {listener:?} uses protocol https but has no tls block")]
    MissingTls { listener: String },

    #[error("listener {listener:?} uses protocol http but has a tls block")]
    UnexpectedTls { listener: String },

    #[error("listener {listener:?} has an empty tls {field:?} path")]
    EmptyTlsPath {
        listener: String,
        field: &'static str,
    },

    #[error("route {route:?} in listener {listener:?} has an invalid header name {name:?}")]
    InvalidHeaderName {
        listener: String,
        route: String,
        name: String,
    },

    #[error("route {route:?} in listener {listener:?} has an invalid value for header {name:?}")]
    InvalidHeaderValue {
        listener: String,
        route: String,
        name: String,
    },

    #[error("route {route:?} in listener {listener:?} has an invalid ipAccess {field} entry {value:?} (expected IP or CIDR)")]
    InvalidIpAccessCidr {
        listener: String,
        route: String,
        field: &'static str,
        value: String,
    },
}
