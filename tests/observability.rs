// P6.g — observability: stat counters + slow-query log.

use std::time::Duration;
use tempfile::tempdir;
use unidb::Engine;

#[test]
fn stats_track_commits_aborts_and_activity() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
    engine
        .execute_sql(x, "INSERT INTO t (id) VALUES (1)")
        .unwrap();
    engine.commit(x).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "INSERT INTO t (id) VALUES (2)")
        .unwrap();
    engine.abort(x).unwrap();

    let s = engine.stats();
    assert!(s.commits >= 1, "commits counted");
    assert!(s.aborts >= 1, "aborts counted");
    assert_eq!(s.active_transactions, 0, "no live txns after commit/abort");
    assert!(s.data_pages > 0, "data pages allocated");
}

#[test]
fn slow_query_log_disabled_by_default_and_captures_when_enabled() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Default threshold 0 → disabled: nothing recorded.
    let x = engine.begin().unwrap();
    engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(x).unwrap();
    assert!(engine.stats().recent_slow_queries.is_empty());

    // A 1µs threshold captures effectively every query (any real statement takes
    // longer than a microsecond) — deterministic.
    engine.set_slow_query_threshold(Duration::from_micros(1));
    for i in 0..40 {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, &format!("INSERT INTO t (id) VALUES ({i})"))
            .unwrap();
        engine.commit(x).unwrap();
    }
    let slow = engine.stats().recent_slow_queries;
    assert!(
        !slow.is_empty(),
        "slow queries must be captured over the threshold"
    );
    assert!(slow.len() <= 32, "the slow-query ring is bounded to 32");
    assert!(slow.iter().all(|q| !q.sql.is_empty() && q.micros >= 1));
}

/// Item 21: the enriched `stats()` surface reflects real activity at every
/// chokepoint — per-statement-kind latency, buffer-pool hit/miss, WAL fsyncs,
/// and the per-table page list.
#[test]
fn item21_stats_reflect_statement_bufferpool_wal_and_table_metrics() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
        .unwrap();
    for i in 0..50 {
        engine
            .execute_sql(
                x,
                &format!("INSERT INTO t (id, body) VALUES ({i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine
        .execute_sql(x, "UPDATE t SET body = 'x' WHERE id = 1")
        .unwrap();
    engine.execute_sql(x, "DELETE FROM t WHERE id = 2").unwrap();
    engine.execute_sql(x, "SELECT COUNT(*) FROM t").unwrap();
    engine.commit(x).unwrap();

    let s = engine.stats();

    // Per-statement-kind latency histograms counted the right shapes.
    assert!(
        s.statement_latency.insert.count >= 50,
        "insert latency histogram counted the 50 inserts, saw {}",
        s.statement_latency.insert.count
    );
    assert!(s.statement_latency.update.count >= 1, "update counted");
    assert!(s.statement_latency.delete.count >= 1, "delete counted");
    assert!(s.statement_latency.select.count >= 1, "select counted");

    // Buffer pool served the workload — hits and misses both moved.
    assert!(s.bufferpool.hits > 0, "buffer-pool hits counted");
    assert!(s.bufferpool.misses > 0, "buffer-pool misses counted");
    assert!(
        s.bufferpool.hit_ratio > 0.0 && s.bufferpool.hit_ratio <= 1.0,
        "hit ratio in (0,1], saw {}",
        s.bufferpool.hit_ratio
    );

    // At least one durable fsync happened (per-statement mode is default off,
    // group-commit forces one at commit).
    assert!(s.wal_fsyncs > 0, "wal fsyncs counted");

    // Per-table page list includes the user table with >=1 page.
    let t = s
        .tables
        .iter()
        .find(|t| t.name == "t")
        .expect("user table 't' present in per-table stats");
    assert!(t.pages >= 1, "table 't' has heap pages, saw {}", t.pages);

    // Horizon is free once the only transaction committed.
    assert_eq!(s.horizon_age_secs, 0.0, "no live snapshot after commit");

    // Worker-governance budget is initialized (>=1 core).
    assert!(s.parallel_workers.global_max >= 1, "worker budget set");
}
