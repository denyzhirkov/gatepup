use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatepup_config::{
    AppConfig, GatePupConfig, HealthCheckConfig, ListenerConfig, MatchConfig, Protocol,
    RetryConfig, RouteConfig, TargetConfig, TlsConfig, UpstreamConfig,
};
use gatepup_observability::Metrics;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

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

/// A backend that echoes the request path+query it received as the response body.
async fn spawn_path_echo_backend() -> u16 {
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
                    let pq = req
                        .uri()
                        .path_and_query()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_else(|| "/".to_string());
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(pq))))
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

/// A backend that completes a WebSocket-style upgrade (101) and then echoes
/// every byte received on the upgraded connection.
async fn spawn_ws_echo_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|mut req: Request<hyper::body::Incoming>| async move {
                    if req.headers().contains_key(http::header::UPGRADE) {
                        let upgrade = hyper::upgrade::on(&mut req);
                        tokio::spawn(async move {
                            if let Ok(upgraded) = upgrade.await {
                                let (mut r, mut w) = tokio::io::split(TokioIo::new(upgraded));
                                let _ = tokio::io::copy(&mut r, &mut w).await;
                            }
                        });
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(101)
                                .header("connection", "upgrade")
                                .header("upgrade", "websocket")
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        )
                    } else {
                        Ok(Response::new(Full::new(Bytes::from_static(b"not-upgrade"))))
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}

fn upgrade_request(host: &str) -> Request<Empty<Bytes>> {
    Request::builder()
        .uri("/")
        .header("host", host)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Empty::<Bytes>::new())
        .unwrap()
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
                strip_prefix: false,
                headers: None,
                ip_access: None,
                rate_limit: None,
                basic_auth: None,
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
            tls_insecure_skip_verify: false,
        }],
        timeouts: Default::default(),
        limits: Default::default(),
        trusted_proxies: Vec::new(),
        compression: None,
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
                strip_prefix: false,
                headers: None,
                ip_access: None,
                rate_limit: None,
                basic_auth: None,
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
            tls_insecure_skip_verify: false,
        }],
        timeouts: Default::default(),
        limits: Default::default(),
        trusted_proxies: Vec::new(),
        compression: None,
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

async fn get_path(port: u16, path: &str) -> Response<hyper::body::Incoming> {
    let req = Request::builder()
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    client().request(req).await.unwrap()
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

fn write_temp_file(content: &str, label: &str) -> String {
    use std::io::Write;
    use std::sync::atomic::AtomicU32;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("gatepup-it-{}-{seq}-{label}", std::process::id()));
    std::fs::File::create(&path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    path.to_string_lossy().into_owned()
}

/// Generate a self-signed cert for `localhost`; return (cert path, key path, cert DER).
fn self_signed() -> (String, String, CertificateDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = write_temp_file(&cert.cert.pem(), "cert.pem");
    let key_path = write_temp_file(&cert.key_pair.serialize_pem(), "key.pem");
    (cert_path, key_path, cert.cert.der().clone())
}

/// A backend that serves HTTPS (self-signed) and answers `200 backend-ok`. Used
/// to exercise TLS-to-upstream.
async fn spawn_tls_backend() -> u16 {
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    let io = TokioIo::new(tls);
                    let service = service_fn(|_req| async {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            b"backend-ok",
                        ))))
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                }
            });
        }
    });
    port
}

fn config_tls(proxy_port: u16, cert: &str, key: &str, backend_port: u16) -> GatePupConfig {
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.listeners[0].protocol = Protocol::Https;
    cfg.listeners[0].tls = Some(TlsConfig {
        cert: cert.to_string(),
        key: key.to_string(),
    });
    cfg
}

