use std::convert::Infallible;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::error::ProxyError;
use crate::proxy::{build_client, handle, ProxyClient};
use crate::snapshot::{ListenerRuntime, RuntimeConfig};

/// Bind every listener and serve until Ctrl-C. Returns once all accept loops
/// have stopped after a shutdown signal.
pub async fn run(snapshot: Arc<RuntimeConfig>) -> Result<(), ProxyError> {
    let client = build_client();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutdown signal received");
            let _ = shutdown_tx.send(true);
        }
    });

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
            shutdown_rx.clone(),
        )));
    }

    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}

async fn accept_loop(
    tcp: TcpListener,
    listener: Arc<ListenerRuntime>,
    config: Arc<RuntimeConfig>,
    client: ProxyClient,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, remote)) => {
                    let listener = listener.clone();
                    let config = config.clone();
                    let client = client.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            let listener = listener.clone();
                            let config = config.clone();
                            let client = client.clone();
                            async move {
                                Ok::<_, Infallible>(
                                    handle(req, listener, config, client, remote).await,
                                )
                            }
                        });
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                            tracing::debug!(error = %err, "connection closed with error");
                        }
                    });
                }
                Err(err) => tracing::warn!(error = %err, "accept failed"),
            },
        }
    }
}
