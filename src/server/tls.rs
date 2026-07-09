//! Native TLS termination (P6.f).
//!
//! Serves HTTPS directly with **rustls** (no OpenSSL, no reverse-proxy
//! assumption) when `UNIDB_TLS_CERT` + `UNIDB_TLS_KEY` point at PEM files;
//! otherwise the server falls back to plain HTTP (unchanged pre-P6.f behavior).
//! This lifts the earlier "assume a TLS-terminating reverse proxy" limitation
//! noted in `auth.rs`.

use std::path::Path;

use axum_server::tls_rustls::RustlsConfig;

use crate::error::{DbError, Result};

/// Install the process-wide rustls crypto provider (aws-lc-rs). Must be called
/// once before building any rustls config. Idempotent — a second call is a
/// harmless no-op (rustls only accepts the first).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Cert + key PEM paths from the environment, if TLS is configured.
pub fn tls_paths_from_env() -> Option<(String, String)> {
    match (
        std::env::var("UNIDB_TLS_CERT"),
        std::env::var("UNIDB_TLS_KEY"),
    ) {
        (Ok(cert), Ok(key)) => Some((cert, key)),
        _ => None,
    }
}

/// Build a rustls server config from a certificate chain + private key PEM.
pub async fn load_rustls_config(cert: &Path, key: &Path) -> Result<RustlsConfig> {
    RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(DbError::Io)
}
