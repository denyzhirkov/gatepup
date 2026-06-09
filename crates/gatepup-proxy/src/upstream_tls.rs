//! Shared builder for the upstream HTTPS connector (used by the proxy client and
//! the health-check client). `insecure` skips certificate verification for
//! internal self-signed backends; otherwise certs are verified against the
//! system root store. The ring crypto provider is used explicitly (consistent
//! with TLS termination — no global default provider install).

use std::sync::Arc;
use std::time::Duration;

use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
};

use crate::error::ProxyError;

pub(crate) fn https_connector(
    connect_timeout: Duration,
    insecure: bool,
) -> Result<HttpsConnector<HttpConnector>, ProxyError> {
    let provider = Arc::new(ring::default_provider());

    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(connect_timeout));
    // Let https:// URIs through to the TLS layer (HttpConnector rejects them by
    // default).
    http.enforce_http(false);

    let builder = if insecure {
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| ProxyError::UpstreamTls(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth();
        HttpsConnectorBuilder::new().with_tls_config(config)
    } else {
        HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(provider)
            .map_err(|e| ProxyError::UpstreamTls(e.to_string()))?
    };

    Ok(builder.https_or_http().enable_http1().wrap_connector(http))
}

/// Certificate verifier that accepts any server certificate. Used only when an
/// upstream opts into `tlsInsecureSkipVerify`. Signature checks still run through
/// the crypto provider, so it is not a no-op TLS — it only skips trust-chain
/// validation.
#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
