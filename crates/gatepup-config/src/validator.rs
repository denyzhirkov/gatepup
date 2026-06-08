use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use crate::error::ValidationError;
use crate::model::{GatePupConfig, HealthCheckConfig};

/// Validate a parsed config. Returns every problem found, not just the first,
/// so the user can fix the whole file in one pass.
pub fn validate(config: &GatePupConfig) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let upstream_names: HashSet<&str> = config.upstreams.iter().map(|u| u.name.as_str()).collect();

    check_app(config, &mut errors);
    check_unique_upstreams(config, &mut errors);
    check_listeners(config, &upstream_names, &mut errors);
    check_upstreams(config, &mut errors);
    check_admin(config, &mut errors);
    check_timeouts(config, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_app(config: &GatePupConfig, errors: &mut Vec<ValidationError>) {
    const LEVELS: [&str; 6] = ["trace", "debug", "info", "warn", "error", "off"];
    if !LEVELS.contains(&config.app.log_level.to_ascii_lowercase().as_str()) {
        errors.push(ValidationError::InvalidLogLevel {
            value: config.app.log_level.clone(),
        });
    }
}

fn check_unique_upstreams(config: &GatePupConfig, errors: &mut Vec<ValidationError>) {
    let mut seen = HashSet::new();
    for upstream in &config.upstreams {
        if !seen.insert(upstream.name.as_str()) {
            errors.push(ValidationError::DuplicateUpstreamName(
                upstream.name.clone(),
            ));
        }
    }
}

fn check_listeners(
    config: &GatePupConfig,
    upstream_names: &HashSet<&str>,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen_listeners = HashSet::new();
    for listener in &config.listeners {
        if !seen_listeners.insert(listener.name.as_str()) {
            errors.push(ValidationError::DuplicateListenerName(
                listener.name.clone(),
            ));
        }

        if listener.bind.parse::<SocketAddr>().is_err() {
            errors.push(ValidationError::InvalidBind {
                listener: listener.name.clone(),
                bind: listener.bind.clone(),
            });
        }

        let mut seen_routes = HashSet::new();
        let mut seen_matches: HashMap<(Option<String>, String), String> = HashMap::new();
        for route in &listener.routes {
            if !seen_routes.insert(route.name.as_str()) {
                errors.push(ValidationError::DuplicateRouteName {
                    listener: listener.name.clone(),
                    route: route.name.clone(),
                });
            }

            if route.matcher.is_empty() {
                errors.push(ValidationError::EmptyRouteMatch {
                    listener: listener.name.clone(),
                    route: route.name.clone(),
                });
            }

            if let Some(prefix) = &route.matcher.path_prefix {
                if !prefix.is_empty() && !prefix.starts_with('/') {
                    errors.push(ValidationError::InvalidPathPrefix {
                        listener: listener.name.clone(),
                        route: route.name.clone(),
                        prefix: prefix.clone(),
                    });
                }
            }

            // Two routes with the same effective (host, path prefix) are ambiguous.
            let effective_prefix = route
                .matcher
                .path_prefix
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "/".to_string());
            let key = (route.matcher.host.clone(), effective_prefix);
            if let Some(first) = seen_matches.get(&key) {
                errors.push(ValidationError::ConflictingRoutes {
                    listener: listener.name.clone(),
                    first: first.clone(),
                    second: route.name.clone(),
                });
            } else {
                seen_matches.insert(key, route.name.clone());
            }

            if !upstream_names.contains(route.upstream.as_str()) {
                errors.push(ValidationError::UnknownUpstream {
                    route: route.name.clone(),
                    upstream: route.upstream.clone(),
                });
            }
        }
    }
}

