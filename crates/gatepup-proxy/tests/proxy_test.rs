use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatepup_config::{
    AppConfig, GatePupConfig, HealthCheckConfig, ListenerConfig, MatchConfig, Protocol,
    RetryConfig, RouteConfig, TargetConfig, UpstreamConfig,
};
use gatepup_observability::Metrics;
use http::Method;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A backend that always answers `200 backend-ok`.
async fn spawn_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|_req| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"backend-ok"))))
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// A backend that answers `200 <label>` on every request, to identify which
/// target served a request in load-balancing tests.
async fn spawn_labeled_backend(label: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| async move {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                        label.as_bytes(),
                    ))))
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// A backend that waits `delay_ms` before answering `200 slow-ok`. Used to keep
/// a request reliably in-flight while a shutdown is triggered.
async fn spawn_slow_backend(delay_ms: u64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"slow-ok"))))
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// A backend that echoes the headers it received as the response body, one
/// `name: value` per line. Lets tests assert on what the proxy forwarded.
async fn spawn_header_echo_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|req: Request<hyper::body::Incoming>| async move {
                    let mut lines = String::new();
                    for (name, value) in req.headers() {
                        lines.push_str(name.as_str());
                        lines.push_str(": ");
                        lines.push_str(value.to_str().unwrap_or(""));
                        lines.push('\n');
                    }
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(lines))))
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// A backend whose `/health` returns 200 or 500 depending on a shared flag;
/// every other path returns `200 ok`. Lets tests drive health transitions.
async fn spawn_flag_backend(initial_healthy: bool) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let flag = Arc::new(AtomicBool::new(initial_healthy));
    let flag_for_loop = flag.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let flag = flag_for_loop.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let flag = flag.clone();
                    async move {
                        let resp = if req.uri().path() == "/health" {
                            let code = if flag.load(Ordering::Relaxed) {
                                200
                            } else {
                                500
                            };
                            Response::builder()
                                .status(code)
                                .body(Full::new(Bytes::new()))
                                .unwrap()
                        } else {
                            Response::new(Full::new(Bytes::from_static(b"ok")))
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    (port, flag)
}

/// Config with active health checks enabled (1s interval, threshold 1) over the
/// given target ports.
fn config_with_health(proxy_port: u16, target_ports: &[u16], health_path: &str) -> GatePupConfig {
    GatePupConfig {
        app: AppConfig {
            name: "test".to_string(),
            log_level: "error".to_string(),
        },
        listeners: vec![ListenerConfig {
            name: "test".to_string(),
            bind: format!("127.0.0.1:{proxy_port}"),
            protocol: Protocol::Http,
            tls: None,
            routes: vec![RouteConfig {
                name: "r".to_string(),
                matcher: MatchConfig {
                    host: None,
                    path_prefix: Some("/".to_string()),
                },
                upstream: "u".to_string(),
            }],
        }],
        upstreams: vec![UpstreamConfig {
            name: "u".to_string(),
            load_balancing: Default::default(),
            targets: target_ports
                .iter()
                .map(|p| TargetConfig {
                    url: format!("http://127.0.0.1:{p}"),
                    weight: 1,
                })
                .collect(),
            health_check: Some(HealthCheckConfig {
                enabled: true,
                path: health_path.to_string(),
                interval_seconds: 1,
                timeout_ms: 500,
                healthy_threshold: 1,
                unhealthy_threshold: 1,
            }),
            retries: None,
        }],
        timeouts: Default::default(),
        admin: None,
        metrics: None,
    }
}

async fn wait_for_status(port: u16, expected: u16, label: &str) {
    for _ in 0..60 {
        if get(port).await.status().as_u16() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{label}: never observed status {expected}");
}

fn config(proxy_port: u16, host: Option<&str>, backend_port: u16) -> GatePupConfig {
    GatePupConfig {
        app: AppConfig {
            name: "test".to_string(),
            log_level: "error".to_string(),
        },
        listeners: vec![ListenerConfig {
            name: "test".to_string(),
            bind: format!("127.0.0.1:{proxy_port}"),
            protocol: Protocol::Http,
            tls: None,
            routes: vec![RouteConfig {
                name: "r".to_string(),
                matcher: MatchConfig {
                    host: host.map(str::to_string),
                    path_prefix: Some("/".to_string()),
                },
                upstream: "u".to_string(),
            }],
        }],
        upstreams: vec![UpstreamConfig {
            name: "u".to_string(),
            load_balancing: Default::default(),
            targets: vec![TargetConfig {
                url: format!("http://127.0.0.1:{backend_port}"),
                weight: 1,
            }],
            health_check: None,
            retries: None,
        }],
        timeouts: Default::default(),
        admin: None,
        metrics: None,
    }
}

async fn spawn_proxy(config: GatePupConfig) {
    let snapshot = Arc::new(gatepup_proxy::build_snapshot(&config).unwrap());
    tokio::spawn(async move {
        let _ = gatepup_proxy::run(snapshot).await;
    });
}

async fn wait_until_listening(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("proxy on port {port} did not start");
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get(port: u16) -> Response<hyper::body::Incoming> {
    request_method(port, Method::GET).await
}

async fn request_method(port: u16, method: Method) -> Response<hyper::body::Incoming> {
    let req = Request::builder()
        .method(method)
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    client().request(req).await.unwrap()
}

/// A backend that always answers `500`.
async fn spawn_500_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(500)
                            .body(Full::new(Bytes::from_static(b"err")))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// Config with two ordered targets (first is tried first) and a retry policy.
fn config_retries(proxy_port: u16, target_ports: &[u16], retry: RetryConfig) -> GatePupConfig {
    let mut cfg = config(proxy_port, None, target_ports[0]);
    cfg.upstreams[0].targets = target_ports
        .iter()
        .map(|p| TargetConfig {
            url: format!("http://127.0.0.1:{p}"),
            weight: 1,
        })
        .collect();
    cfg.upstreams[0].retries = Some(retry);
    cfg
}

fn retry_policy(methods: &[&str], retry_on: &[&str], attempts: u32) -> RetryConfig {
    RetryConfig {
        enabled: true,
        attempts,
        methods: methods.iter().map(|m| m.to_string()).collect(),
        retry_on: retry_on.iter().map(|c| c.to_string()).collect(),
    }
}

#[tokio::test]
async fn proxies_request_to_backend() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config(proxy_port, None, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"backend-ok");
}

#[tokio::test]
async fn returns_502_when_upstream_refuses_connection() {
    let dead_port = free_port(); // nothing is listening here
    let proxy_port = free_port();
    spawn_proxy(config(proxy_port, None, dead_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 502);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("upstream_connect_error"),
        "body was: {body:?}"
    );
}

#[tokio::test]
async fn forwards_proxy_headers_to_upstream() {
    let backend_port = spawn_header_echo_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config(proxy_port, None, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let seen = String::from_utf8_lossy(&body).to_lowercase();

    assert!(seen.contains("x-forwarded-for:"), "missing xff in {seen:?}");
    assert!(
        seen.contains("x-forwarded-proto: http"),
        "missing xfp in {seen:?}"
    );
    assert!(
        seen.contains("x-request-id:"),
        "missing request id in {seen:?}"
    );
    // Host is rewritten to the upstream authority, not the inbound proxy host.
    assert!(
        seen.contains(&format!("host: 127.0.0.1:{backend_port}")),
        "host not rewritten in {seen:?}"
    );
}

#[tokio::test]
async fn active_checks_exclude_dead_target() {
    let live_port = spawn_backend().await; // answers 200 on every path incl. /health
    let dead_port = free_port(); // nothing listening → probes fail
    let proxy_port = free_port();
    spawn_proxy(config_with_health(
        proxy_port,
        &[live_port, dead_port],
        "/health",
    ))
    .await;
    wait_until_listening(proxy_port).await;

    // Once the dead target is ejected, every request must succeed.
    wait_for_status(proxy_port, 200, "after ejection").await;
    for _ in 0..6 {
        assert_eq!(
            get(proxy_port).await.status(),
            200,
            "dead target not excluded"
        );
    }
}

#[tokio::test]
async fn active_checks_eject_then_recover() {
    // Start unhealthy: /health returns 500 until we flip the flag.
    let (backend_port, healthy) = spawn_flag_backend(false).await;
    let proxy_port = free_port();
    spawn_proxy(config_with_health(proxy_port, &[backend_port], "/health")).await;
    wait_until_listening(proxy_port).await;

    // Sole target is unhealthy → 503 no_healthy_upstream.
    wait_for_status(proxy_port, 503, "while unhealthy").await;
    let resp = get(proxy_port).await;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("no_healthy_upstream"));

    // Flip backend healthy → active probe recovers it → 200.
    healthy.store(true, Ordering::Relaxed);
    wait_for_status(proxy_port, 200, "after recovery").await;
}

#[tokio::test]
async fn weighted_round_robin_distributes_by_weight() {
    let heavy = spawn_labeled_backend("heavy").await; // weight 3
    let light = spawn_labeled_backend("light").await; // weight 1
    let proxy_port = free_port();

    let mut cfg = config(proxy_port, None, heavy);
    cfg.upstreams[0].targets = vec![
        TargetConfig {
            url: format!("http://127.0.0.1:{heavy}"),
            weight: 3,
        },
        TargetConfig {
            url: format!("http://127.0.0.1:{light}"),
            weight: 1,
        },
    ];
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let mut heavy_hits = 0;
    let mut light_hits = 0;
    for _ in 0..80 {
        let body = get(proxy_port)
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        match &body[..] {
            b"heavy" => heavy_hits += 1,
            b"light" => light_hits += 1,
            other => panic!("unexpected body: {:?}", other),
        }
    }
    // 3:1 over 80 requests (schedule length 4) is deterministic.
    assert_eq!(heavy_hits, 60, "heavy should get 3/4");
    assert_eq!(light_hits, 20, "light should get 1/4");
}

#[tokio::test]
async fn retries_connect_failure_onto_next_target() {
    let dead = free_port(); // target 0: nothing listening -> connect error
    let healthy = spawn_labeled_backend("B").await; // target 1
    let proxy_port = free_port();
    let cfg = config_retries(
        proxy_port,
        &[dead, healthy],
        retry_policy(&["GET"], &["connect_error"], 2),
    );

    let snapshot = Arc::new(gatepup_proxy::build_snapshot(&cfg).unwrap());
    let metrics = Arc::new(Metrics::new().unwrap());
    let (_tx, rx) = watch::channel(false);
    let _server = tokio::spawn(gatepup_proxy::serve(snapshot, metrics.clone(), rx));
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        &body[..],
        b"B",
        "should have retried onto the healthy target"
    );
    assert!(
        metrics
            .encode()
            .contains("gatepup_upstream_retries_total 1"),
        "retry counter not incremented"
    );
}

#[tokio::test]
async fn non_idempotent_method_not_retried() {
    let dead = free_port();
    let healthy = spawn_labeled_backend("B").await;
    let proxy_port = free_port();
    // Retries enabled, but only for GET — a POST must not be retried.
    spawn_proxy(config_retries(
        proxy_port,
        &[dead, healthy],
        retry_policy(&["GET"], &["connect_error"], 2),
    ))
    .await;
    wait_until_listening(proxy_port).await;

    let resp = request_method(proxy_port, Method::POST).await;
    assert_eq!(resp.status(), 502, "POST should not be retried");
}

#[tokio::test]
async fn retries_on_5xx_onto_next_target() {
    let failing = spawn_500_backend().await; // target 0 -> 500
    let healthy = spawn_labeled_backend("B").await; // target 1 -> 200
    let proxy_port = free_port();
    spawn_proxy(config_retries(
        proxy_port,
        &[failing, healthy],
        retry_policy(&["GET"], &["upstream_5xx"], 2),
    ))
    .await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"B", "should have retried off the 5xx target");
}

