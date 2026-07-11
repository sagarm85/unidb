//! Native TLS termination over the wire (P6.f): a real HTTPS server with a
//! self-signed cert, hit by an HTTPS client.

#[path = "server_common/mod.rs"]
mod server_common;

use std::sync::Arc;

use axum_prometheus::PrometheusMetricLayer;
use axum_server::tls_rustls::RustlsConfig;
use server_common::{valid_token, TEST_JWT_SECRET};
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};

#[tokio::test]
async fn https_round_trip_with_self_signed_cert() {
    unidb::server::tls::install_crypto_provider();
    // Self-signed cert for localhost.
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = certified.cert.pem();
    let key_pem = certified.key_pair.serialize_pem();
    let config = RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
        .await
        .unwrap();

    // Build the real router over a fresh temp-dir engine.
    let tempdir = tempfile::tempdir().unwrap();
    let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
    let state = AppState::new(Arc::new(engine));
    let jwt = JwtConfig::new(TEST_JWT_SECRET);
    // This test binary is its own process, so `pair()` is called exactly once.
    let (layer, handle) = PrometheusMetricLayer::pair();
    let router = build_router(state, jwt, layer, handle);

    // Bind an ephemeral port, then serve HTTPS on it.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum_server::from_tcp_rustls(std_listener, config)
            .serve(router.into_make_service())
            .await;
    });

    // HTTPS client that accepts the self-signed cert, using rustls (avoids the
    // platform native-TLS handshake quirks against a rustls server).
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .use_rustls_tls()
        .build()
        .unwrap();
    let auth = format!("Bearer {}", valid_token());
    let url = format!("https://localhost:{}/sql", addr.port());

    // Retry briefly while the TLS acceptor comes up.
    let mut last_err = None;
    let mut status = None;
    for _ in 0..50 {
        match client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "sql": "CREATE TABLE t (id INT)" }))
            .send()
            .await
        {
            Ok(resp) => {
                status = Some(resp.status());
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
    task.abort();
    assert_eq!(
        status,
        Some(reqwest::StatusCode::OK),
        "HTTPS /sql must succeed (last error: {last_err:?})"
    );
}
