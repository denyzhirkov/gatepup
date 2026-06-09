use std::net::IpAddr;

use gatepup_config::{parse_trusted_proxy, HeaderOpsConfig, IpAccessConfig, RouteConfig};
use http::{HeaderMap, HeaderName, HeaderValue, Uri};
use ipnet::IpNet;

/// Client-IP access control for a route. `deny` always blocks (precedence); a
/// non-empty `allow` restricts to listed networks (default-deny). Empty = allow
/// all. Built from already-validated config; unparseable entries are dropped.
#[derive(Default)]
pub(crate) struct IpAccess {
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
}

impl IpAccess {
    fn compile(cfg: &IpAccessConfig) -> Self {
        let parse = |v: &[String]| v.iter().filter_map(|s| parse_trusted_proxy(s)).collect();
        Self {
            allow: parse(&cfg.allow),
            deny: parse(&cfg.deny),
        }
    }

    pub(crate) fn allows(&self, ip: IpAddr) -> bool {
        if self.deny.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        true
    }
}

/// Compiled header rewrite for one direction: `remove` runs, then `set` (so an
/// explicit `set` always wins). Built from already-validated config; any
/// name/value that fails to compile is dropped defensively.
#[derive(Default)]
pub(crate) struct HeaderOps {
    set: Vec<(HeaderName, HeaderValue)>,
    remove: Vec<HeaderName>,
}

impl HeaderOps {
    fn compile(cfg: &HeaderOpsConfig) -> Self {
        let remove = cfg
            .remove
            .iter()
            .filter_map(|n| HeaderName::try_from(n.as_str()).ok())
            .collect();
        let set = cfg
            .set
            .iter()
            .filter_map(|(n, v)| {
                Some((
                    HeaderName::try_from(n.as_str()).ok()?,
                    HeaderValue::try_from(v.as_str()).ok()?,
                ))
            })
            .collect();
        Self { set, remove }
    }

    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        for name in &self.remove {
            headers.remove(name);
        }
        for (name, value) in &self.set {
            headers.insert(name, value.clone());
        }
    }
}

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
    strip_prefix: bool,
    /// Header rewrites for the upstream request and the client response.
    pub(crate) request_headers: HeaderOps,
    pub(crate) response_headers: HeaderOps,
    /// Client-IP access control (allow/deny).
    pub(crate) ip_access: IpAccess,
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

    /// The path + query to forward upstream, applying `stripPrefix` if set.
    /// Strips the matched `path_prefix` and normalizes to a leading `/`.
    pub(crate) fn rewritten_path_and_query(&self, uri: &Uri) -> String {
        let original = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        if !self.strip_prefix {
            return original.to_string();
        }
        let (path, query) = match original.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (original, None),
        };
        let stripped = path.strip_prefix(self.path_prefix.as_str()).unwrap_or(path);
        let new_path = if stripped.is_empty() {
            "/".to_string()
        } else if stripped.starts_with('/') {
            stripped.to_string()
        } else {
            format!("/{stripped}")
        };
        match query {
            Some(q) => format!("{new_path}?{q}"),
            None => new_path,
        }
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
                    strip_prefix: r.strip_prefix,
                    request_headers: r
                        .headers
                        .as_ref()
                        .map(|h| HeaderOps::compile(&h.request))
                        .unwrap_or_default(),
                    response_headers: r
                        .headers
                        .as_ref()
                        .map(|h| HeaderOps::compile(&h.response))
                        .unwrap_or_default(),
                    ip_access: r
                        .ip_access
                        .as_ref()
                        .map(IpAccess::compile)
                        .unwrap_or_default(),
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
            strip_prefix: false,
            headers: None,
            ip_access: None,
        }
    }

    #[test]
    fn header_ops_remove_then_set() {
        use std::collections::BTreeMap;
        let cfg = HeaderOpsConfig {
            set: BTreeMap::from([("x-a".to_string(), "new".to_string())]),
            remove: vec!["x-b".to_string()],
        };
        let ops = HeaderOps::compile(&cfg);
        let mut h = HeaderMap::new();
        h.insert("x-b", HeaderValue::from_static("old"));
        h.insert("x-a", HeaderValue::from_static("orig"));
        ops.apply(&mut h);
        assert!(!h.contains_key("x-b"), "remove should delete x-b");
        assert_eq!(h.get("x-a").unwrap(), "new", "set should overwrite x-a");
    }

    #[test]
    fn header_ops_compile_skips_invalid() {
        use std::collections::BTreeMap;
        let cfg = HeaderOpsConfig {
            set: BTreeMap::from([("bad name".to_string(), "v".to_string())]),
            remove: vec!["also bad".to_string()],
        };
        let ops = HeaderOps::compile(&cfg);
        let mut h = HeaderMap::new();
        ops.apply(&mut h); // must not panic; invalid entries are dropped
        assert!(h.is_empty());
    }

    fn acl(allow: &[&str], deny: &[&str]) -> IpAccess {
        IpAccess::compile(&IpAccessConfig {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn ip_access_empty_allows_all() {
        assert!(IpAccess::default().allows("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn ip_access_deny_blocks() {
        let a = acl(&[], &["10.0.0.0/8"]);
        assert!(!a.allows("10.1.2.3".parse().unwrap()));
        assert!(a.allows("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn ip_access_allowlist_restricts() {
        let a = acl(&["192.168.0.0/16"], &[]);
        assert!(a.allows("192.168.1.1".parse().unwrap()));
        assert!(!a.allows("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn ip_access_deny_wins_over_allow() {
        let a = acl(&["10.0.0.0/8"], &["10.0.0.5"]);
        assert!(a.allows("10.0.0.1".parse().unwrap()));
        assert!(!a.allows("10.0.0.5".parse().unwrap()));
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
    fn strip_prefix_rewrites_forwarded_path() {
        let mut r = route("api", Some("h"), Some("/api"));
        r.strip_prefix = true;
        let router = Router::build(&[r]);
        let cr = router.match_route("h", "/api/users").unwrap();
        assert_eq!(
            cr.rewritten_path_and_query(&"http://x/api/users?q=1".parse().unwrap()),
            "/users?q=1"
        );
        // Stripping down to nothing normalizes to "/".
        assert_eq!(
            cr.rewritten_path_and_query(&"http://x/api".parse().unwrap()),
            "/"
        );
    }

    #[test]
    fn no_strip_keeps_original_path() {
        let router = Router::build(&[route("api", Some("h"), Some("/api"))]);
        let cr = router.match_route("h", "/api/users").unwrap();
        assert_eq!(
            cr.rewritten_path_and_query(&"http://x/api/users".parse().unwrap()),
            "/api/users"
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
