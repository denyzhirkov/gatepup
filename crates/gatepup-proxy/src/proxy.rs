use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::PathAndQuery;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use gatepup_observability::Metrics;

use crate::snapshot::{ListenerRuntime, RuntimeConfig};
use crate::BoxError;

/// Hard request timeout until configurable timeout policies land (v0.2).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Hop-by-hop headers that must not be forwarded to the upstream.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub(crate) type ResponseBody = BoxBody<Bytes, BoxError>;
pub(crate) type ProxyClient = Client<HttpConnector, Incoming>;

pub(crate) fn build_client() -> ProxyClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Per-request failure that maps to a specific HTTP status + JSON error body.
#[derive(Debug, Clone, Copy)]
enum GatewayError {
    RouteNotFound,
    UpstreamMissing,
    NoHealthyUpstream,
    UpstreamTimeout,
    UpstreamConnect,
    BadGateway,
}

impl GatewayError {
    fn parts(self) -> (StatusCode, &'static str) {
        match self {
            GatewayError::RouteNotFound => (StatusCode::NOT_FOUND, "route_not_found"),
            GatewayError::NoHealthyUpstream => {
                (StatusCode::SERVICE_UNAVAILABLE, "no_healthy_upstream")
            }
            GatewayError::UpstreamTimeout => (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
            GatewayError::UpstreamConnect => (StatusCode::BAD_GATEWAY, "upstream_connect_error"),
            GatewayError::UpstreamMissing | GatewayError::BadGateway => {
                (StatusCode::BAD_GATEWAY, "bad_gateway")
            }
        }
    }
}

/// A successfully forwarded response plus the route/upstream it resolved to.
struct Forwarded {
    response: Response<ResponseBody>,
    route: String,
    upstream: String,
}

/// Handle one request: match a route, forward to an upstream target, and stream
/// the response back. Always resolves to a `Response` — failures become mapped
/// error responses, never a service error.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    req: Request<Incoming>,
    listener: Arc<ListenerRuntime>,
    config: Arc<RuntimeConfig>,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    remote: SocketAddr,
) -> Response<ResponseBody> {
    let started = Instant::now();
    let request_id = ensure_request_id(&req);
    let host = request_host(&req);
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    metrics.inc_requests();
    let (response, route, upstream) =
        match forward(req, &listener, &config, &client, &host, remote, &request_id).await {
            Ok(f) => {
                metrics.inc_upstream_requests();
                (f.response, Some(f.route), Some(f.upstream))
            }
            Err(err) => {
                if matches!(err, GatewayError::RouteNotFound) {
                    metrics.inc_route_not_found();
                } else {
                    metrics.inc_upstream_errors();
                }
                (error_response(err, &request_id), None, None)
            }
        };

    metrics.observe_duration(started.elapsed().as_secs_f64());
    tracing::info!(
        request_id = %request_id,
        method = %method,
        host = %host,
        path = %path,
        route = route.as_deref().unwrap_or("-"),
        upstream = upstream.as_deref().unwrap_or("-"),
        status = response.status().as_u16(),
        duration_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

#[allow(clippy::too_many_arguments)]
async fn forward(
    req: Request<Incoming>,
    listener: &ListenerRuntime,
    config: &RuntimeConfig,
    client: &ProxyClient,
    host: &str,
    remote: SocketAddr,
    request_id: &str,
) -> Result<Forwarded, GatewayError> {
    let route = listener
        .router
        .match_route(host, req.uri().path())
        .ok_or(GatewayError::RouteNotFound)?;
    let route_name = route.name.clone();
    let upstream_name = route.upstream.clone();
    let upstream = config
        .upstreams
        .get(&route.upstream)
        .ok_or(GatewayError::UpstreamMissing)?;
    let target = upstream
        .pick_target()
        .ok_or(GatewayError::NoHealthyUpstream)?;

    let upstream_req = build_upstream_request(req, &target.url, host, remote, request_id)
        .map_err(|_| GatewayError::BadGateway)?;

    // Passive health: feed each proxied outcome into the target's health state.
    // Connect error / timeout / 5xx count as failures; anything else succeeds.
    match tokio::time::timeout(REQUEST_TIMEOUT, client.request(upstream_req)).await {
        Err(_elapsed) => {
            target.state.observe(false);
            Err(GatewayError::UpstreamTimeout)
        }
        Ok(Err(_)) => {
            target.state.observe(false);
            Err(GatewayError::UpstreamConnect)
        }
        Ok(Ok(resp)) => {
            target.state.observe(resp.status().as_u16() < 500);
            Ok(Forwarded {
                response: resp.map(|body| body.map_err(|e| Box::new(e) as BoxError).boxed()),
                route: route_name,
                upstream: upstream_name,
            })
        }
    }
}

fn build_upstream_request(
    req: Request<Incoming>,
    target_url: &str,
    fwd_host: &str,
    remote: SocketAddr,
    request_id: &str,
) -> Result<Request<Incoming>, ()> {
    let (mut parts, body) = req.into_parts();
    parts.uri = build_upstream_uri(target_url, &parts.uri)?;
    rewrite_headers(&mut parts.headers, fwd_host, remote.ip(), request_id);
    Ok(Request::from_parts(parts, body))
}

/// Strip hop-by-hop and Host headers, then set the forwarding headers. Pure
/// over a `HeaderMap` so it can be exercised directly in tests.
fn rewrite_headers(headers: &mut HeaderMap, fwd_host: &str, remote: IpAddr, request_id: &str) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    headers.remove(header::HOST);

    append_forwarded_for(headers, remote);
    set_header(headers, "x-forwarded-host", fwd_host);
    set_header(headers, "x-forwarded-proto", "http");
    set_header(headers, "x-request-id", request_id);
}

fn build_upstream_uri(target_url: &str, original: &Uri) -> Result<Uri, ()> {
    let base: Uri = target_url.parse().map_err(|_| ())?;
    let mut parts = base.into_parts();
    let path_and_query = original
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    parts.path_and_query = Some(path_and_query);
    Uri::from_parts(parts).map_err(|_| ())
}

fn append_forwarded_for(headers: &mut HeaderMap, ip: IpAddr) {
    let ip = ip.to_string();
    let value = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing}, {ip}"),
        None => ip,
    };
    set_header(headers, "x-forwarded-for", &value);
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn request_host(req: &Request<Incoming>) -> String {
    let raw = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
        .unwrap_or_default();
    raw.split(':').next().unwrap_or("").to_ascii_lowercase()
}

