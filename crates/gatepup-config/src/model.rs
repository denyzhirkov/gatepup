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
    pub limits: LimitsConfig,
    /// CIDRs (or bare IPs) of proxies in front of GatePup whose `X-Forwarded-For`
    /// is trusted when resolving the real client IP. Empty (default) means the
    /// direct TCP peer is always treated as the client.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
}

/// Inbound resource limits (DoS hardening). Distinct from `timeouts`, which
/// bound the *upstream* round-trip; these bound what a *client* can consume.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitsConfig {
    /// Max request body size in bytes; a larger body is rejected with 413.
    /// `0` (default) means unlimited — bodies stream through unbounded.
    #[serde(default)]
    pub max_body_bytes: u64,
    /// Max time to read the full request header from a client (slowloris guard).
    /// `0` disables it. Default 15s.
    #[serde(default = "default_header_read_timeout_ms")]
    pub header_read_timeout_ms: u64,
    /// Max bytes for the connection read buffer, which bounds the request header
    /// section. `0` (default) keeps hyper's built-in ~400KB bound. When set, must
    /// be at least 8192 (hyper's floor).
    #[serde(default)]
    pub max_header_bytes: usize,
}

fn default_header_read_timeout_ms() -> u64 {
    15_000
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 0,
            header_read_timeout_ms: default_header_read_timeout_ms(),
            max_header_bytes: 0,
        }
    }
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
    /// Header rewrites applied to the upstream request and the client response.
    #[serde(default)]
    pub headers: Option<HeaderRulesConfig>,
}

/// Per-route header rewrites: applied to the request before forwarding upstream
/// and to the response before returning to the client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeaderRulesConfig {
    #[serde(default)]
    pub request: HeaderOpsConfig,
    #[serde(default)]
    pub response: HeaderOpsConfig,
}

/// `set` overwrites (or inserts) a header; `remove` deletes it. `set` is applied
/// after `remove`. Names/values are validated as valid HTTP header tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeaderOpsConfig {
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
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
