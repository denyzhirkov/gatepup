use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::request::Parts;
use http::uri::PathAndQuery;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;

use gatepup_observability::Metrics;

use crate::snapshot::RuntimeConfig;
use crate::{BoxError, SharedConfig};

/// Max request body buffered to make a request replayable for retries. Idempotent
/// methods are normally body-less; larger bodies fall back to a single streamed
/// attempt (no retry).
const BODY_BUFFER_CAP: usize = 64 * 1024;

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

/// Unified boxed body used for both the response we return and the request we
/// send upstream (so streamed and buffered request bodies share one client type).
pub(crate) type ResponseBody = BoxBody<Bytes, BoxError>;
pub(crate) type ProxyClient = Client<HttpConnector, ResponseBody>;

pub(crate) fn build_client(connect_timeout: Duration) -> ProxyClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(connect_timeout));
    Client::builder(TokioExecutor::new()).build(connector)
}

fn box_incoming(body: Incoming) -> ResponseBody {
    body.map_err(|e| Box::new(e) as BoxError).boxed()
}

fn box_bytes(bytes: Bytes) -> ResponseBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// The request body for an upstream attempt: either a buffered (replayable) body
/// or a single-shot streamed body that can be sent only once.
enum BodySource {
    Buffered(Bytes),
    Once(Option<ResponseBody>),
}

impl BodySource {
    /// A body for the next attempt: buffered bodies clone; a streamed body is
    /// yielded once, then `None` (no further attempt possible).
    fn next(&mut self) -> Option<ResponseBody> {
        match self {
            BodySource::Buffered(bytes) => Some(box_bytes(bytes.clone())),
            BodySource::Once(slot) => slot.take(),
        }
    }
}

/// Decide whether the request body can be buffered for replay, and produce the
/// matching [`BodySource`]. Buffering is bounded by [`BODY_BUFFER_CAP`]; bodies
/// that exceed it (or fail to read) fall back to a single streamed attempt. A
/// non-zero `max_body_bytes` wraps the streamed body so it is cut if it exceeds
/// the cap mid-stream (declared-oversize bodies are rejected earlier with 413).
async fn prepare_body_source(body: Incoming, replayable: bool, max_body_bytes: u64) -> BodySource {
    if replayable {
        return match Limited::new(body, BODY_BUFFER_CAP).collect().await {
            Ok(collected) => BodySource::Buffered(collected.to_bytes()),
            // Over the cap or a read error: can't safely replay, and the original
            // body is now consumed — surface as a single (already-consumed) attempt.
            Err(_) => BodySource::Once(None),
        };
    }
    let boxed = if max_body_bytes > 0 {
        Limited::new(body, max_body_bytes as usize).boxed()
    } else {
        box_incoming(body)
    };
    BodySource::Once(Some(boxed))
}

/// Per-request failure that maps to a specific HTTP status + JSON error body.
#[derive(Debug, Clone, Copy)]
enum GatewayError {
    RouteNotFound,
    PayloadTooLarge,
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
            GatewayError::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
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

