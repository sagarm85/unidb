// M8.c benchmark: attach-client overhead vs. direct `Engine::execute_sql` and
// vs. raw `reqwest::blocking` calls to the same route.  The goal is "how much
// does the `AttachClient` wrapper add on top of raw reqwest" — the answer
// should be very close to zero.
//
// Run with: cargo bench --bench attach -p unidb-attach

use std::sync::{Arc, OnceLock};

use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use criterion::{criterion_group, criterion_main, Criterion};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use tempfile::tempdir;
use unidb::{
    server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState},
    Engine,
};
use unidb_attach::AttachClient;

const SECRET: &str = "unidb-attach-bench-secret";

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

fn bench_token() -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
        + 3600;
    encode(
        &Header::default(),
        &Claims {
            sub: "bench".into(),
            exp,
        },
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

// `PrometheusMetricLayer::pair()` installs a process-global recorder — calling
// it more than once in one process panics (see `benches/server.rs` for the
// full explanation).
fn metrics_pair() -> &'static (PrometheusMetricLayer<'static>, PrometheusHandle) {
    static PAIR: OnceLock<(PrometheusMetricLayer<'static>, PrometheusHandle)> = OnceLock::new();
    PAIR.get_or_init(PrometheusMetricLayer::pair)
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// Spawn a real server on an ephemeral port and return its base URL.
/// The backing temp-dir is intentionally leaked (`mem::forget`) — acceptable
/// for a benchmark binary that exits after one run.
fn spawn_bench_server(rt: &tokio::runtime::Runtime) -> String {
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        std::mem::forget(dir);

        let engine = EngineHandle::spawn(&dir_path, 0).unwrap();
        let state = AppState::new(Arc::new(engine));
        let jwt_config = JwtConfig::new(SECRET);
        let (layer, handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, layer, handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    })
}

/// Three-way comparison for `execute_sql("INSERT ...")`:
///
/// 1. `direct_engine`  — `Engine::execute_sql` called in-process, no HTTP.
///    This is the hard floor: every millisecond above this is HTTP overhead.
/// 2. `raw_reqwest`    — `reqwest::blocking::Client::post(...).json(...).send()`.
///    This is the cost of HTTP + JSON serialization, no wrapper.
/// 3. `attach_client`  — `AttachClient::execute_sql(...)`.
///    Difference from `raw_reqwest` is the wrapper's own overhead (should be ~0).
fn bench_execute_sql(c: &mut Criterion) {
    let mut group = c.benchmark_group("attach_execute_sql");

    // ── baseline: direct embedded engine ────────────────────────────────────
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE bench (id INT)")
        .unwrap();
    engine.commit(xid).unwrap();
    group.bench_function("direct_engine", |b| {
        b.iter(|| {
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, "INSERT INTO bench (id) VALUES (1)")
                .unwrap();
            engine.commit(xid).unwrap();
        });
    });

    // ── raw reqwest (no AttachClient wrapper) ────────────────────────────────
    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let token = bench_token();
    let auth_header = format!("Bearer {token}");

    // Pre-create the table.
    rt.block_on(async {
        reqwest::Client::new()
            .post(format!("{base_url}/sql"))
            .header("Authorization", &auth_header)
            .json(&serde_json::json!({"sql": "CREATE TABLE bench (id INT)"}))
            .send()
            .await
            .unwrap();
    });

    let raw = reqwest::blocking::Client::new();
    let base_url_clone = base_url.clone();
    let auth_clone = auth_header.clone();
    group.bench_function("raw_reqwest", |b| {
        b.iter(|| {
            raw.post(format!("{base_url_clone}/sql"))
                .header("Authorization", &auth_clone)
                .json(&serde_json::json!({"sql": "INSERT INTO bench (id) VALUES (1)"}))
                .send()
                .unwrap();
        });
    });

    // ── AttachClient wrapper ─────────────────────────────────────────────────
    let client = AttachClient::new(&base_url, &token).unwrap();
    group.bench_function("attach_client", |b| {
        b.iter(|| {
            client
                .execute_sql("INSERT INTO bench (id) VALUES (1)")
                .unwrap();
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_execute_sql
}
criterion_main!(benches);
