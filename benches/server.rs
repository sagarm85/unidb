// M5.d benchmarks: server-overhead-focused only, per the locked decision
// (see MEMORY.md/PROGRESS.md) — not the deferred cross-domain "replaced
// stack" showcase, since there is no external "REST+JWT+SSE embedded
// database server" incumbent this project is trying to beat. What matters
// here is "how much does wrapping the already-measured engine in HTTP
// cost" — an internal-only baseline against M1's `benches/load.rs` numbers.
// Run with: cargo bench --bench server --features server

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use futures_util::StreamExt;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;
use unidb::{
    server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState},
    Engine,
};

const SECRET: &str = "unidb-server-bench-secret";

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

fn bench_token() -> String {
    let claims = Claims {
        sub: "bench".into(),
        exp: (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600) as usize,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// `PrometheusMetricLayer::pair()` installs a process-global recorder —
/// calling it twice in one process panics. This whole `cargo bench`
/// process runs every benchmark function below, so the pair is obtained
/// exactly once and reused, mirroring `tests/server_common/mod.rs`'s
/// identical need.
fn metrics_pair() -> &'static (PrometheusMetricLayer<'static>, PrometheusHandle) {
    static PAIR: OnceLock<(PrometheusMetricLayer<'static>, PrometheusHandle)> = OnceLock::new();
    PAIR.get_or_init(PrometheusMetricLayer::pair)
}

/// Spawn a real server on an ephemeral port and return its base URL. The
/// backing temp-dir is intentionally leaked (`mem::forget`) — acceptable
/// for a benchmark binary that exits after one run, not something a
/// production or test code path should ever do.
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

/// (1) Direct `Engine::insert` vs. the same op via `POST /rows` — isolates
/// HTTP+writer-thread-channel overhead from engine cost. Compare against
/// M1's already-recorded `benches/load.rs` single-table INSERT numbers
/// (PROGRESS.md's M1 entry) for the full picture.
fn bench_insert_direct_vs_http(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_overhead");

    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    group.bench_function("direct_engine_insert", |b| {
        b.iter(|| {
            let xid = engine.begin().unwrap();
            engine.insert(xid, b"benchmark-row").unwrap();
            engine.commit(xid).unwrap();
        });
    });

    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", bench_token());
    group.bench_function("http_post_rows", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .post(format!("{base_url}/rows"))
                    .header("Authorization", &auth)
                    .body("benchmark-row")
                    .send()
                    .await
                    .unwrap();
            });
        });
    });

    group.finish();
}

/// (2) JWT verification cost in isolation from HTTP entirely — the
/// cleanest micro-benchmark of what `auth::require_jwt` pays per request.
fn bench_jwt_verification(c: &mut Criterion) {
    let token = bench_token();
    let decoding_key = DecodingKey::from_secret(SECRET.as_bytes());
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);

    c.bench_function("jwt_verify_single_token", |b| {
        b.iter(|| {
            let _: jsonwebtoken::TokenData<Claims> =
                decode(&token, &decoding_key, &validation).unwrap();
        });
    });
}

/// (3) SSE polling overhead at 1/10/50 simulated concurrent subscribers —
/// turns "N subscribers x poll interval x poll_events's own
/// linear-in-table-size cost" (M4's finding, restated in `sse.rs`'s module
/// doc) into a real number: connection setup + first-chunk latency per
/// subscriber, against a table with 100 pre-existing events.
fn bench_sse_polling_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_polling_overhead");
    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let auth = format!("Bearer {}", bench_token());

    rt.block_on(async {
        let client = reqwest::Client::new();
        client
            .post(format!("{base_url}/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({"sql": "CREATE TABLE sse_bench (id INT)"}))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base_url}/tables/sse_bench/events"))
            .header("Authorization", &auth)
            .send()
            .await
            .unwrap();
        for i in 0..100 {
            client
                .post(format!("{base_url}/sql"))
                .header("Authorization", &auth)
                .json(
                    &serde_json::json!({"sql": format!("INSERT INTO sse_bench (id) VALUES ({i})")}),
                )
                .send()
                .await
                .unwrap();
        }
    });

    for n in [1usize, 10, 50] {
        group.bench_with_input(BenchmarkId::new("subscribers", n), &n, |b, &n| {
            b.iter(|| {
                rt.block_on(async {
                    let mut handles = Vec::new();
                    for i in 0..n {
                        let url = base_url.clone();
                        let auth = auth.clone();
                        handles.push(tokio::spawn(async move {
                            let client = reqwest::Client::new();
                            let resp = client
                                .get(format!(
                                    "{url}/events/subscribe?consumer=bench{i}&interval_ms=50"
                                ))
                                .header("Authorization", auth)
                                .send()
                                .await
                                .unwrap();
                            let mut stream = resp.bytes_stream();
                            let _ = tokio::time::timeout(Duration::from_millis(300), stream.next())
                                .await;
                        }));
                    }
                    for h in handles {
                        let _ = h.await;
                    }
                });
            });
        });
    }
    group.finish();
}

