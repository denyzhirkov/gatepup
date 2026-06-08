use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use gatepup_observability::Metrics;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

/// Max time to drain in-flight connections after a shutdown signal before
/// forcing exit. Bounds shutdown so a stuck connection can't hang forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Max time for a TLS handshake before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

use crate::error::ProxyError;
use crate::health::{build_health_client, run_health_checks};
use crate::proxy::{build_client, handle, ProxyClient};
use crate::snapshot::RuntimeConfig;
use crate::SharedConfig;

/// Serve a fixed snapshot (no reload). Wraps it in a swap cell and delegates to
/// [`serve_shared`]; used standalone and in tests.
pub async fn serve(
    snapshot: Arc<RuntimeConfig>,
    metrics: Arc<Metrics>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProxyError> {
    serve_shared(Arc::new(ArcSwap::from(snapshot)), metrics, shutdown).await
}

/// Serve from a shared, swappable snapshot. Listener bind addresses and TLS
/// acceptors are fixed from the initial snapshot; routing/upstreams are read
/// from the current snapshot per request, so a reload takes effect immediately.
pub async fn serve_shared(
    shared: SharedConfig,
    metrics: Arc<Metrics>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProxyError> {
    let snapshot = shared.load_full();
    let client = build_client(snapshot.connect_timeout);

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
        )));
    }

    // Active health checks for upstreams that opted in (from the initial
    // snapshot; reload-time health lifecycle is handled by the reload path).
    let health_client = build_health_client(snapshot.connect_timeout);
    for (name, upstream) in &snapshot.upstreams {
        if let Some(settings) = upstream.health.clone() {
            tracing::info!(upstream = %name, "active health checks enabled");
            handles.push(tokio::spawn(run_health_checks(
                name.clone(),
                upstream.clone(),
                settings,
                health_client.clone(),
                shutdown.clone(),
            )));
        }
    }

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

async fn accept_loop(
    tcp: TcpListener,
    listener_name: Arc<str>,
    tls: Option<TlsAcceptor>,
    shared: SharedConfig,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();
    let is_tls = tls.is_some();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, remote)) => {
                    let listener_name = listener_name.clone();
                    let shared = shared.clone();
                    let client = client.clone();
                    let metrics = metrics.clone();
                    let service = service_fn(move |req| {
                        let listener_name = listener_name.clone();
                        let shared = shared.clone();
                        let client = client.clone();
                        let metrics = metrics.clone();
                        async move {
                            Ok::<_, Infallible>(
                                handle(req, &listener_name, is_tls, shared, client, metrics, remote)
                                    .await,
                            )
                        }
                    });

                    match &tls {
                        // TLS: complete the handshake (bounded) before serving so the
                        // connection joins the graceful-drain set. Handshakes run on the
                        // accept loop for now — offloading them is a hardening follow-up.
                        Some(acceptor) => {
                            let tls = match tokio::time::timeout(
                                HANDSHAKE_TIMEOUT,
                                acceptor.accept(stream),
                            )
                            .await
                            {
                                Ok(Ok(tls)) => tls,
                                Ok(Err(err)) => {
                                    tracing::debug!(error = %err, "tls handshake failed");
                                    continue;
                                }
                                Err(_) => {
                                    tracing::debug!("tls handshake timed out");
                                    continue;
                                }
                            };
                            let conn = http1::Builder::new()
                                .serve_connection(TokioIo::new(tls), service);
                            let watched = graceful.watch(conn);
                            tokio::spawn(async move {
                                if let Err(err) = watched.await {
                                    tracing::debug!(error = %err, "connection closed with error");
                                }
                            });
                        }
                        None => {
                            let conn = http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service);
                            let watched = graceful.watch(conn);
                            tokio::spawn(async move {
                                if let Err(err) = watched.await {
                                    tracing::debug!(error = %err, "connection closed with error");
                                }
                            });
                        }
                    }
                }
                Err(err) => tracing::warn!(error = %err, "accept failed"),
            },
        }
    }

    // Stop accepting, then drain in-flight connections (bounded by DRAIN_TIMEOUT).
    drop(tcp);
    tokio::select! {
        _ = graceful.shutdown() => {
            tracing::info!(listener = %listener_name, "drained in-flight connections");
        }
        _ = tokio::time::sleep(DRAIN_TIMEOUT) => {
            tracing::warn!(listener = %listener_name, "drain timed out, forcing shutdown");
        }
    }
}