    /// Client-side rejections (4xx) are not upstream failures and must not be
    /// counted against upstream error metrics.
    fn is_client_error(self) -> bool {
        matches!(
            self,
            GatewayError::RouteNotFound | GatewayError::PayloadTooLarge
        )
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
    listener_name: &str,
    is_tls: bool,
    shared: SharedConfig,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    remote: SocketAddr,
) -> Response<ResponseBody> {
    let started = Instant::now();
    let request_id = ensure_request_id(&req);
    let host = request_host(&req);
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let scheme = if is_tls { "https" } else { "http" };
    let snapshot = shared.load_full();

    metrics.inc_requests();
    let outcome = if is_upgrade(&req) {
        handle_upgrade(
            req,
            &snapshot,
            listener_name,
            &host,
            scheme,
            remote,
            &request_id,
            &metrics,
        )
        .await
    } else {
        forward(
            req,
            &snapshot,
            listener_name,
            &client,
            &metrics,
            &host,
            scheme,
            remote,
            &request_id,
        )
        .await
    };
    let (response, route, upstream) = match outcome {
        Ok(f) => {
            metrics.inc_upstream_requests();
            (f.response, Some(f.route), Some(f.upstream))
        }
        Err(err) => {
            if matches!(err, GatewayError::RouteNotFound) {
                metrics.inc_route_not_found();
            } else if !err.is_client_error() {
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
    snapshot: &RuntimeConfig,
    listener_name: &str,
    client: &ProxyClient,
    metrics: &Metrics,
    host: &str,
    scheme: &str,
    remote: SocketAddr,
    request_id: &str,
) -> Result<Forwarded, GatewayError> {
    // The listener's router comes from the current snapshot (it may have been
    // hot-swapped since this connection was accepted).
    let router = snapshot
        .listeners
        .iter()
        .find(|l| l.name == listener_name)
        .map(|l| &l.router)
        .ok_or(GatewayError::RouteNotFound)?;
    let route = router
        .match_route(host, req.uri().path())
        .ok_or(GatewayError::RouteNotFound)?;
    let route_name = route.name.clone();
    let upstream_name = route.upstream.clone();
    let upstream = snapshot
        .upstreams
        .get(&route.upstream)
        .ok_or(GatewayError::UpstreamMissing)?;

    let (mut parts, body) = req.into_parts();
    let method = parts.method.clone();
    // Reject a declared-oversize body before touching the upstream.
    if body_exceeds_limit(&parts.headers, snapshot.max_body_bytes) {
        return Err(GatewayError::PayloadTooLarge);
    }
    let upstream_pq = route.rewritten_path_and_query(&parts.uri);
    rewrite_headers(&mut parts.headers, host, scheme, remote.ip(), request_id);

    // The body is buffered for replay only when retries are enabled for an
    // eligible method and the body fits the cap; otherwise it streams once.
    let replayable = match upstream.retry.as_ref() {
        Some(policy) if policy.max_attempts > 1 && policy.allows_method(&method) => {
            content_length(&parts.headers).is_none_or(|len| len <= BODY_BUFFER_CAP)
        }
        _ => false,
    };
    let mut body_source = prepare_body_source(body, replayable, snapshot.max_body_bytes).await;

    // Retries only when the body is replayable; otherwise a single attempt.
    let policy = upstream.retry.as_ref();
    let max_attempts = match policy {
        Some(p) if replayable => p.max_attempts,
        _ => 1,
    };

    let mut last_err = GatewayError::NoHealthyUpstream;
    for attempt in 0..max_attempts {
        let Some(target) = upstream.pick_target() else {
            last_err = GatewayError::NoHealthyUpstream;
            break;
        };
        // A streamed (non-replayable) body can only be sent once.
        let Some(body) = body_source.next() else {
            break;
        };
        let Some(upstream_req) = build_attempt_request(&parts, &upstream_pq, &target.url, body)
        else {
            return Err(GatewayError::BadGateway);
        };
        let is_last = attempt + 1 == max_attempts;

        // Passive health: feed each proxied outcome into the target's state.
        match tokio::time::timeout(snapshot.request_timeout, client.request(upstream_req)).await {
            // Overall request timeout is NOT retried (the backend may have
            // already processed the request).
            Err(_elapsed) => {
                target.state.observe(false);
                return Err(GatewayError::UpstreamTimeout);
            }
            Ok(Err(_)) => {
                target.state.observe(false);
                last_err = GatewayError::UpstreamConnect;
                let retry = policy.is_some_and(|p| p.on_connect_failure);
                if is_last || !retry {
                    return Err(last_err);
                }
                metrics.inc_upstream_retries();
            }
            Ok(Ok(resp)) => {
                let status = resp.status().as_u16();
                target.state.observe(status < 500);
                let retry_5xx = status >= 500 && policy.is_some_and(|p| p.on_5xx);
                if status < 500 || is_last || !retry_5xx {
                    return Ok(Forwarded {
                        response: resp.map(box_incoming),
                        route: route_name,
                        upstream: upstream_name,
                    });
                }
                metrics.inc_upstream_retries();
            }
        }
    }

    Err(last_err)
}

/// Hop-by-hop headers stripped from an upgrade request — note `connection` and
/// `upgrade` are intentionally PRESERVED so the upstream sees the handshake.
const UPGRADE_STRIP: &[&str] = &[
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// True when the request is an HTTP Upgrade (e.g. WebSocket): `Connection`
/// contains an `upgrade` token and an `Upgrade` header is present.
fn is_upgrade<B>(req: &Request<B>) -> bool {
    let headers = req.headers();
    let connection_upgrade = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    connection_upgrade && headers.contains_key(header::UPGRADE)
}

/// Proxy an Upgrade request: forward it to the chosen target over a one-off
/// connection (the pooled client can't surface an upgrade), and on a 101 tunnel
/// raw bytes bidirectionally between client and upstream.
#[allow(clippy::too_many_arguments)]
async fn handle_upgrade(
    mut req: Request<Incoming>,
    snapshot: &RuntimeConfig,
    listener_name: &str,
    host: &str,
    scheme: &str,
    remote: SocketAddr,
    request_id: &str,
    metrics: &Metrics,
) -> Result<Forwarded, GatewayError> {
    let router = snapshot
        .listeners
        .iter()
        .find(|l| l.name == listener_name)
        .map(|l| &l.router)
        .ok_or(GatewayError::RouteNotFound)?;
    let route = router
        .match_route(host, req.uri().path())
        .ok_or(GatewayError::RouteNotFound)?;
    let route_name = route.name.clone();
    let upstream_name = route.upstream.clone();
    let upstream = snapshot
        .upstreams
        .get(&route.upstream)
        .ok_or(GatewayError::UpstreamMissing)?;
    let target = upstream
        .pick_target()
        .ok_or(GatewayError::NoHealthyUpstream)?;
    let (target_host, target_port) =
        parse_authority(&target.url).ok_or(GatewayError::BadGateway)?;
    let authority = format!("{target_host}:{target_port}");

    let upstream_pq = route.rewritten_path_and_query(req.uri());
    // The client's upgraded IO becomes available after we return the 101.
    let client_upgrade = hyper::upgrade::on(&mut req);
    let upstream_req = build_upgrade_request(
        &req,
        &upstream_pq,
        &authority,
        host,
        scheme,
        remote,
        request_id,
    )
    .ok_or(GatewayError::BadGateway)?;

    let tcp = match TcpStream::connect((target_host.as_str(), target_port)).await {
        Ok(stream) => stream,
        Err(_) => {
            target.state.observe(false);
            return Err(GatewayError::UpstreamConnect);
        }
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await {
        Ok(pair) => pair,
        Err(_) => {
            target.state.observe(false);
            return Err(GatewayError::UpstreamConnect);
        }
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let mut resp = match sender.send_request(upstream_req).await {
        Ok(resp) => resp,
        Err(_) => {
            target.state.observe(false);
            return Err(GatewayError::UpstreamConnect);
        }
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        target.state.observe(true);
        metrics.inc_websocket();
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            if let (Ok(client_io), Ok(upstream_io)) = (client_upgrade.await, upstream_upgrade.await)
            {
                let mut client_io = TokioIo::new(client_io);
                let mut upstream_io = TokioIo::new(upstream_io);
                let _ = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await;
            }
        });
        // Relay the 101 (with the upstream's handshake headers) to the client.
        let (parts, _body) = resp.into_parts();
        let mut response = Response::new(box_bytes(Bytes::new()));
        *response.status_mut() = parts.status;
        *response.headers_mut() = parts.headers;
        Ok(Forwarded {
            response,
            route: route_name,
            upstream: upstream_name,
        })
    } else {
        // Upstream declined the upgrade: relay its response normally.
        target.state.observe(resp.status().as_u16() < 500);
        Ok(Forwarded {
            response: resp.map(box_incoming),
            route: route_name,
            upstream: upstream_name,
        })
    }
}

fn parse_authority(target_url: &str) -> Option<(String, u16)> {
    let uri: Uri = target_url.parse().ok()?;
    let host = uri.host()?.to_string();
    let port = uri.port_u16().unwrap_or(80);
    Some((host, port))
}

#[allow(clippy::too_many_arguments)]
fn build_upgrade_request(
    req: &Request<Incoming>,
    path_and_query: &str,
    authority: &str,
    fwd_host: &str,
    scheme: &str,
    remote: SocketAddr,
    request_id: &str,
) -> Option<Request<Empty<Bytes>>> {
    let mut headers = req.headers().clone();
    for name in UPGRADE_STRIP {
        headers.remove(*name);
    }
    headers.remove(header::HOST);
    set_header(&mut headers, "host", authority);
    append_forwarded_for(&mut headers, remote.ip());
    set_header(&mut headers, "x-forwarded-host", fwd_host);
    set_header(&mut headers, "x-forwarded-proto", scheme);
    set_header(&mut headers, "x-request-id", request_id);

    let mut builder = Request::builder()
        .method(req.method().clone())
        .uri(path_and_query);
    *builder.headers_mut()? = headers;
    builder.body(Empty::<Bytes>::new()).ok()
}

/// Build one upstream request for `target_url` from the rewritten parts template.
/// Headers are cloned per attempt; extensions are intentionally dropped.
fn build_attempt_request(
    template: &Parts,
    path_and_query: &str,
    target_url: &str,
    body: ResponseBody,
) -> Option<Request<ResponseBody>> {
    let uri = build_upstream_uri(target_url, path_and_query).ok()?;
    let mut builder = Request::builder()
        .method(template.method.clone())
        .uri(uri)
        .version(template.version);
    if let Some(headers) = builder.headers_mut() {
        *headers = template.headers.clone();
    }
    builder.body(body).ok()
}

/// True when a declared `Content-Length` exceeds the configured cap. `0` means
/// no cap. Undeclared (e.g. chunked) bodies pass here and are bounded mid-stream
/// by the [`BodySource`] wrapper instead.
fn body_exceeds_limit(headers: &HeaderMap, max_body_bytes: u64) -> bool {
    max_body_bytes > 0 && content_length(headers).is_some_and(|len| len as u64 > max_body_bytes)
}

fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Strip hop-by-hop and Host headers, then set the forwarding headers. Pure
/// over a `HeaderMap` so it can be exercised directly in tests.
fn rewrite_headers(
    headers: &mut HeaderMap,
    fwd_host: &str,
    scheme: &str,
    remote: IpAddr,
    request_id: &str,
) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    headers.remove(header::HOST);

    append_forwarded_for(headers, remote);
    set_header(headers, "x-forwarded-host", fwd_host);
    set_header(headers, "x-forwarded-proto", scheme);
    set_header(headers, "x-request-id", request_id);
}

fn build_upstream_uri(target_url: &str, path_and_query: &str) -> Result<Uri, ()> {
    let base: Uri = target_url.parse().map_err(|_| ())?;
    let mut parts = base.into_parts();
    let pq = PathAndQuery::try_from(path_and_query).map_err(|_| ())?;
    parts.path_and_query = Some(pq);
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
            (GatewayError::PayloadTooLarge, 413, "payload_too_large"),
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

    fn headers_with_content_length(len: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(len).unwrap());
        h
    }

    #[test]
    fn body_limit_zero_means_unlimited() {
        assert!(!body_exceeds_limit(
            &headers_with_content_length("99999999"),
            0
        ));
    }

    #[test]
    fn body_within_limit_is_allowed() {
        assert!(!body_exceeds_limit(&headers_with_content_length("10"), 10));
        assert!(!body_exceeds_limit(&headers_with_content_length("9"), 10));
    }

    #[test]
    fn declared_oversize_body_exceeds_limit() {
        assert!(body_exceeds_limit(&headers_with_content_length("11"), 10));
    }

    #[test]
    fn undeclared_body_passes_content_length_check() {
        // No Content-Length (e.g. chunked) is not rejected up front; it is bounded
        // mid-stream by the BodySource wrapper instead.
        assert!(!body_exceeds_limit(&HeaderMap::new(), 10));
    }

    #[test]
    fn build_upstream_uri_sets_path_and_query() {
        let uri = build_upstream_uri("http://backend:4000", "/users?page=2").unwrap();
        assert_eq!(uri.to_string(), "http://backend:4000/users?page=2");
    }

    #[test]
    fn build_upstream_uri_rejects_invalid_target() {
        assert!(build_upstream_uri("not a url", "/").is_err());
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

        rewrite_headers(&mut headers, "gatepup.local", "http", ip(), "rid-1");

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
        rewrite_headers(&mut headers, "api.example.com", "https", ip(), "rid-2");

        assert_eq!(headers.get("x-forwarded-for").unwrap(), "203.0.113.7");
        assert_eq!(headers.get("x-forwarded-host").unwrap(), "api.example.com");
        assert_eq!(headers.get("x-forwarded-proto").unwrap(), "https");
        assert_eq!(headers.get("x-request-id").unwrap(), "rid-2");
    }

    #[test]
    fn rewrite_headers_appends_to_existing_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("198.51.100.1"),
        );
        rewrite_headers(&mut headers, "h", "http", ip(), "rid-3");
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

    #[test]
    fn detects_upgrade_requests() {
        let mut req = Request::builder().uri("/").body(()).unwrap();
        assert!(!is_upgrade(&req), "plain request is not an upgrade");

        req.headers_mut().insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert!(!is_upgrade(&req), "upgrade token without Upgrade header");

        req.headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(is_upgrade(&req), "connection: upgrade + upgrade header");
    }

    #[test]
    fn parse_authority_splits_host_and_port() {
        assert_eq!(
            parse_authority("http://backend:4000"),
            Some(("backend".to_string(), 4000))
        );
        assert_eq!(
            parse_authority("http://1.2.3.4"),
            Some(("1.2.3.4".to_string(), 80))
        );
        assert_eq!(parse_authority("not a url"), None);
    }
}
