use std::net::SocketAddr;

use thiserror::Error;

/// Failure while standing up the proxy runtime (binding listeners, building the
/// snapshot). Per-request failures are mapped to HTTP responses instead, see
/// `proxy::GatewayError`.
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("invalid bind address {bind:?} for listener {name:?}")]
    InvalidBind { name: String, bind: String },

    #[error("failed to bind listener {name:?} on {bind}: {source}")]
    Bind {
        name: String,
        bind: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to initialize metrics: {0}")]
    Metrics(String),
}
