use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use gatepup_observability::Metrics;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Max time to drain in-flight connections after a shutdown signal before
/// forcing exit. Bounds shutdown so a stuck connection can't hang forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Max time for a TLS handshake before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

use crate::error::ProxyError;
use crate::health::{build_health_client, run_health_checks};
use crate::proxy::{build_client, handle, ProxyClient};
use crate::snapshot::{ListenerRuntime, RuntimeConfig};

/// Serve all listeners and run active health checks until `shutdown` fires.
/// Binds synchronously so bind failures surface immediately. The shared
/// `metrics` are incremented from the request path.
pub async fn serve(
    snapshot: Arc<RuntimeConfig>,
    metrics: Arc<Metrics>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProxyError> {
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
            listener.clone(),
            snapshot.clone(),
            client.clone(),
            metrics.clone(),
            shutdown.clone(),
        )));
    }

    // Active health checks for upstreams that opted in.
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
    listener: Arc<ListenerRuntime>,
    config: Arc<RuntimeConfig>,
    client: ProxyClient,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, remote)) => {
                    let svc_listener = listener.clone();
                    let config = config.clone();
                    let client = client.clone();
                    let metrics = metrics.clone();
                    let service = service_fn(move |req| {
                        let listener = svc_listener.clone();
                        let config = config.clone();
                        let client = client.clone();
                        let metrics = metrics.clone();
                        async move {
                            Ok::<_, Infallible>(
                                handle(req, listener, config, client, metrics, remote).await,
                            )
                        }
                    });

                    match &listener.tls {
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
            tracing::info!(listener = %listener.name, "drained in-flight connections");
        }
        _ = tokio::time::sleep(DRAIN_TIMEOUT) => {
            tracing::warn!(listener = %listener.name, "drain timed out, forcing shutdown");
        }
    }
}
