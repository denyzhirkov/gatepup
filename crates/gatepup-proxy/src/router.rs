use gatepup_config::RouteConfig;

/// How a route matches the request host.
enum HostMatch {
    /// Matches any host.
    Any,
    /// Matches one exact host.
    Exact(String),
    /// Matches any subdomain: `*.example.com` -> suffix `.example.com`.
    Wildcard(String),
}

impl HostMatch {
    fn compile(host: Option<&str>) -> Self {
        match host {
            None | Some("") => HostMatch::Any,
            Some(h) if h.starts_with("*.") => HostMatch::Wildcard(h[1..].to_string()),
            Some(h) => HostMatch::Exact(h.to_string()),
        }
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Any => true,
            HostMatch::Exact(h) => h == host,
            // A non-empty label must precede the suffix (excludes the bare domain).
            HostMatch::Wildcard(suffix) => {
                host.len() > suffix.len() && host.ends_with(suffix.as_str())
            }
        }
    }

    /// Specificity rank: exact > wildcard > any.
    fn rank(&self) -> u8 {
        match self {
            HostMatch::Exact(_) => 2,
            HostMatch::Wildcard(_) => 1,
            HostMatch::Any => 0,
        }
    }
}

/// A route compiled for fast matching at request time.
pub(crate) struct CompiledRoute {
    pub(crate) name: String,
    pub(crate) upstream: String,
    /// Original host string, kept for admin introspection.
    host: Option<String>,
    host_match: HostMatch,
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
        self.host_match.matches(host) && path.starts_with(&self.path_prefix)
    }

    /// Higher is more specific: exact host > wildcard host > any host, then a
    /// longer path prefix beats a shorter one.
    fn specificity(&self) -> (u8, usize) {
        (self.host_match.rank(), self.path_prefix.len())
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
            .map(|r| {
                let host = r.matcher.host.clone().filter(|h| !h.is_empty());
                CompiledRoute {
                    name: r.name.clone(),
                    upstream: r.upstream.clone(),
                    host_match: HostMatch::compile(host.as_deref()),
                    host,
                    path_prefix: r
                        .matcher
                        .path_prefix
                        .clone()
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "/".to_string()),
                }
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
    fn wildcard_matches_subdomains_not_bare() {
        let router = Router::build(&[route("wild", Some("*.example.com"), Some("/"))]);
        assert_eq!(
            router.match_route("api.example.com", "/").unwrap().upstream,
            "wild"
        );
        assert_eq!(
            router.match_route("a.b.example.com", "/").unwrap().upstream,
            "wild"
        );
        assert!(
            router.match_route("example.com", "/").is_none(),
            "bare domain must not match the wildcard"
        );
        assert!(
            router.match_route("evilexample.com", "/").is_none(),
            "suffix without a dot boundary must not match"
        );
    }

    #[test]
    fn exact_beats_wildcard_beats_any() {
        let router = Router::build(&[
            route("any", None, Some("/")),
            route("wild", Some("*.example.com"), Some("/")),
            route("exact", Some("api.example.com"), Some("/")),
        ]);
        assert_eq!(
            router.match_route("api.example.com", "/").unwrap().upstream,
            "exact"
        );
        assert_eq!(
            router
                .match_route("other.example.com", "/")
                .unwrap()
                .upstream,
            "wild"
        );
        assert_eq!(router.match_route("foo.com", "/").unwrap().upstream, "any");
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
