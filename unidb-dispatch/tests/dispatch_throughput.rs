//! Ignored-by-default throughput / scale probe for the dispatcher (item 20,
//! §0.6). Run with:
//!   cargo test -p unidb-dispatch --release --test dispatch_throughput -- --ignored --nocapture
//!
//! It exists to surface — honestly and in-repo — the dispatcher's scaling
//! shape, which is dominated by M4's documented cost model: `poll_events` has
//! **no predicate pushdown**, so each poll pass is O(total `__events__` rows),
//! and draining N events in batches of `limit` costs ≈ O(N²/limit) poll work.
//! That is the queue's property, inherited — not something the dispatcher adds
//! or can fix without an engine-side `seq` index (tracked in the M4 tech debt).

use std::sync::Arc;
use std::time::Instant;

use unidb::Engine;
use unidb_dispatch::{CollectingSink, Dispatcher, Filter};

fn commit_sql(engine: &Engine, sql: &str) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
}

async fn measure(n: usize, poll_limit: usize) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    commit_sql(&engine, "CREATE TABLE t (id INT, note TEXT)");
    engine.enable_events("t").unwrap();

    let t_ingest = Instant::now();
    for i in 0..n {
        commit_sql(
            &engine,
            &format!("INSERT INTO t (id, note) VALUES ({i}, 'row-{i}')"),
        );
    }
    let ingest = t_ingest.elapsed();

    let sink = Arc::new(CollectingSink::new("bench"));
    let dispatcher = Dispatcher::builder(engine.clone(), "bench-consumer")
        .subscribe(Filter::all(), sink.clone())
        .poll_limit(poll_limit)
        .build();

    let t_drain = Instant::now();
    loop {
        let report = dispatcher.run_once().await.unwrap();
        if report.polled == 0 {
            break;
        }
    }
    let drain = t_drain.elapsed();

    assert_eq!(sink.events().len(), n, "every event delivered exactly once");
    println!(
        "N={n:>6} limit={poll_limit:>4}  ingest={:>7.1}ms ({:>8.0} ev/s)  \
         drain={:>7.1}ms ({:>8.0} ev/s)",
        ingest.as_secs_f64() * 1e3,
        n as f64 / ingest.as_secs_f64(),
        drain.as_secs_f64() * 1e3,
        n as f64 / drain.as_secs_f64(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "throughput probe; run explicitly with --ignored --release"]
async fn dispatch_throughput_scale() {
    for (n, limit) in [(1000usize, 512usize), (2000, 512), (4000, 512)] {
        measure(n, limit).await;
    }
}