fn check_upstreams(config: &GatePupConfig, errors: &mut Vec<ValidationError>) {
    for upstream in &config.upstreams {
        if upstream.targets.is_empty() {
            errors.push(ValidationError::EmptyTargets {
                upstream: upstream.name.clone(),
            });
        }

        for target in &upstream.targets {
            if !is_valid_target_url(&target.url) {
                errors.push(ValidationError::InvalidTargetUrl {
                    upstream: upstream.name.clone(),
                    url: target.url.clone(),
                });
            }
            if target.weight == 0 {
                errors.push(ValidationError::ZeroWeight {
                    upstream: upstream.name.clone(),
                    url: target.url.clone(),
                });
            }
        }

        if let Some(hc) = &upstream.health_check {
            check_health_timeout(&upstream.name, hc, errors);
        }
    }
}

fn check_health_timeout(upstream: &str, hc: &HealthCheckConfig, errors: &mut Vec<ValidationError>) {
    let interval_ms = hc.interval_seconds.saturating_mul(1000);
    if hc.timeout_ms >= interval_ms {
        errors.push(ValidationError::HealthTimeoutNotLessThanInterval {
            upstream: upstream.to_string(),
            timeout_ms: hc.timeout_ms,
            interval_ms,
        });
    }
}

fn check_admin(config: &GatePupConfig, errors: &mut Vec<ValidationError>) {
    if let Some(admin) = &config.admin {
        if admin.bind.parse::<SocketAddr>().is_err() {
            errors.push(ValidationError::InvalidAdminBind {
                bind: admin.bind.clone(),
            });
        }
    }
}

fn check_timeouts(config: &GatePupConfig, errors: &mut Vec<ValidationError>) {
    if config.timeouts.connect_timeout_ms == 0 {
        errors.push(ValidationError::ZeroTimeout {
            field: "connectTimeoutMs",
        });
    }
    if config.timeouts.request_timeout_ms == 0 {
        errors.push(ValidationError::ZeroTimeout {
            field: "requestTimeoutMs",
        });
    }
}

