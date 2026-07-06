// The single most important test in M4 (mirroring M2.d's
// `tests/vector_mvcc.rs` and M3.d's `tests/graph_mvcc.rs`): event capture
// is synchronous (M4.a — a durable `heap.insert` under the writing
// transaction's own xid), so unlike M2's background-worker index, there is
// no "did the worker catch up yet" race to account for. What must be
// proven instead is that the event row's fate really is tied to the
// surrounding transaction's fate via the ordinary MVCC/abort machinery —
// self-visible before abort, permanently invisible after.

use tempfile::tempdir;
use unidb::Engine;

fn setup(engine: &mut Engine) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(xid).unwrap();
    engine.enable_events("t").unwrap();
}

#[test]
fn aborted_event_insert_is_self_visible_then_invisible_to_fresh_txn() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);

    let doomed = engine.begin().unwrap();
    engine
        .execute_sql(doomed, "INSERT INTO t (id) VALUES (1)")
        .unwrap();

    // Self-visibility: the inserting transaction sees its own uncommitted
    // event immediately — ordinary MVCC, confirmed as a precondition
    // before aborting (proves the event row genuinely exists pre-abort,
    // not that a fresh transaction happens to find nothing either way).
    let self_view = engine.poll_events(doomed, "c", 10).unwrap();
    assert_eq!(
        self_view.len(),
        1,
        "the inserting transaction must see its own uncommitted event"
    );

    engine.abort(doomed).unwrap();

    let fresh = engine.begin().unwrap();
    let fresh_view = engine.poll_events(fresh, "c", 10).unwrap();
    assert!(
        fresh_view.is_empty(),
        "aborted transaction's event leaked into a fresh transaction's view: {fresh_view:?}"
    );
}

#[test]
fn aborted_ack_does_not_durably_advance_offset() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);

    let xid = engine.begin().unwrap();
    for i in 1..=3 {
        engine
            .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({i})"))
            .unwrap();
    }
    engine.commit(xid).unwrap();

    let doomed = engine.begin().unwrap();
    let all = engine.poll_events(doomed, "c", 10).unwrap();
    assert_eq!(all.len(), 3);
    engine.ack_events(doomed, "c", all[1].seq).unwrap();

    // Self-visibility of the uncommitted ack.
    let self_view = engine.poll_events(doomed, "c", 10).unwrap();
    assert_eq!(
        self_view.len(),
        1,
        "the acking transaction must see its own uncommitted offset advance"
    );

    engine.abort(doomed).unwrap();

    let fresh = engine.begin().unwrap();
    let fresh_view = engine.poll_events(fresh, "c", 10).unwrap();
    assert_eq!(
        fresh_view.len(),
        3,
        "aborted ack must not durably advance the offset a fresh transaction sees"
    );
}