fn ensure_request_id(req: &Request<Incoming>) -> String {
    req.headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(generate_request_id)
}

fn generate_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{seq:08x}")
}

fn error_response(err: GatewayError, request_id: &str) -> Response<ResponseBody> {
    let (status, code) = err.parts();
    let body = Full::new(Bytes::from(format!("{{\"error\":\"{code}\"}}\n")))
        .map_err(|never| match never {})
        .boxed();

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(request_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_errors_map_to_expected_status_and_code() {
        let cases = [
            (GatewayError::RouteNotFound, 404, "route_not_found"),
            (GatewayError::NoHealthyUpstream, 503, "no_healthy_upstream"),
            (GatewayError::UpstreamTimeout, 504, "upstream_timeout"),
            (GatewayError::UpstreamConnect, 502, "upstream_connect_error"),
            (GatewayError::UpstreamMissing, 502, "bad_gateway"),
            (GatewayError::BadGateway, 502, "bad_gateway"),
        ];
        for (err, status, code) in cases {
            let (s, c) = err.parts();
            assert_eq!(s.as_u16(), status, "status for {err:?}");
            assert_eq!(c, code, "code for {err:?}");
        }
    }

    #[test]
    fn error_response_carries_json_body_and_request_id() {
        let resp = error_response(GatewayError::RouteNotFound, "rid-123");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(resp.headers().get("x-request-id").unwrap(), "rid-123");
    }

    #[test]
    fn build_upstream_uri_keeps_path_and_query() {
        let original: Uri = "http://gatepup.local/users?page=2".parse().unwrap();
        let uri = build_upstream_uri("http://backend:4000", &original).unwrap();
        assert_eq!(uri.to_string(), "http://backend:4000/users?page=2");
    }

    #[test]
    fn build_upstream_uri_defaults_empty_path_to_root() {
        let original: Uri = "http://gatepup.local".parse().unwrap();
        let uri = build_upstream_uri("http://backend:4000", &original).unwrap();
        assert_eq!(uri.to_string(), "http://backend:4000/");
    }

    #[test]
    fn build_upstream_uri_rejects_invalid_target() {
        let original: Uri = "/".parse().unwrap();
        assert!(build_upstream_uri("not a url", &original).is_err());
    }

    fn ip() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    #[test]
    fn rewrite_headers_strips_hop_by_hop_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("gatepup.local"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        headers.insert(
            HeaderName::from_static("transfer-encoding"),
            HeaderValue::from_static("chunked"),
        );
        headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        headers.insert(
            HeaderName::from_static("x-keep"),
            HeaderValue::from_static("yes"),
        );

        rewrite_headers(&mut headers, "gatepup.local", ip(), "rid-1");

        assert!(headers.get(header::HOST).is_none());
        assert!(headers.get(header::CONNECTION).is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get(header::UPGRADE).is_none());
        // Non-hop-by-hop headers are preserved.
        assert_eq!(headers.get("x-keep").unwrap(), "yes");
    }

    #[test]
    fn rewrite_headers_sets_forwarding_headers() {
        let mut headers = HeaderMap::new();
        rewrite_headers(&mut headers, "api.example.com", ip(), "rid-2");

        assert_eq!(headers.get("x-forwarded-for").unwrap(), "203.0.113.7");
        assert_eq!(headers.get("x-forwarded-host").unwrap(), "api.example.com");
        assert_eq!(headers.get("x-forwarded-proto").unwrap(), "http");
        assert_eq!(headers.get("x-request-id").unwrap(), "rid-2");
    }

    #[test]
    fn rewrite_headers_appends_to_existing_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("198.51.100.1"),
        );
        rewrite_headers(&mut headers, "h", ip(), "rid-3");
        assert_eq!(
            headers.get("x-forwarded-for").unwrap(),
            "198.51.100.1, 203.0.113.7"
        );
    }

    #[test]
    fn generated_request_ids_are_unique() {
        let a = generate_request_id();
        let b = generate_request_id();
        assert_ne!(a, b);
    }
}
