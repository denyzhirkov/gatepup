use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatepup_config::{
    AppConfig, GatePupConfig, ListenerConfig, MatchConfig, Protocol, RouteConfig, TargetConfig,
    UpstreamConfig,
};
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};

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
        }],
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
    let req = Request::builder()
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    client().request(req).await.unwrap()
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
