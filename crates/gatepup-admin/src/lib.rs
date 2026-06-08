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

use arc_swap::ArcSwap;
use gatepup_observability::Metrics;
use gatepup_proxy::SharedConfig;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
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

/// Serve the admin endpoints until `shutdown` fires.
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

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            let state = state.clone();
                            async move { Ok::<_, Infallible>(api::route(&state, req)) }
                        });
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                            tracing::debug!(error = %err, "admin connection closed with error");
                        }
                    });
                }
                Err(err) => tracing::warn!(error = %err, "admin accept failed"),
            },
        }
    }
    Ok(())
}