/// (4) Concurrent HTTP throughput against the single writer thread — the
/// number that demonstrates the single-writer-thread design's actual
/// throughput ceiling under concurrent load, not an assumed-fine
/// architectural claim (CLAUDE.md §6).
fn bench_concurrent_http_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_http_throughput");
    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let auth = format!("Bearer {}", bench_token());
    rt.block_on(async {
        let client = reqwest::Client::new();
        client
            .post(format!("{base_url}/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({"sql": "CREATE TABLE throughput_bench (id INT)"}))
            .send()
            .await
            .unwrap();
    });

    for concurrency in [1usize, 10, 50] {
        group.bench_with_input(
            BenchmarkId::new("concurrent_clients", concurrency),
            &concurrency,
            |b, &n| {
                b.iter(|| {
                    rt.block_on(async {
                        let mut handles = Vec::new();
                        for _ in 0..n {
                            let url = base_url.clone();
                            let auth = auth.clone();
                            handles.push(tokio::spawn(async move {
                                let client = reqwest::Client::new();
                                client
                                    .post(format!("{url}/sql"))
                                    .header("Authorization", auth)
                                    .json(&serde_json::json!({"sql": "INSERT INTO throughput_bench (id) VALUES (1)"}))
                                    .send()
                                    .await
                                    .unwrap();
                            }));
                        }
                        for h in handles {
                            let _ = h.await;
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

/// (5) Concurrent read throughput (6b) — N clients each `GET /rows/{id}` the
/// same row. Reads run on the shared `ReadHandle`, off the single writer
/// thread, so unlike the write throughput above this should *scale* with
/// concurrency rather than sit at the single-writer ceiling.
fn bench_concurrent_read_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_read_throughput");
    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let auth = format!("Bearer {}", bench_token());

    // Insert one row and learn its RowId, then read it concurrently.
    let path = rt.block_on(async {
        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .post(format!("{base_url}/rows"))
            .header("Authorization", &auth)
            .body("concurrent-read-row")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let page_id = resp["row_id"]["page_id"].as_u64().unwrap();
        let slot = resp["row_id"]["slot"].as_u64().unwrap();
        format!("{base_url}/rows/{page_id}/{slot}")
    });

    for concurrency in [1usize, 10, 50] {
        group.bench_with_input(
            BenchmarkId::new("concurrent_clients", concurrency),
            &concurrency,
            |b, &n| {
                b.iter(|| {
                    rt.block_on(async {
                        let mut handles = Vec::new();
                        for _ in 0..n {
                            let path = path.clone();
                            let auth = auth.clone();
                            handles.push(tokio::spawn(async move {
                                let client = reqwest::Client::new();
                                client
                                    .get(&path)
                                    .header("Authorization", auth)
                                    .send()
                                    .await
                                    .unwrap();
                            }));
                        }
                        for h in handles {
                            let _ = h.await;
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

/// (6) REST enrichment (R1/R4): what the new surface actually buys.
/// - `oneshot_100_inserts` vs `session_100_inserts`: 100 INSERT statements
///   as 100 auto-commit requests (100 group-committed fsyncs) vs 100
///   requests inside one transaction session + one commit (one fsync).
/// - `single_500_post_rows` vs `batch_500_rows`: 500 raw rows as 500
///   `POST /rows` (500 txns) vs one `POST /rows/batch` (one txn).
fn bench_rest_enrichment(c: &mut Criterion) {
    let mut group = c.benchmark_group("rest_enrichment");
    let rt = tokio_rt();
    let base_url = spawn_bench_server(&rt);
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", bench_token());

    rt.block_on(async {
        client
            .post(format!("{base_url}/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({"sql": "CREATE TABLE enrich (id INT)"}))
            .send()
            .await
            .unwrap();
    });

    group.bench_function("oneshot_100_inserts", |b| {
        b.iter(|| {
            rt.block_on(async {
                for i in 0..100 {
                    let resp = client
                        .post(format!("{base_url}/sql"))
                        .header("Authorization", &auth)
                        .json(&serde_json::json!(
                            {"sql": format!("INSERT INTO enrich (id) VALUES ({i})")}
                        ))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(resp.status().as_u16(), 200);
                }
            });
        });
    });

    group.bench_function("session_100_inserts", |b| {
        b.iter(|| {
            rt.block_on(async {
                let resp = client
                    .post(format!("{base_url}/txn/begin"))
                    .header("Authorization", &auth)
                    .send()
                    .await
                    .unwrap();
                let txn = resp.json::<serde_json::Value>().await.unwrap()["txn_id"]
                    .as_u64()
                    .unwrap();
                for i in 0..100 {
                    let resp = client
                        .post(format!("{base_url}/sql"))
                        .header("Authorization", &auth)
                        .header("X-Txn-Id", txn.to_string())
                        .json(&serde_json::json!(
                            {"sql": format!("INSERT INTO enrich (id) VALUES ({i})")}
                        ))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(resp.status().as_u16(), 200);
                }
                let resp = client
                    .post(format!("{base_url}/txn/{txn}/commit"))
                    .header("Authorization", &auth)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status().as_u16(), 200);
            });
        });
    });

    use base64::Engine as _;
    let encoded: Vec<String> = (0..500)
        .map(|i| {
            base64::engine::general_purpose::STANDARD.encode(format!("bench-row-{i}").as_bytes())
        })
        .collect();

    group.bench_function("single_500_post_rows", |b| {
        b.iter(|| {
            rt.block_on(async {
                for i in 0..500 {
                    let resp = client
                        .post(format!("{base_url}/rows"))
                        .header("Authorization", &auth)
                        .body(format!("bench-row-{i}"))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(resp.status().as_u16(), 201);
                }
            });
        });
    });

    group.bench_function("batch_500_rows", |b| {
        b.iter(|| {
            rt.block_on(async {
                let resp = client
                    .post(format!("{base_url}/rows/batch"))
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({ "rows": encoded }))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status().as_u16(), 201);
            });
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_insert_direct_vs_http, bench_jwt_verification,
              bench_sse_polling_overhead, bench_concurrent_http_throughput,
              bench_concurrent_read_throughput, bench_rest_enrichment
}
criterion_main!(benches);
