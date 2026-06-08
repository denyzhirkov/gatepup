use serde::{Deserialize, Serialize};

/// Root configuration document. Deserialized from JSON, then validated before
/// it is turned into a runtime snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatePupConfig {
    pub app: AppConfig,
    pub listeners: Vec<ListenerConfig>,
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
}

/// Proxy timeouts. `connect` bounds establishing the upstream connection;
/// `request` bounds the whole upstream round-trip (mapped to 504 on expiry).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimeoutConfig {
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

fn default_connect_timeout_ms() -> u64 {
    5_000
}

fn default_request_timeout_ms() -> u64 {
    30_000
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: default_connect_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListenerConfig {
    pub name: String,
    pub bind: String,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Http,
    Https,
}

/// TLS material for an `https` listener: PEM file paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub matcher: MatchConfig,
    pub upstream: String,
    /// Strip the matched `pathPrefix` from the path before forwarding.
    #[serde(default)]
    pub strip_prefix: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
}

impl MatchConfig {
    /// A match is empty when it constrains nothing — neither host nor path.
    pub fn is_empty(&self) -> bool {
        let host_empty = self.host.as_deref().map(str::is_empty).unwrap_or(true);
        let path_empty = self
            .path_prefix
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true);
        host_empty && path_empty
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    #[serde(default)]
    pub load_balancing: LoadBalancing,
    pub targets: Vec<TargetConfig>,
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    #[serde(default)]
    pub retries: Option<RetryConfig>,
}

/// Retry policy for an upstream. Off unless present and `enabled`. `attempts` is
/// the max total number of tries (>= 1). Only `methods` (default: idempotent)
/// are retried, on the conditions in `retryOn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetryConfig {
    pub enabled: bool,
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_retry_methods")]
    pub methods: Vec<String>,
    #[serde(default = "default_retry_on")]
    pub retry_on: Vec<String>,
}

fn default_retry_attempts() -> u32 {
    2
}

fn default_retry_methods() -> Vec<String> {
    vec!["GET".to_string(), "HEAD".to_string(), "OPTIONS".to_string()]
}

fn default_retry_on() -> Vec<String> {
    vec!["connect_error".to_string(), "upstream_5xx".to_string()]
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancing {
    #[default]
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetConfig {
    pub url: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub path: String,
    pub interval_seconds: u64,
    pub timeout_ms: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminConfig {
    pub enabled: bool,
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub path: String,
}