/// HTTPS GET over a TLS client that trusts `cert_der`. Returns (status, body).
async fn tls_get(port: u16, cert_der: CertificateDer<'static>) -> (StatusCode, String) {
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(std::sync::Arc::new(config));

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let domain = ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(domain, tcp).await.unwrap();

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .uri("/")
        .header("host", "localhost")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn tls_listener_proxies_and_sets_https_proto() {
    let backend = spawn_header_echo_backend().await;
    let (cert_path, key_path, cert_der) = self_signed();
    let proxy_port = free_port();
    spawn_proxy(config_tls(proxy_port, &cert_path, &key_path, backend)).await;
    wait_until_listening(proxy_port).await;

    let (status, body) = tls_get(proxy_port, cert_der).await;
    assert_eq!(status, 200);
    assert!(
        body.to_lowercase().contains("x-forwarded-proto: https"),
        "upstream did not see https proto; body: {body}"
    );
}

#[tokio::test]
async fn proxies_to_https_upstream_with_insecure_skip_verify() {
    let backend_port = spawn_tls_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.upstreams[0].targets[0].url = format!("https://127.0.0.1:{backend_port}");
    cfg.upstreams[0].tls_insecure_skip_verify = true;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"backend-ok");
}

#[tokio::test]
async fn https_upstream_without_skip_verify_fails_cert_check() {
    let backend_port = spawn_tls_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.upstreams[0].targets[0].url = format!("https://127.0.0.1:{backend_port}");
    // tls_insecure_skip_verify defaults false: the self-signed cert is untrusted.
    spawn_proxy(cfg).await;
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
async fn https_listener_with_missing_cert_fails_to_build() {
    let cfg = config_tls(
        free_port(),
        "/no/such/cert.pem",
        "/no/such/key.pem",
        free_port(),
    );
    assert!(gatepup_proxy::build_snapshot(&cfg).is_err());
}

async fn body_of(resp: Response<hyper::body::Incoming>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Build a SharedConfig + serve it, returning the shared cell + reload sender so
/// the test can hot-swap routing.
async fn spawn_reloadable(cfg: GatePupConfig) -> (gatepup_proxy::SharedConfig, watch::Sender<u64>) {
    let shared = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
        gatepup_proxy::build_snapshot(&cfg).unwrap(),
    ));
    let metrics = Arc::new(Metrics::new().unwrap());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    std::mem::forget(_shutdown_tx); // keep alive for the test's lifetime
    let (reload_tx, reload_rx) = watch::channel(0u64);
    let server = shared.clone();
    tokio::spawn(async move {
        let _ = gatepup_proxy::serve_shared(server, metrics, shutdown_rx, reload_rx).await;
    });
    (shared, reload_tx)
}

#[tokio::test]
async fn reload_swaps_routing_for_new_requests() {
    let backend_a = spawn_labeled_backend("A").await;
    let backend_b = spawn_labeled_backend("B").await;
    let proxy_port = free_port();

    let (shared, reload_tx) = spawn_reloadable(config(proxy_port, None, backend_a)).await;
    wait_until_listening(proxy_port).await;
    assert_eq!(body_of(get(proxy_port).await).await, "A");

    // Hot-swap routing to backend B.
    shared.store(std::sync::Arc::new(
        gatepup_proxy::build_snapshot(&config(proxy_port, None, backend_b)).unwrap(),
    ));
    reload_tx.send_modify(|v| *v += 1);

    assert_eq!(body_of(get(proxy_port).await).await, "B");
}

#[tokio::test]
async fn reload_does_not_affect_in_flight_requests() {
    let slow = spawn_slow_backend(500).await; // returns "slow-ok" after ~500ms
    let fast = spawn_labeled_backend("B").await;
    let proxy_port = free_port();

    let (shared, reload_tx) = spawn_reloadable(config(proxy_port, None, slow)).await;
    wait_until_listening(proxy_port).await;

    // Start an in-flight request routed to the slow backend.
    let in_flight = tokio::spawn(async move { body_of(get(proxy_port).await).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Swap routing to the fast backend mid-flight.
    shared.store(std::sync::Arc::new(
        gatepup_proxy::build_snapshot(&config(proxy_port, None, fast)).unwrap(),
    ));
    reload_tx.send_modify(|v| *v += 1);

    // New request uses the new routing...
    assert_eq!(body_of(get(proxy_port).await).await, "B");
    // ...but the in-flight request still completes against the old target.
    assert_eq!(in_flight.await.unwrap(), "slow-ok");
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

async fn post_body(port: u16, len: usize) -> Response<hyper::body::Incoming> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(Full::new(Bytes::from(vec![b'x'; len])))
        .unwrap();
    client().request(req).await.unwrap()
}

#[tokio::test]
async fn rejects_oversize_body_with_413() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.limits.max_body_bytes = 10;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = post_body(proxy_port, 100).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("payload_too_large"),
        "body was: {body:?}"
    );
}

