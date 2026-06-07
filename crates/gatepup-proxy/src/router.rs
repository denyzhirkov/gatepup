use gatepup_config::RouteConfig;

/// A route compiled for fast matching at request time.
pub(crate) struct CompiledRoute {
    pub(crate) name: String,
    pub(crate) upstream: String,
    /// Exact host to match, or `None` to match any host.
    host: Option<String>,
    /// Path prefix; defaults to `/` (matches every path).
    path_prefix: String,
}

/// Read-only view of a route for admin/introspection.
pub(crate) struct RouteSummary {
    pub(crate) name: String,
    pub(crate) host: Option<String>,
    pub(crate) path_prefix: String,
    pub(crate) upstream: String,
}

impl CompiledRoute {
    fn matches(&self, host: &str, path: &str) -> bool {
        let host_ok = match &self.host {
            Some(h) => h == host,
            None => true,
        };
        host_ok && path.starts_with(&self.path_prefix)
    }

    /// Higher is more specific: a host-constrained route beats a host-agnostic
    /// one, then a longer path prefix beats a shorter one.
    fn specificity(&self) -> (u8, usize) {
        (self.host.is_some() as u8, self.path_prefix.len())
    }
}

/// Per-listener routing table. Picks the most specific matching route.
pub(crate) struct Router {
    routes: Vec<CompiledRoute>,
}

impl Router {
    pub(crate) fn build(routes: &[RouteConfig]) -> Self {
        let routes = routes
            .iter()
            .map(|r| CompiledRoute {
                name: r.name.clone(),
                upstream: r.upstream.clone(),
                host: r.matcher.host.clone().filter(|h| !h.is_empty()),
                path_prefix: r
                    .matcher
                    .path_prefix
                    .clone()
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "/".to_string()),
            })
            .collect();
        Self { routes }
    }

    pub(crate) fn match_route(&self, host: &str, path: &str) -> Option<&CompiledRoute> {
        self.routes
            .iter()
            .filter(|r| r.matches(host, path))
            .max_by_key(|r| r.specificity())
    }

    pub(crate) fn summaries(&self) -> Vec<RouteSummary> {
        self.routes
            .iter()
            .map(|r| RouteSummary {
                name: r.name.clone(),
                host: r.host.clone(),
                path_prefix: r.path_prefix.clone(),
                upstream: r.upstream.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gatepup_config::MatchConfig;

    fn route(name_upstream: &str, host: Option<&str>, prefix: Option<&str>) -> RouteConfig {
        RouteConfig {
            name: name_upstream.to_string(),
            matcher: MatchConfig {
                host: host.map(str::to_string),
                path_prefix: prefix.map(str::to_string),
            },
            upstream: name_upstream.to_string(),
        }
    }

    #[test]
    fn matches_by_host() {
        let router = Router::build(&[
            route("api", Some("api.example.com"), Some("/")),
            route("web", Some("example.com"), Some("/")),
        ]);
        assert_eq!(
            router
                .match_route("api.example.com", "/users")
                .unwrap()
                .upstream,
            "api"
        );
        assert_eq!(
            router.match_route("example.com", "/").unwrap().upstream,
            "web"
        );
    }

    #[test]
    fn returns_none_for_unknown_host() {
        let router = Router::build(&[route("api", Some("api.example.com"), Some("/"))]);
        assert!(router.match_route("nope.example.com", "/").is_none());
    }

    #[test]
    fn prefers_longest_path_prefix() {
        let router = Router::build(&[
            route("root", Some("example.com"), Some("/")),
            route("api", Some("example.com"), Some("/api")),
        ]);
        assert_eq!(
            router
                .match_route("example.com", "/api/v1")
                .unwrap()
                .upstream,
            "api"
        );
        assert_eq!(
            router
                .match_route("example.com", "/other")
                .unwrap()
                .upstream,
            "root"
        );
    }

    #[test]
    fn prefers_host_match_over_wildcard() {
        let router = Router::build(&[
            route("any", None, Some("/")),
            route("exact", Some("example.com"), Some("/")),
        ]);
        assert_eq!(
            router.match_route("example.com", "/").unwrap().upstream,
            "exact"
        );
        assert_eq!(
            router.match_route("other.com", "/").unwrap().upstream,
            "any"
        );
    }
}
