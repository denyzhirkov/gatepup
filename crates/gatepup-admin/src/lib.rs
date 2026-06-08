//! Admin / management HTTP server.
//!
//! Serves read-only introspection (`/health`, `/routes`, `/upstreams`,
//! `/config/effective`) and the Prometheus metrics endpoint, on a single bind
//! (default `127.0.0.1:8080`). It depends on `gatepup-proxy` for the runtime
//! snapshot views and on `gatepup-observability` for metrics — never the other
//! way around.

mod api;
mod health;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use gatepup_observability::Metrics;
use gatepup_proxy::SharedConfig;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("failed to bind admin server on {bind}: {source}")]
    Bind {
        bind: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Everything the admin handlers need. Reads the CURRENT snapshot/effective
/// config so the admin API reflects hot reloads.
pub struct AdminState {
    pub bind: SocketAddr,
    pub snapshot: SharedConfig,
    pub metrics: Arc<Metrics>,
    /// Metrics endpoint path, or `None` when metrics are disabled.
    pub metrics_path: Option<String>,
    /// Pre-rendered effective config JSON for `/config/effective` (swapped on reload).
    pub effective_config: Arc<ArcSwap<String>>,
    pub version: &'static str,
}

/// Max time to drain in-flight admin connections after shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve the admin endpoints until `shutdown` fires, then drain in-flight
/// connections (bounded by `DRAIN_TIMEOUT`).
pub async fn serve(
    state: Arc<AdminState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), AdminError> {
    let tcp = TcpListener::bind(state.bind)
        .await
        .map_err(|source| AdminError::Bind {
            bind: state.bind,
            source,
        })?;
    tracing::info!(bind = %state.bind, "admin listening");

    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(api::route(&state, req)) }
                    });
                    let conn = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    let watched = graceful.watch(conn);
                    tokio::spawn(async move {
                        if let Err(err) = watched.await {
                            tracing::debug!(error = %err, "admin connection closed with error");
                        }
                    });
                }
                Err(err) => tracing::warn!(error = %err, "admin accept failed"),
            },
        }
    }

    drop(tcp);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(DRAIN_TIMEOUT) => {
            tracing::warn!("admin drain timed out");
        }
    }
    Ok(())
}