/// A request body with no known length, sent in one frame — hyper transmits it
/// chunked (no Content-Length), so the proxy can't pre-check it and must enforce
/// the cap mid-stream.
struct OneShotBody(Option<Bytes>);

impl hyper::body::Body for OneShotBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        std::task::Poll::Ready(self.0.take().map(|b| Ok(hyper::body::Frame::data(b))))
    }
}

#[tokio::test]
async fn chunked_oversize_body_returns_413() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.limits.max_body_bytes = 10;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    // No Content-Length (chunked) + body over the cap -> cut mid-stream -> 413.
    let client: Client<HttpConnector, OneShotBody> =
        Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://127.0.0.1:{proxy_port}/"))
        .body(OneShotBody(Some(Bytes::from(vec![b'x'; 100]))))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 413);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("payload_too_large"),
        "body was: {body:?}"
    );
}

#[tokio::test]
async fn forwards_body_within_limit() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.limits.max_body_bytes = 1000;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = post_body(proxy_port, 50).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"backend-ok");
}

#[tokio::test]
async fn slow_request_header_is_dropped_by_header_read_timeout() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.limits.header_read_timeout_ms = 300;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    // Send a partial request head and never terminate it. The server must close
    // the connection once the header-read timeout elapses.
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();

    let mut buf = [0u8; 64];
    // Read should resolve (EOF or a response) well within this bound; if the
    // connection were held open forever, this read would block past the timeout.
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("connection was not closed after the header-read timeout")
        .unwrap();
    // hyper closes the connection (EOF = 0) or sends an error response; either way
    // the slow client does not keep the connection indefinitely.
    assert!(
        n == 0 || buf.starts_with(b"HTTP/1.1 4"),
        "expected connection close or 4xx, read {n} bytes: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

#[tokio::test]
async fn applies_route_header_rules() {
    use gatepup_config::{HeaderOpsConfig, HeaderRulesConfig};
    use std::collections::BTreeMap;

    let backend_port = spawn_header_echo_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.listeners[0].routes[0].headers = Some(HeaderRulesConfig {
        request: HeaderOpsConfig {
            set: BTreeMap::from([("x-custom".to_string(), "hi".to_string())]),
            // Remove a header the proxy itself sets, proving route ops run after
            // the standard X-Forwarded-* rewrite.
            remove: vec!["x-forwarded-proto".to_string()],
        },
        response: HeaderOpsConfig {
            set: BTreeMap::from([("x-frame-options".to_string(), "DENY".to_string())]),
            remove: vec![],
        },
    });
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 200);
    // response.set is applied to the client response.
    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let seen = String::from_utf8_lossy(&body).to_lowercase();
    // request.set reached the upstream...
    assert!(
        seen.contains("x-custom: hi"),
        "set header missing in {seen:?}"
    );
    // ...and request.remove deleted the proxy-set X-Forwarded-Proto.
    assert!(
        !seen.contains("x-forwarded-proto:"),
        "remove failed in {seen:?}"
    );
}

fn ip_acl(allow: &[&str], deny: &[&str]) -> gatepup_config::IpAccessConfig {
    gatepup_config::IpAccessConfig {
        allow: allow.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test]
async fn denies_blocked_client_ip_with_403() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.listeners[0].routes[0].ip_access = Some(ip_acl(&[], &["127.0.0.1/32"]));
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 403);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("forbidden"),
        "body was: {body:?}"
    );
}

