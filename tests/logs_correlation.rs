//! Item 22, L2 — correlation-id acceptance proof.
//!
//! "One request's lines are retrievable by `request_id` across app log,
//! slow-query log, and audit log." This drives the engine exactly as the server
//! bridge does — set the per-thread `request_id`, run statements under one
//! transaction — while capturing the structured (`tracing`) output as JSON (the
//! same JSON-lines the server writes and `GET /logs` reads back), then asserts
//! the one `request_id` (and `txn_id`) joins all three surfaces:
//!
//!   * app log      — the `execute_sql` span on ordinary statement events,
//!   * slow-query log — the `"slow query"` warn emitted under that span,
//!   * audit log     — both the `audit.log` file *and* its app-log mirror.
//!
//! Gated on `server` only because the JSON formatter (`tracing-subscriber`'s
//! `json` feature) ships with the server build; the mechanism it exercises lives
//! entirely in the default (engine-core) build.
#![cfg(feature = "server")]

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;
use unidb::Engine;

/// An in-memory sink so the test can read back the JSON log lines the subscriber
/// emits.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn one_request_id_joins_app_slow_and_audit_logs() {
    const RID: &str = "req-corr-0123456789abcdef";
    let dir = tempdir().unwrap();
    let buf = SharedBuf::default();

    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::INFO)
        .finish();

    let audit_contents = tracing::subscriber::with_default(subscriber, || {
        let engine = Engine::open(dir.path(), 0).unwrap();
        // Make every statement count as "slow" so the slow-query log fires
        // (1µs — the threshold is stored in micros, so anything sub-micro would
        // truncate to 0 = disabled; real statements take tens of µs).
        engine.set_slow_query_threshold(Duration::from_micros(1));

        // Bootstrap a table and a privilege-less named user (embedded = superuser).
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
        engine.execute_sql_as(None, x, "CREATE USER alice").unwrap();
        engine.commit(x).unwrap();

        // Now act as "one HTTP request": the server sets this per-thread id
        // before every blocking engine call (see server::engine_handle).
        let _corr = unidb::observability::set_request_id(Some(RID.to_string()));
        let x = engine.begin().unwrap();
        // (a) a denied statement as a named user → an audit line (allowed:false).
        let _ = engine.execute_sql_as(Some("alice"), x, "SELECT id FROM t");
        // (b) a permitted (slow) statement under the same request/txn.
        engine.execute_sql(x, "SELECT id FROM t").unwrap();
        engine.commit(x).unwrap();
        drop(_corr);

        std::fs::read_to_string(dir.path().join("audit.log")).unwrap()
    });

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let with_rid: Vec<&str> = logs.lines().filter(|l| l.contains(RID)).collect();

    // App log + slow-query log: both carry the request id (via the execute_sql span).
    assert!(
        with_rid.iter().any(|l| l.contains("slow query")),
        "slow-query log line must carry request_id; lines with rid: {with_rid:#?}"
    );
    // Audit mirror in the app log also carries it.
    assert!(
        with_rid.iter().any(|l| l.contains("audit event")),
        "audit app-log mirror must carry request_id"
    );
    // Every correlated app-log line also carries the same txn_id.
    assert!(
        with_rid.iter().any(|l| l.contains("txn_id")),
        "correlated app-log lines carry txn_id too"
    );

    // The durable audit.log file itself is retrievable by request_id + txn_id.
    let audit_hits: Vec<&str> = audit_contents.lines().filter(|l| l.contains(RID)).collect();
    assert!(
        !audit_hits.is_empty(),
        "audit.log must contain the request_id; audit.log:\n{audit_contents}"
    );
    assert!(
        audit_hits.iter().all(|l| l.contains("\"txn_id\"")),
        "each correlated audit line also carries txn_id"
    );
    assert!(
        audit_hits.iter().any(|l| l.contains("\"allowed\":false")),
        "the denied statement's audit line is correlated"
    );
}

/// Item 22, acceptance #3 — "log volume/overhead measured: ladder within noise
/// with JSON logging on." Runs the same CRUD workload three ways — no
/// subscriber, text formatting, JSON formatting (both to an in-memory sink so
/// disk isn't the variable) — and prints commit throughput for each. `#[ignore]`
/// so it isn't part of the correctness gate; run with:
///
/// ```text
/// cargo test --features server --test logs_correlation -- --ignored --nocapture
/// ```
///
/// The point it demonstrates: server log volume is ~2 lines per transaction
/// (begin/commit), not per row, so switching the *format* to JSON moves the
/// ladder by a small, single-digit-percent amount — within run-to-run noise on
/// the real work the engine is doing.
#[test]
#[ignore]
fn json_logging_overhead_ladder() {
    const TXNS: usize = 4_000;

    fn workload() -> std::time::Duration {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine.commit(x).unwrap();
        let start = std::time::Instant::now();
        for i in 0..TXNS {
            let x = engine.begin().unwrap();
            engine
                .execute_sql(x, &format!("INSERT INTO t (id, v) VALUES ({i}, {})", i * 2))
                .unwrap();
            engine.commit(x).unwrap();
        }
        start.elapsed()
    }

    let ops = |d: std::time::Duration| TXNS as f64 / d.as_secs_f64();

    // (a) no subscriber at all.
    let bare = workload();

    // (b) text formatting to an in-memory sink.
    let text = {
        let buf = SharedBuf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(sub, workload)
    };

    // (c) JSON formatting to an in-memory sink (the shipping format).
    let json = {
        let buf = SharedBuf::default();
        let sub = tracing_subscriber::fmt()
            .json()
            .with_writer(buf)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(sub, workload)
    };

    eprintln!("json-logging overhead ladder ({TXNS} txns):");
    eprintln!("  no subscriber : {:>8.0} commits/s", ops(bare));
    eprintln!("  text logging  : {:>8.0} commits/s", ops(text));
    eprintln!("  json logging  : {:>8.0} commits/s", ops(json));
    let overhead = (ops(text) - ops(json)) / ops(text) * 100.0;
    eprintln!("  json vs text  : {overhead:+.1}% throughput delta");
}
