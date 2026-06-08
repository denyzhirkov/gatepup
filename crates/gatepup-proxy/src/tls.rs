//! TLS termination: load PEM cert/key and build a rustls `TlsAcceptor`.
//!
//! Uses the **ring** crypto provider explicitly (passed to the builder) rather
//! than relying on a process-default, so no global install is needed and the
//! aws-lc-rs build toolchain (cmake/nasm) is avoided.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use gatepup_config::TlsConfig;
use thiserror::Error;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("failed to read TLS file {path:?}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("no certificates found in {path:?}")]
    NoCerts { path: String },

    #[error("no private key found in {path:?}")]
    NoKey { path: String },

    #[error("invalid TLS material: {0}")]
    Invalid(String),
}

/// Build a `TlsAcceptor` from a listener's cert/key PEM files.
pub(crate) fn build_acceptor(tls: &TlsConfig) -> Result<TlsAcceptor, TlsError> {
    let certs = load_certs(&tls.cert)?;
    let key = load_key(&tls.key)?;

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Invalid(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TlsError::Invalid(e.to_string()))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut reader = open(path)?;
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsError::Read {
            path: path.to_string(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsError::NoCerts {
            path: path.to_string(),
        });
    }
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut reader = open(path)?;
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| TlsError::Read {
            path: path.to_string(),
            source,
        })?
        .ok_or_else(|| TlsError::NoKey {
            path: path.to_string(),
        })
}

fn open(path: &str) -> Result<BufReader<File>, TlsError> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|source| TlsError::Read {
            path: path.to_string(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn write_temp(content: &str, label: &str) -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gatepup-tls-{}-{seq}-{label}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn builds_acceptor_from_self_signed_cert() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let tls = TlsConfig {
            cert: write_temp(&cert.cert.pem(), "cert.pem"),
            key: write_temp(&cert.key_pair.serialize_pem(), "key.pem"),
        };
        assert!(build_acceptor(&tls).is_ok());
    }

    #[test]
    fn fails_on_missing_files() {
        let tls = TlsConfig {
            cert: "/no/such/cert.pem".to_string(),
            key: "/no/such/key.pem".to_string(),
        };
        assert!(matches!(build_acceptor(&tls), Err(TlsError::Read { .. })));
    }

    #[test]
    fn fails_on_empty_cert_file() {
        let tls = TlsConfig {
            cert: write_temp("", "empty-cert.pem"),
            key: write_temp("", "empty-key.pem"),
        };
        assert!(build_acceptor(&tls).is_err());
    }
}
