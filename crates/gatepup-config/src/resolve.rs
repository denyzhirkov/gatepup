//! Resolve the effective config from `--config` and/or the environment.
//!
//! Precedence for the base config: `GATEPUP_CONFIG` (file path) >
//! `GATEPUP_CONFIG_JSON` (inline) > `--config` > simple env-mode
//! (`GATEPUP_LISTEN` / `GATEPUP_UPSTREAM`). Scalar overrides
//! (`GATEPUP_LOG`, `GATEPUP_ADMIN`, `GATEPUP_METRICS_PATH`,
//! `GATEPUP_TIMEOUT_*_MS`) are then applied on top. Env reads go through a
//! single injectable getter so the logic is unit-testable without process env.

use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::loader::load_from_file;
use crate::model::*;

/// Where the resolved config came from (used for reload + startup logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    Inline,
    Env,
}

/// Resolve the effective (unvalidated) config from `--config` and the
/// environment. Validate the result before use.
pub fn resolve_config(
    cli_path: Option<&Path>,
) -> Result<(GatePupConfig, ConfigSource), ConfigError> {
    resolve_with(cli_path, &|key| std::env::var(key).ok())
}

fn resolve_with(
    cli_path: Option<&Path>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(GatePupConfig, ConfigSource), ConfigError> {
    let (mut config, source) = base_config(cli_path, get)?;
    apply_overrides(&mut config, get)?;
    Ok((config, source))
}

fn base_config(
    cli_path: Option<&Path>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(GatePupConfig, ConfigSource), ConfigError> {
    if let Some(path) = get("GATEPUP_CONFIG") {
        let path = PathBuf::from(path);
        let config = load_from_file(&path)?;
        return Ok((config, ConfigSource::File(path)));
    }
    if let Some(json) = get("GATEPUP_CONFIG_JSON") {
        let config =
            serde_json::from_str(&json).map_err(|source| ConfigError::ParseInline { source })?;
        return Ok((config, ConfigSource::Inline));
    }
    if let Some(path) = cli_path {
        let config = load_from_file(path)?;
        return Ok((config, ConfigSource::File(path.to_path_buf())));
    }
    if get("GATEPUP_LISTEN").is_some() || get("GATEPUP_UPSTREAM").is_some() {
        return Ok((build_from_env(get), ConfigSource::Env));
    }
    Err(ConfigError::NoConfigSource)
}

/// Build a minimal single-listener config from env (simple mode).
fn build_from_env(get: &dyn Fn(&str) -> Option<String>) -> GatePupConfig {
    let bind = get("GATEPUP_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_string());
    let targets = get("GATEPUP_UPSTREAM")
        .map(|csv| {
            csv.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|url| TargetConfig {
                    url: url.to_string(),
                    weight: 1,
                })
                .collect()
        })
        .unwrap_or_default();
    let tls = match (get("GATEPUP_TLS_CERT"), get("GATEPUP_TLS_KEY")) {
        (Some(cert), Some(key)) => Some(TlsConfig { cert, key }),
        _ => None,
    };
    let protocol = if tls.is_some() {
        Protocol::Https
    } else {
        Protocol::Http
    };
    let health_check = get("GATEPUP_HEALTHCHECK_PATH").map(|path| HealthCheckConfig {
        enabled: true,
        path,
        interval_seconds: 10,
        timeout_ms: 1000,
        healthy_threshold: 2,
        unhealthy_threshold: 3,
    });

    GatePupConfig {
        app: AppConfig {
            name: "gatepup".to_string(),
            log_level: "info".to_string(),
        },
        listeners: vec![ListenerConfig {
            name: "default".to_string(),
            bind,
            protocol,
            tls,
            routes: vec![RouteConfig {
                name: "default".to_string(),
                matcher: MatchConfig {
                    host: get("GATEPUP_ROUTE_HOST"),
                    path_prefix: Some("/".to_string()),
                },
                upstream: "default".to_string(),
                strip_prefix: false,
                headers: None,
                ip_access: None,
                rate_limit: None,
                basic_auth: None,
            }],
        }],
        upstreams: vec![UpstreamConfig {
            name: "default".to_string(),
            load_balancing: LoadBalancing::RoundRobin,
            targets,
            health_check,
            retries: None,
            tls_insecure_skip_verify: false,
        }],
        timeouts: TimeoutConfig::default(),
        limits: LimitsConfig::default(),
        trusted_proxies: Vec::new(),
        compression: None,
        admin: None,
        metrics: None,
    }
}

/// Apply unambiguous scalar env overrides on top of the base config.
fn apply_overrides(
    config: &mut GatePupConfig,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ConfigError> {
    if let Some(level) = get("GATEPUP_LOG") {
        config.app.log_level = level;
    }
    if let Some(bind) = get("GATEPUP_ADMIN") {
        config.admin = Some(AdminConfig {
            enabled: true,
            bind,
            token: get("GATEPUP_ADMIN_TOKEN"),
        });
    }
    if let Some(path) = get("GATEPUP_METRICS_PATH") {
        config.metrics = Some(MetricsConfig {
            enabled: true,
            path,
        });
    }
    if let Some(value) = get("GATEPUP_TIMEOUT_CONNECT_MS") {
        config.timeouts.connect_timeout_ms = parse_ms("GATEPUP_TIMEOUT_CONNECT_MS", &value)?;
    }
    if let Some(value) = get("GATEPUP_TIMEOUT_REQUEST_MS") {
        config.timeouts.request_timeout_ms = parse_ms("GATEPUP_TIMEOUT_REQUEST_MS", &value)?;
    }
    Ok(())
}

fn parse_ms(var: &'static str, value: &str) -> Result<u64, ConfigError> {
    value.parse().map_err(|_| ConfigError::BadEnvValue {
        var,
        value: value.to_string(),
        reason: "expected a non-negative integer (milliseconds)",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn no_source_errors() {
        let get = env(&[]);
        assert!(matches!(
            resolve_with(None, &get),
            Err(ConfigError::NoConfigSource)
        ));
    }

    #[test]
    fn simple_mode_builds_and_validates() {
        let get = env(&[
            ("GATEPUP_LISTEN", "0.0.0.0:80"),
            ("GATEPUP_UPSTREAM", "http://a:1, http://b:2"),
            ("GATEPUP_ROUTE_HOST", "ex.com"),
        ]);
        let (cfg, source) = resolve_with(None, &get).unwrap();
        assert_eq!(source, ConfigSource::Env);
        assert_eq!(cfg.listeners[0].bind, "0.0.0.0:80");
        assert_eq!(cfg.upstreams[0].targets.len(), 2);
        assert_eq!(
            cfg.listeners[0].routes[0].matcher.host.as_deref(),
            Some("ex.com")
        );
        crate::validate(&cfg).expect("env-built config should validate");
    }

    #[test]
    fn scalar_overrides_apply() {
        let get = env(&[
            ("GATEPUP_UPSTREAM", "http://a:1"),
            ("GATEPUP_LOG", "debug"),
            ("GATEPUP_ADMIN", "127.0.0.1:9090"),
            ("GATEPUP_METRICS_PATH", "/m"),
            ("GATEPUP_TIMEOUT_REQUEST_MS", "5000"),
        ]);
        let (cfg, _) = resolve_with(None, &get).unwrap();
        assert_eq!(cfg.app.log_level, "debug");
        assert_eq!(cfg.admin.unwrap().bind, "127.0.0.1:9090");
        assert_eq!(cfg.metrics.unwrap().path, "/m");
        assert_eq!(cfg.timeouts.request_timeout_ms, 5000);
    }

    #[test]
    fn simple_mode_https_when_tls_set() {
        let get = env(&[
            ("GATEPUP_UPSTREAM", "http://a:1"),
            ("GATEPUP_TLS_CERT", "/c.pem"),
            ("GATEPUP_TLS_KEY", "/k.pem"),
        ]);
        let (cfg, _) = resolve_with(None, &get).unwrap();
        assert_eq!(cfg.listeners[0].protocol, Protocol::Https);
        assert!(cfg.listeners[0].tls.is_some());
    }

    #[test]
    fn inline_json_wins_over_cli_path() {
        let json = r#"{"app":{"name":"inline"},"listeners":[{"name":"l","bind":"0.0.0.0:80","routes":[{"name":"r","match":{"pathPrefix":"/"},"upstream":"u"}]}],"upstreams":[{"name":"u","targets":[{"url":"http://a:1"}]}]}"#;
        let get = env(&[("GATEPUP_CONFIG_JSON", json)]);
        let (cfg, source) = resolve_with(Some(Path::new("/no/such.json")), &get).unwrap();
        assert_eq!(source, ConfigSource::Inline);
        assert_eq!(cfg.app.name, "inline");
    }

    #[test]
    fn bad_timeout_env_is_named_error() {
        let get = env(&[
            ("GATEPUP_UPSTREAM", "http://a:1"),
            ("GATEPUP_TIMEOUT_CONNECT_MS", "soon"),
        ]);
        assert!(matches!(
            resolve_with(None, &get),
            Err(ConfigError::BadEnvValue {
                var: "GATEPUP_TIMEOUT_CONNECT_MS",
                ..
            })
        ));
    }
}