#[tokio::test]
async fn allows_permitted_client_ip() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.listeners[0].routes[0].ip_access = Some(ip_acl(&["127.0.0.1/32"], &[]));
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    assert_eq!(get(proxy_port).await.status(), 200);
}

#[tokio::test]
async fn allowlist_excludes_unlisted_client_with_403() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    // Allowlist that does not include loopback -> default-deny.
    cfg.listeners[0].routes[0].ip_access = Some(ip_acl(&["10.0.0.0/8"], &[]));
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    assert_eq!(get(proxy_port).await.status(), 403);
}

#[tokio::test]
async fn rate_limits_excess_requests_with_429() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend_port);
    // rate 1/s, burst 2: first two pass, third is limited (refill negligible
    // across back-to-back loopback requests).
    cfg.listeners[0].routes[0].rate_limit = Some(gatepup_config::RateLimitConfig {
        requests_per_second: 1.0,
        burst: 2,
    });
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    assert_eq!(get(proxy_port).await.status(), 200);
    assert_eq!(get(proxy_port).await.status(), 200);
    let resp = get(proxy_port).await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("rate_limited"),
        "body was: {body:?}"
    );
}

/// A backend returning `body_len` bytes with a given content-type and optional
/// pre-set `content-encoding`. Used to exercise response compression.
async fn spawn_typed_backend(
    content_type: &'static str,
    body_len: usize,
    pre_encoding: Option<&'static str>,
) -> u16 {
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
                    let mut builder = Response::builder().header("content-type", content_type);
                    if let Some(enc) = pre_encoding {
                        builder = builder.header("content-encoding", enc);
                    }
                    Ok::<_, Infallible>(
                        builder
                            .body(Full::new(Bytes::from(vec![b'a'; body_len])))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

fn config_compress(proxy_port: u16, backend_port: u16) -> GatePupConfig {
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.compression = Some(gatepup_config::CompressionConfig {
        enabled: true,
        algorithms: vec!["gzip".to_string()],
        min_bytes: 1024,
        types: vec!["text/html".to_string()],
    });
    cfg
}

async fn get_with_accept(port: u16, accept_encoding: &str) -> Response<hyper::body::Incoming> {
    let req = Request::builder()
        .uri(format!("http://127.0.0.1:{port}/"))
        .header("accept-encoding", accept_encoding)
        .body(Full::new(Bytes::new()))
        .unwrap();
    client().request(req).await.unwrap()
}

#[tokio::test]
async fn compresses_text_html_when_gzip_accepted() {
    use std::io::Read;
    let backend_port = spawn_typed_backend("text/html", 4096, None).await;
    let proxy_port = free_port();
    spawn_proxy(config_compress(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get_with_accept(proxy_port, "gzip").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "gzip");
    assert!(
        resp.headers().get("content-length").is_none(),
        "length dropped"
    );
    let vary = resp
        .headers()
        .get("vary")
        .unwrap()
        .to_str()
        .unwrap()
        .to_lowercase();
    assert!(vary.contains("accept-encoding"), "vary: {vary}");

    let compressed = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        compressed.len() < 4096,
        "body should be smaller when compressed"
    );
    let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert_eq!(out, vec![b'a'; 4096], "gzip round-trips to the original");
}

#[tokio::test]
async fn passthrough_when_encoding_not_accepted() {
    let backend_port = spawn_typed_backend("text/html", 4096, None).await;
    let proxy_port = free_port();
    spawn_proxy(config_compress(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    // No Accept-Encoding -> served uncompressed.
    let resp = get(proxy_port).await;
    assert!(resp.headers().get("content-encoding").is_none());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 4096);
}

#[tokio::test]
async fn passthrough_already_encoded_response() {
    // Backend already sets content-encoding -> proxy must not re-compress.
    let backend_port = spawn_typed_backend("text/html", 4096, Some("br")).await;
    let proxy_port = free_port();
    spawn_proxy(config_compress(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get_with_accept(proxy_port, "gzip").await;
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "br");
}

#[tokio::test]
async fn passthrough_small_body() {
    let backend_port = spawn_typed_backend("text/html", 100, None).await;
    let proxy_port = free_port();
    spawn_proxy(config_compress(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get_with_accept(proxy_port, "gzip").await;
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "small body not compressed"
    );
}

fn config_basic_auth(proxy_port: u16, backend_port: u16) -> GatePupConfig {
    let mut cfg = config(proxy_port, None, backend_port);
    cfg.listeners[0].routes[0].basic_auth = Some(gatepup_config::BasicAuthConfig {
        users: [("alice".to_string(), "secret".to_string())]
            .into_iter()
            .collect(),
    });
    cfg
}

async fn get_basic(port: u16, creds: Option<&str>) -> Response<hyper::body::Incoming> {
    let mut builder = Request::builder().uri(format!("http://127.0.0.1:{port}/"));
    if let Some(creds) = creds {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds);
        builder = builder.header("authorization", format!("Basic {b64}"));
    }
    let req = builder.body(Full::new(Bytes::new())).unwrap();
    client().request(req).await.unwrap()
}

#[tokio::test]
async fn basic_auth_challenges_without_credentials() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config_basic_auth(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get_basic(proxy_port, None).await;
    assert_eq!(resp.status(), 401);
    let www = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(www.starts_with("Basic"), "www-authenticate: {www}");
}

#[tokio::test]
async fn basic_auth_rejects_wrong_credentials() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config_basic_auth(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    assert_eq!(
        get_basic(proxy_port, Some("alice:wrong")).await.status(),
        401
    );
}

#[tokio::test]
async fn basic_auth_allows_valid_credentials() {
    let backend_port = spawn_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config_basic_auth(proxy_port, backend_port)).await;
    wait_until_listening(proxy_port).await;

    let resp = get_basic(proxy_port, Some("alice:secret")).await;
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
async fn proxies_websocket_upgrade_and_tunnels_bytes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let backend = spawn_ws_echo_backend().await;
    let proxy_port = free_port();
    spawn_proxy(config(proxy_port, None, backend)).await;
    wait_until_listening(proxy_port).await;

    let tcp = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let mut resp = sender
        .send_request(upgrade_request("ws.local"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 101, "proxy should relay the 101");

    // Tunnel bytes through: client -> proxy -> backend echo -> back.
    let upgraded = hyper::upgrade::on(&mut resp).await.unwrap();
    let mut io = TokioIo::new(upgraded);
    io.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping", "bytes should tunnel both ways");
}

#[tokio::test]
async fn non_101_upgrade_response_is_relayed() {
    let backend = spawn_backend().await; // answers 200 to everything, ignores upgrade
    let proxy_port = free_port();
    spawn_proxy(config(proxy_port, None, backend)).await;
    wait_until_listening(proxy_port).await;

    let tcp = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let resp = sender.send_request(upgrade_request("x")).await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "non-101 upgrade response is relayed as-is"
    );
    assert_eq!(body_of(resp).await, "backend-ok");
}

#[tokio::test]
async fn strip_prefix_forwards_stripped_path() {
    let backend = spawn_path_echo_backend().await;
    let proxy_port = free_port();
    let mut cfg = config(proxy_port, None, backend);
    cfg.listeners[0].routes[0].matcher.path_prefix = Some("/api".to_string());
    cfg.listeners[0].routes[0].strip_prefix = true;
    spawn_proxy(cfg).await;
    wait_until_listening(proxy_port).await;

    assert_eq!(
        body_of(get_path(proxy_port, "/api/users?x=1").await).await,
        "/users?x=1",
        "matched prefix should be stripped, query preserved"
    );
    assert_eq!(
        body_of(get_path(proxy_port, "/api").await).await,
        "/",
        "stripping to empty normalizes to /"
    );
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
