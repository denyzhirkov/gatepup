//! Minimal, fast HTTP backend for load testing GatePup.
//!
//! Answers every request with `200 ok` and an `X-Backend: <id>` header so the
//! load harness can see which upstream target served a request (round-robin /
//! resilience visibility). Usage: `bench-backend [BIND] [ID]`
//! (defaults `127.0.0.1:9000` and `0`).

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http::{HeaderValue, Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let bind: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:9000".to_string())
        .parse()
        .expect("valid bind address");
    let id: &'static str = Box::leak(
        args.next()
            .unwrap_or_else(|| "0".to_string())
            .into_boxed_str(),
    );
    // Optional per-request delay to keep requests in-flight (graceful-shutdown tests).
    let delay_ms: u64 = std::env::var("GATEPUP_BENCH_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let listener = TcpListener::bind(bind).await.expect("bind backend");
    eprintln!("bench-backend {id} listening on {bind} (delay {delay_ms}ms)");

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |_req: Request<Incoming>| async move {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let mut resp = Response::new(Full::new(Bytes::from_static(b"ok")));
                resp.headers_mut()
                    .insert("x-backend", HeaderValue::from_static(id));
                Ok::<_, Infallible>(resp)
            });
            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}
