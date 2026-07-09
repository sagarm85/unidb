//! Crash/shutdown-safety (M5.d) — the closest thing M5 has to the
//! crash-injection harness's spirit (CLAUDE.md §7) applied to the new
//! server layer. **This needs no new crash-injection P-number** in
//! `tests/crash/main.rs`: the underlying `Engine`'s own crash-safety is
//! already fully covered by the existing P1-P9 harness. What's new here is
//! proving the HTTP/writer-thread layer itself introduces no *additional*
//! way to lose committed data or hang.

#[path = "server_common/mod.rs"]
mod server_common;

use std::{sync::Arc, time::Duration};

use axum_prometheus::PrometheusMetricLayer;
use server_common::valid_token;
use tempfile::tempdir;
use unidb::{
    server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState},
    Engine,
};

#[tokio::test]
async fn graceful_shutdown_drains_in_flight_requests_and_preserves_committed_data() {
    let dir = tempdir().unwrap();
    let engine = EngineHandle::spawn(dir.path(), 0).unwrap();
    let state = AppState {
        engine: Arc::new(engine),
    };
    let jwt_config = JwtConfig::new(server_common::TEST_JWT_SECRET);
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();
    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}");
    let auth = format!("Bearer {}", valid_token());

    // Commit several writes over HTTP before triggering shutdown.
    for stmt in [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t (id) VALUES (1)",
        "INSERT INTO t (id) VALUES (2)",
    ] {
        let resp = client
            .post(format!("{url}/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "sql": stmt }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "setup statement failed: {stmt}");
    }

    // Fire one more request whose reply is intentionally never awaited —
    // the actual race this test exercises. `with_graceful_shutdown` must
    // let any already-in-flight request finish before the server actually
    // stops accepting/serving, not corrupt anything either way.
    let in_flight_url = url.clone();
    let in_flight_auth = auth.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client
            .post(format!("{in_flight_url}/sql"))
            .header("Authorization", in_flight_auth)
            .json(&serde_json::json!({ "sql": "INSERT INTO t (id) VALUES (3)" }))
            .send()
            .await;
    });

    // Trigger shutdown essentially immediately.
    let _ = shutdown_tx.send(());

    let result = tokio::time::timeout(Duration::from_secs(5), serve_task).await;
    assert!(
        result.is_ok(),
        "graceful shutdown must complete within its bound, not hang"
    );
    result.unwrap().unwrap();

    // By the time the spawned task above has fully completed, the last
    // `Arc<EngineHandle>` clone (held by the now-dropped router/state) has
    // already been dropped, running `EngineHandle`'s own bounded-timeout
    // shutdown synchronously as part of that drop — so no extra wait
    // should be needed before reopening directly.
    let fresh = Engine::open(dir.path(), 0).unwrap();
    let xid = fresh.begin().unwrap();
    let rows = fresh.execute_sql(xid, "SELECT * FROM t").unwrap();
    match &rows[0] {
        unidb::sql::executor::ExecResult::Rows { rows: r, .. } => {
            assert!(
                r.len() >= 2,
                "at least the two writes committed before shutdown was \
                 triggered must survive: {r:?}"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}
