use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use gatepup_observability::Metrics;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

/// Max time to drain in-flight connections after a shutdown signal before
/// forcing exit. Bounds shutdown so a stuck connection can't hang forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Max time for a TLS handshake before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection-level inbound limits, fixed from the initial snapshot (they apply
/// when building each connection, like the bind address and TLS acceptor).
#[derive(Clone, Copy)]
struct ConnLimits {
    header_read_timeout: Option<Duration>,
    max_header_bytes: Option<usize>,
}

use crate::error::ProxyError;
use crate::health::run_health_supervisor;
use crate::proxy::{build_client, handle, ProxyClient};
use crate::rate_limit::RateLimiter;
use crate::snapshot::RuntimeConfig;
use crate::SharedConfig;

/// Serve a fixed snapshot (no reload). Wraps it in a swap cell and delegates to
/// [`serve_shared`]; used standalone and in tests.
pub async fn serve(
    snapshot: Arc<RuntimeConfig>,
    metrics: Arc<Metrics>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProxyError> {
    // No reload trigger; the sender stays alive for the duration of serve_shared.
    let (_reload_tx, reload_rx) = watch::channel(0u64);
    serve_shared(
        Arc::new(ArcSwap::from(snapshot)),
        metrics,
        shutdown,
        reload_rx,
    )
    .await
}

/// Serve from a shared, swappable snapshot. Listener bind addresses and TLS
/// acceptors are fixed from the initial snapshot; routing/upstreams are read
/// from the current snapshot per request, so a reload takes effect immediately.
/// A health supervisor (re)spawns active checks across reloads, driven by
/// `reload` (bumped after a successful swap).
pub async fn serve_shared(
    shared: SharedConfig,
    metrics: Arc<Metrics>,
    shutdown: watch::Receiver<bool>,
    reload: watch::Receiver<u64>,
) -> Result<(), ProxyError> {
    let snapshot = shared.load_full();
    let client = build_client(snapshot.connect_timeout)?;
    // Connection-level limits are fixed at startup (applied per accepted
    // connection); a reload that changes them takes effect on restart.
    let conn_limits = ConnLimits {
        header_read_timeout: snapshot.header_read_timeout,
        max_header_bytes: snapshot.max_header_bytes,
    };
    // Rate-limit state is shared across listeners and survives reloads (the
    // per-route params come from the snapshot per request; the buckets persist).
    let rate_limiter = Arc::new(RateLimiter::new());

    let mut handles = Vec::with_capacity(snapshot.listeners.len());
    for listener in &snapshot.listeners {
        let tcp = TcpListener::bind(listener.bind)
            .await
            .map_err(|source| ProxyError::Bind {
                name: listener.name.clone(),
                bind: listener.bind,
                source,
            })?;
        tracing::info!(listener = %listener.name, bind = %listener.bind, "listening");

        handles.push(tokio::spawn(accept_loop(
            tcp,
            Arc::from(listener.name.as_str()),
            listener.tls.clone(),
            shared.clone(),
            client.clone(),
            metrics.clone(),
            shutdown.clone(),
            conn_limits,
            rate_limiter.clone(),
        )));
    }

    // Active health checks are managed by a supervisor that respawns them when
    // the snapshot is reloaded.
    handles.push(tokio::spawn(run_health_supervisor(
        shared.clone(),
        snapshot.connect_timeout,
        shutdown.clone(),
        reload,
    )));

    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}

/// Convenience entry point: build metrics, install a Ctrl-C shutdown, and serve.
/// Used standalone and in tests; the orchestrated path (with the admin server)
/// calls [`serve`] directly so it can share metrics and shutdown.
pub async fn run(snapshot: Arc<RuntimeConfig>) -> Result<(), ProxyError> {
    let metrics = Arc::new(Metrics::new().map_err(|e| ProxyError::Metrics(e.to_string()))?);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutdown signal received");
            let _ = shutdown_tx.send(true);
        }
    });

    serve(snapshot, metrics, shutdown_rx).await
}