#[tokio::test]
async fn retries_exhausted_returns_error() {
    let dead1 = free_port();
    let dead2 = free_port();
    let proxy_port = free_port();
    spawn_proxy(config_retries(
        proxy_port,
        &[dead1, dead2],
        retry_policy(&["GET"], &["connect_error"], 2),
    ))
    .await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 502);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("upstream_connect_error"));
}

#[tokio::test]
async fn returns_504_when_request_exceeds_timeout() {
    let backend_port = spawn_slow_backend(1000).await; // ~1s response
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.timeouts.request_timeout_ms = 150; // shorter than the backend delay
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 504);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("upstream_timeout"),
        "body was: {body:?}"
    );
}

#[tokio::test]
async fn graceful_shutdown_drains_in_flight_request() {
    let backend_port = spawn_slow_backend(500).await;
    let proxy_port = free_port();
    let snapshot =
        Arc::new(gatepup_proxy::build_snapshot(&config(proxy_port, None, backend_port)).unwrap());
    let metrics = Arc::new(Metrics::new().unwrap());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let server = tokio::spawn(gatepup_proxy::serve(snapshot, metrics, shutdown_rx));
    wait_until_listening(proxy_port).await;

    // Start a request that will be in-flight for ~500ms.
    let in_flight = tokio::spawn(async move { get(proxy_port).await.status().as_u16() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Trigger shutdown while the request is still being served.
    shutdown_tx.send(true).unwrap();

    // The in-flight request must still complete successfully (drained, not cut).
    let status = in_flight.await.unwrap();
    assert_eq!(status, 200, "in-flight request was severed by shutdown");

    // serve() returns once draining completes, well within the grace window.
    let served = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(served.is_ok(), "serve did not return after shutdown");
    assert!(served.unwrap().unwrap().is_ok(), "serve returned an error");
}

#[tokio::test]
async fn returns_404_for_unmatched_host() {
    let proxy_port = free_port();
    // Route requires a host the client will never send (it sends 127.0.0.1).
    spawn_proxy(config(proxy_port, Some("never.matches"), 1)).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 404);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("route_not_found"),
        "body was: {:?}",
        body
    );
}
