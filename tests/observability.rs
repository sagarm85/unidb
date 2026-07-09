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