#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    tcp: TcpListener,
    listener_name: Arc<str>,
    tls: Option<TlsAcceptor>,
    shared: SharedConfig,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
    limits: ConnLimits,
    rate_limiter: Arc<RateLimiter>,
) {
    let is_tls = tls.is_some();
    // Connections are served with upgrades (WebSocket), so we drain them
    // manually (hyper_util's graceful set doesn't cover upgradeable connections):
    // each connection self-`graceful_shutdown`s on the signal; we await the set.
    let mut conns = JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, remote)) => {
                    let ctx = ConnCtx {
                        listener_name: listener_name.clone(),
                        shared: shared.clone(),
                        client: client.clone(),
                        metrics: metrics.clone(),
                        is_tls,
                        remote,
                        shutdown: shutdown.clone(),
                        limits,
                        rate_limiter: rate_limiter.clone(),
                    };
                    // Handshake (TLS) runs inside the spawned task, off the accept
                    // loop, so a slow handshake can't stall accepting new connections.
                    conns.spawn(serve_connection_maybe_tls(stream, tls.clone(), ctx));
                }
                Err(err) => tracing::warn!(error = %err, "accept failed"),
            },
        }
    }

    // Stop accepting; each connection self-drains on the shutdown signal. Wait
    // for the set to empty, bounded by DRAIN_TIMEOUT.
    drop(tcp);
    tokio::select! {
        _ = async { while conns.join_next().await.is_some() {} } => {
            tracing::info!(listener = %listener_name, "drained in-flight connections");
        }
        _ = tokio::time::sleep(DRAIN_TIMEOUT) => {
            tracing::warn!(listener = %listener_name, "drain timed out, forcing shutdown");
            conns.abort_all();
        }
    }
}

/// Complete the TLS handshake (if any) inside the connection task, then serve.
/// Bounded by `HANDSHAKE_TIMEOUT`; a failed/slow handshake drops the connection.
async fn serve_connection_maybe_tls(stream: TcpStream, tls: Option<TlsAcceptor>, ctx: ConnCtx) {
    match tls {
        Some(acceptor) => {
            let tls_stream =
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls)) => tls,
                    Ok(Err(err)) => {
                        tracing::debug!(error = %err, "tls handshake failed");
                        return;
                    }
                    Err(_) => {
                        tracing::debug!("tls handshake timed out");
                        return;
                    }
                };
            serve_connection(TokioIo::new(tls_stream), ctx).await;
        }
        None => serve_connection(TokioIo::new(stream), ctx).await,
    }
}

/// Per-connection context captured for serving.
struct ConnCtx {
    listener_name: Arc<str>,
    shared: SharedConfig,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    is_tls: bool,
    remote: std::net::SocketAddr,
    shutdown: watch::Receiver<bool>,
    limits: ConnLimits,
    rate_limiter: Arc<RateLimiter>,
}

/// Serve one connection (with upgrades) until it finishes or shutdown is
/// signaled, in which case it is gracefully shut down and then awaited.
async fn serve_connection<I>(io: TokioIo<I>, ctx: ConnCtx)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let ConnCtx {
        listener_name,
        shared,
        client,
        metrics,
        is_tls,
        remote,
        mut shutdown,
        limits,
        rate_limiter,
    } = ctx;

    let service = service_fn(move |req| {
        let listener_name = listener_name.clone();
        let shared = shared.clone();
        let client = client.clone();
        let metrics = metrics.clone();
        let rate_limiter = rate_limiter.clone();
        async move {
            Ok::<_, Infallible>(
                handle(
                    req,
                    &listener_name,
                    is_tls,
                    shared,
                    client,
                    metrics,
                    rate_limiter,
                    remote,
                )
                .await,
            )
        }
    });

    let mut builder = http1::Builder::new();
    if let Some(timeout) = limits.header_read_timeout {
        // hyper panics if header_read_timeout is set without a timer.
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(timeout);
    }
    if let Some(max) = limits.max_header_bytes {
        builder.max_buf_size(max);
    }
    let conn = builder.serve_connection(io, service).with_upgrades();
    let mut conn = std::pin::pin!(conn);
    tokio::select! {
        result = conn.as_mut() => {
            if let Err(err) = result {
                tracing::debug!(error = %err, "connection closed with error");
            }
        }
        _ = shutdown.changed() => {
            conn.as_mut().graceful_shutdown();
            if let Err(err) = conn.await {
                tracing::debug!(error = %err, "connection closed with error");
            }
        }
    }
}
