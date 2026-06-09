use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatepup_admin::AdminState;
use gatepup_config::{
    AppConfig, GatePupConfig, HealthCheckConfig, ListenerConfig, MatchConfig, Protocol,
    RouteConfig, TargetConfig, UpstreamConfig,
};
use gatepup_observability::Metrics;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpStream;
use tokio::sync::watch;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn sample_config() -> GatePupConfig {
    GatePupConfig {
        app: AppConfig {
            name: "gatepup-admin-test".to_string(),
            log_level: "error".to_string(),
        },
        listeners: vec![ListenerConfig {
            name: "public".to_string(),
            bind: "127.0.0.1:0".to_string(),
            protocol: Protocol::Http,
            tls: None,
            routes: vec![RouteConfig {
                name: "api".to_string(),
                matcher: MatchConfig {
                    host: Some("api.example.com".to_string()),
                    path_prefix: Some("/".to_string()),
                },
                upstream: "api".to_string(),
                strip_prefix: false,
            }],
        }],
        upstreams: vec![UpstreamConfig {
            name: "api".to_string(),
            load_balancing: Default::default(),
            targets: vec![TargetConfig {
                url: "http://127.0.0.1:4000".to_string(),
                weight: 1,
            }],
            health_check: Some(HealthCheckConfig {
                enabled: true,
                path: "/health".to_string(),
                interval_seconds: 10,
                timeout_ms: 500,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            }),
            retries: None,
        }],
        timeouts: Default::default(),
        limits: Default::default(),
        trusted_proxies: Vec::new(),
        admin: None,
        metrics: None,
    }
}

async fn spawn_admin() -> u16 {
    let config = sample_config();
    let snapshot = Arc::new(arc_swap::ArcSwap::from_pointee(
        gatepup_proxy::build_snapshot(&config).unwrap(),
    ));
    let metrics = Arc::new(Metrics::new().unwrap());
    metrics.inc_requests(); // ensure a non-zero counter shows up in /metrics
    let effective_config = Arc::new(arc_swap::ArcSwap::from_pointee(
        serde_json::to_string_pretty(&config).unwrap(),
    ));
    let port = free_port();

    let state = Arc::new(AdminState {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        snapshot,
        metrics,
        metrics_path: Some("/metrics".to_string()),
        effective_config,
        version: "0.1.0-test",
    });

    let (_tx, rx) = watch::channel(false);
    // Keep the sender alive for the duration of the test by leaking it; the
    // runtime tears everything down when the test ends.
    Box::leak(Box::new(_tx));
    tokio::spawn(async move {
        let _ = gatepup_admin::serve(state, rx).await;
    });

    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    port
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn request(port: u16, method: Method, path: &str) -> Response<hyper::body::Incoming> {
    let req = Request::builder()
        .method(method)
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    client().request(req).await.unwrap()
}

async fn body_string(resp: Response<hyper::body::Incoming>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn health_endpoint() {
    let port = spawn_admin().await;
    let resp = request(port, Method::GET, "/health").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("\"status\":\"ok\""), "{body}");
    assert!(body.contains("0.1.0-test"), "{body}");
}

#[tokio::test]
async fn routes_endpoint() {
    let port = spawn_admin().await;
    let body = body_string(request(port, Method::GET, "/routes").await).await;
    assert!(body.contains("\"api\""), "{body}");
    assert!(body.contains("api.example.com"), "{body}");
}

#[tokio::test]
async fn upstreams_endpoint_reports_health() {
    let port = spawn_admin().await;
    let body = body_string(request(port, Method::GET, "/upstreams").await).await;
    assert!(body.contains("\"name\":\"api\""), "{body}");
    assert!(body.contains("\"healthy\":true"), "{body}");
    assert!(body.contains("lastCheckEpochMs"), "{body}");
}

#[tokio::test]
async fn effective_config_endpoint() {
    let port = spawn_admin().await;
    let body = body_string(request(port, Method::GET, "/config/effective").await).await;
    assert!(body.contains("gatepup-admin-test"), "{body}");
}

#[tokio::test]
async fn metrics_endpoint_is_prometheus_text() {
    let port = spawn_admin().await;
    let resp = request(port, Method::GET, "/metrics").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    let body = body_string(resp).await;
    assert!(body.contains("gatepup_requests_total"), "{body}");
    // Health gauge is set from the snapshot at scrape time.
    assert!(
        body.contains("gatepup_upstream_healthy{upstream=\"api\"} 1"),
        "{body}"
    );
}

#[tokio::test]
async fn unknown_path_is_404() {
    let port = spawn_admin().await;
    let resp = request(port, Method::GET, "/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_string(resp).await.contains("not_found"));
}

#[tokio::test]
async fn non_get_is_405() {
    let port = spawn_admin().await;
    let resp = request(port, Method::POST, "/health").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(body_string(resp).await.contains("method_not_allowed"));
}