fn is_valid_target_url(raw: &str) -> bool {
    match url::Url::parse(raw) {
        Ok(url) => matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn target(url: &str) -> TargetConfig {
        TargetConfig {
            url: url.to_string(),
            weight: 1,
        }
    }

    fn upstream(name: &str, targets: Vec<TargetConfig>) -> UpstreamConfig {
        UpstreamConfig {
            name: name.to_string(),
            load_balancing: LoadBalancing::RoundRobin,
            targets,
            health_check: None,
        }
    }

    fn route(name: &str, host: &str, upstream: &str) -> RouteConfig {
        RouteConfig {
            name: name.to_string(),
            matcher: MatchConfig {
                host: Some(host.to_string()),
                path_prefix: Some("/".to_string()),
            },
            upstream: upstream.to_string(),
        }
    }

    fn valid_config() -> GatePupConfig {
        GatePupConfig {
            app: AppConfig {
                name: "gatepup".to_string(),
                log_level: "info".to_string(),
            },
            listeners: vec![ListenerConfig {
                name: "public".to_string(),
                bind: "0.0.0.0:80".to_string(),
                protocol: Protocol::Http,
                routes: vec![route("api", "api.example.com", "api")],
            }],
            upstreams: vec![upstream("api", vec![target("http://api-1:4000")])],
            timeouts: Default::default(),
            admin: None,
            metrics: None,
        }
    }

    #[test]
    fn accepts_a_valid_config() {
        assert_eq!(validate(&valid_config()), Ok(()));
    }

    #[test]
    fn rejects_unknown_upstream() {
        let mut cfg = valid_config();
        cfg.listeners[0].routes[0].upstream = "missing".to_string();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::UnknownUpstream {
            route: "api".to_string(),
            upstream: "missing".to_string(),
        }));
    }

    #[test]
    fn rejects_duplicate_upstream_names() {
        let mut cfg = valid_config();
        cfg.upstreams
            .push(upstream("api", vec![target("http://api-2:4000")]));
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::DuplicateUpstreamName("api".to_string())));
    }

    #[test]
    fn rejects_duplicate_route_names_in_listener() {
        let mut cfg = valid_config();
        cfg.listeners[0]
            .routes
            .push(route("api", "other.example.com", "api"));
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::DuplicateRouteName {
            listener: "public".to_string(),
            route: "api".to_string(),
        }));
    }

    #[test]
    fn rejects_invalid_bind() {
        let mut cfg = valid_config();
        cfg.listeners[0].bind = "not-an-address".to_string();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::InvalidBind {
            listener: "public".to_string(),
            bind: "not-an-address".to_string(),
        }));
    }

    #[test]
    fn rejects_empty_targets() {
        let mut cfg = valid_config();
        cfg.upstreams[0].targets.clear();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::EmptyTargets {
            upstream: "api".to_string(),
        }));
    }

    #[test]
    fn rejects_invalid_target_url() {
        let mut cfg = valid_config();
        cfg.upstreams[0].targets[0].url = "://broken".to_string();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::InvalidTargetUrl {
            upstream: "api".to_string(),
            url: "://broken".to_string(),
        }));
    }

    #[test]
    fn rejects_empty_route_match() {
        let mut cfg = valid_config();
        cfg.listeners[0].routes[0].matcher = MatchConfig::default();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::EmptyRouteMatch {
            listener: "public".to_string(),
            route: "api".to_string(),
        }));
    }

    #[test]
    fn rejects_health_timeout_not_less_than_interval() {
        let mut cfg = valid_config();
        cfg.upstreams[0].health_check = Some(HealthCheckConfig {
            enabled: true,
            path: "/health".to_string(),
            interval_seconds: 1,
            timeout_ms: 2000,
            healthy_threshold: 2,
            unhealthy_threshold: 3,
        });
        let errors = validate(&cfg).unwrap_err();
        assert!(
            errors.contains(&ValidationError::HealthTimeoutNotLessThanInterval {
                upstream: "api".to_string(),
                timeout_ms: 2000,
                interval_ms: 1000,
            })
        );
    }

    #[test]
    fn rejects_invalid_log_level() {
        let mut cfg = valid_config();
        cfg.app.log_level = "verbose".to_string();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::InvalidLogLevel {
            value: "verbose".to_string(),
        }));
    }

    #[test]
    fn accepts_log_level_case_insensitively() {
        let mut cfg = valid_config();
        cfg.app.log_level = "INFO".to_string();
        assert_eq!(validate(&cfg), Ok(()));
    }

    #[test]
    fn rejects_path_prefix_without_leading_slash() {
        let mut cfg = valid_config();
        cfg.listeners[0].routes[0].matcher.path_prefix = Some("api".to_string());
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::InvalidPathPrefix {
            listener: "public".to_string(),
            route: "api".to_string(),
            prefix: "api".to_string(),
        }));
    }

    #[test]
    fn rejects_conflicting_routes_with_same_match() {
        let mut cfg = valid_config();
        // Second route, different name, identical host + path prefix.
        cfg.listeners[0]
            .routes
            .push(route("api2", "api.example.com", "api"));
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::ConflictingRoutes {
            listener: "public".to_string(),
            first: "api".to_string(),
            second: "api2".to_string(),
        }));
    }

    #[test]
    fn rejects_zero_weight_target() {
        let mut cfg = valid_config();
        cfg.upstreams[0].targets[0].weight = 0;
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::ZeroWeight {
            upstream: "api".to_string(),
            url: "http://api-1:4000".to_string(),
        }));
    }

    #[test]
    fn rejects_zero_timeouts() {
        let mut cfg = valid_config();
        cfg.timeouts.request_timeout_ms = 0;
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.contains(&ValidationError::ZeroTimeout {
            field: "requestTimeoutMs",
        }));
    }

    #[test]
    fn reports_multiple_errors_at_once() {
        let mut cfg = valid_config();
        cfg.listeners[0].bind = "bad".to_string();
        cfg.upstreams[0].targets.clear();
        let errors = validate(&cfg).unwrap_err();
        assert!(errors.len() >= 2);
    }
}
