//! Acceptance (item 20, E2): a downstream demo service consumes
//! INSERT/UPDATE/DELETE events with **at-least-once** delivery, **resumes from
//! its durable offset after a restart**, and loses **zero** events across an
//! engine crash (replay proof).

use std::sync::Arc;

use unidb::Engine;
use unidb_dispatch::{CollectingSink, Dispatcher, Filter};

/// Commit one statement in its own transaction (durable at commit via the
/// engine's group-commit force-log).
fn commit_sql(engine: &Engine, sql: &str) {
    let xid = engine.begin().expect("begin");
    engine.execute_sql(xid, sql).expect("execute");
    engine.commit(xid).expect("commit");
}

fn setup_table(engine: &Engine) {
    commit_sql(engine, "CREATE TABLE t (id INT, note TEXT)");
    engine.enable_events("t").expect("enable events");
}

/// Drain the dispatcher: run cycles until a poll returns nothing.
async fn drain(d: &Dispatcher) {
    loop {
        let report = d.run_once().await.expect("cycle");
        if report.polled == 0 {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumes_iud_at_least_once_and_acks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    setup_table(&engine);

    // Full row lifecycle → one insert, one update, one delete event.
    commit_sql(&engine, "INSERT INTO t (id, note) VALUES (1, 'a')");
    commit_sql(&engine, "UPDATE t SET note = 'b' WHERE id = 1");
    commit_sql(&engine, "DELETE FROM t WHERE id = 1");

    let sink = Arc::new(CollectingSink::new("demo"));
    let dispatcher = Dispatcher::builder(engine.clone(), "demo-consumer")
        .subscribe(Filter::table("t"), sink.clone())
        .build();

    drain(&dispatcher).await;

    let ops: Vec<String> = sink.events().iter().map(|e| e.op.clone()).collect();
    assert_eq!(
        ops,
        vec!["insert", "update", "delete"],
        "all three op kinds"
    );
    assert_eq!(sink.seqs(), vec![1, 2, 3], "in offset order, once each");
    assert_eq!(
        dispatcher
            .stats()
            .last_acked_seq
            .load(std::sync::atomic::Ordering::Relaxed),
        3,
        "offset durably advanced to the last event"
    );

    // Re-draining without new events delivers nothing more (offset respected).
    drain(&dispatcher).await;
    assert_eq!(sink.seqs(), vec![1, 2, 3], "no redelivery once acked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumes_from_durable_offset_with_zero_loss_across_crash() {
    let dir = tempfile::tempdir().unwrap();

    // ── Phase 1: deliver + ack the first three events, then "crash". ──
    let seen_before: Vec<i64>;
    {
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        setup_table(&engine);
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (1, 'a')");
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (2, 'b')");
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (3, 'c')");

        let sink = Arc::new(CollectingSink::new("demo"));
        let dispatcher = Dispatcher::builder(engine.clone(), "demo-consumer")
            .subscribe(Filter::all(), sink.clone())
            .build();
        drain(&dispatcher).await;
        seen_before = sink.seqs();
        assert_eq!(seen_before, vec![1, 2, 3]);

        // Two MORE events commit (durable), but the dispatcher never runs
        // again before the crash — they are un-acked.
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (4, 'd')");
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (5, 'e')");
        // Drop engine + dispatcher: simulate a crash/restart.
    }

    // ── Phase 2: reopen the same directory; recovery replays committed WAL. ──
    let seen_after: Vec<i64>;
    {
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        let sink = Arc::new(CollectingSink::new("demo"));
        let dispatcher = Dispatcher::builder(engine.clone(), "demo-consumer")
            .subscribe(Filter::all(), sink.clone())
            .build();
        drain(&dispatcher).await;
        seen_after = sink.seqs();
    }

    // Resume-from-offset: the restarted consumer sees ONLY the un-acked tail —
    // the durable offset survived the crash, so 1..=3 are not replayed.
    assert_eq!(
        seen_after,
        vec![4, 5],
        "resumed strictly past the durable offset"
    );

    // Zero loss: across both lifetimes every committed event was delivered
    // exactly once, none dropped by the crash.
    let mut union = seen_before.clone();
    union.extend(seen_after.clone());
    union.sort_unstable();
    assert_eq!(
        union,
        vec![1, 2, 3, 4, 5],
        "no committed event lost across crash"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_between_deliver_and_ack_redelivers() {
    let dir = tempfile::tempdir().unwrap();

    // Phase 1: deliver an event to a sink WITHOUT acking (simulating a crash in
    // the window between fan-out and the durable ack), then drop the engine.
    {
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        setup_table(&engine);
        commit_sql(&engine, "INSERT INTO t (id, note) VALUES (1, 'a')");

        // Poll + "deliver" happen, but no ack_events call — the offset stays 0.
        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "demo-consumer", 10).unwrap();
        engine.commit(xid).unwrap();
        assert_eq!(batch.len(), 1, "the event was polled and 'delivered'");
        // crash: engine dropped with offset un-advanced.
    }

    // Phase 2: a restarted dispatcher must REDELIVER the un-acked event
    // (at-least-once), not silently skip it.
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let sink = Arc::new(CollectingSink::new("demo"));
    let dispatcher = Dispatcher::builder(engine.clone(), "demo-consumer")
        .subscribe(Filter::all(), sink.clone())
        .build();
    drain(&dispatcher).await;
    assert_eq!(
        sink.seqs(),
        vec![1],
        "un-acked event redelivered after restart"
    );
}
