// The durability-contract proof for M4 (M4.c): a naive WAL-tailing queue
// design would either block WAL truncation for a slow consumer or lose
// events out from under it. Since M4.a copies events into an ordinary
// durable `__events__` heap table at write time, `checkpoint.rs::run()`'s
// unconditional WAL truncation is *structurally* incapable of caring how
// far behind a consumer is — `wal_truncation_is_unaffected_by_consumer_lag`
// below proves this with a concrete test, not just an inference from
// reading `checkpoint.rs`.

use tempfile::tempdir;
use unidb::Engine;

fn insert_events(engine: &mut Engine, n: i64) {
    let xid = engine.begin().unwrap();
    for i in 1..=n {
        engine
            .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({i})"))
            .unwrap();
    }
    engine.commit(xid).unwrap();
}

fn setup(engine: &mut Engine) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(xid).unwrap();
    engine.enable_events("t").unwrap();
}

#[test]
fn wal_truncation_is_unaffected_by_consumer_lag() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);

    // Register a consumer that will never ack, then generate events and
    // force multiple checkpoints (WAL truncations). If truncation were
    // coupled to consumer lag, this would hang, error, or lose events the
    // slow consumer hasn't seen yet — none of that should happen.
    let ack_xid = engine.begin().unwrap();
    engine.ack_events(ack_xid, "slow", 0).unwrap();
    engine.commit(ack_xid).unwrap();

    for _ in 0..5 {
        insert_events(&mut engine, 10);
        engine.checkpoint().unwrap();
    }

    let xid = engine.begin().unwrap();
    let batch = engine.poll_events(xid, "slow", 1000).unwrap();
    assert_eq!(
        batch.len(),
        50,
        "the never-acking consumer must still see every event; WAL \
         truncation must not have dropped or blocked on any of them"
    );
}

#[test]
fn slow_consumer_survives_vacuum_fast_consumer_does_not_block_it() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);
    insert_events(&mut engine, 5);

    let xid = engine.begin().unwrap();
    let batch = engine.poll_events(xid, "fast", 10).unwrap();
    engine
        .ack_events(xid, "fast", batch.last().unwrap().seq)
        .unwrap();
    engine.ack_events(xid, "slow", 0).unwrap();
    engine.commit(xid).unwrap();

    let vacuum_xid = engine.begin().unwrap();
    let reclaimed = engine.vacuum_events(vacuum_xid).unwrap();
    engine.commit(vacuum_xid).unwrap();

    assert_eq!(
        reclaimed, 0,
        "vacuum must bound reclaim to min(offsets), not the fastest consumer"
    );

    let check_xid = engine.begin().unwrap();
    assert_eq!(
        engine.poll_events(check_xid, "slow", 10).unwrap().len(),
        5,
        "slow consumer's events must survive vacuum untouched"
    );
}

#[test]
fn vacuum_is_noop_with_zero_registered_consumers() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);
    insert_events(&mut engine, 5);

    let xid = engine.begin().unwrap();
    let reclaimed = engine.vacuum_events(xid).unwrap();
    engine.commit(xid).unwrap();
    assert_eq!(reclaimed, 0);

    let check_xid = engine.begin().unwrap();
    assert_eq!(
        engine.poll_events(check_xid, "anyone", 10).unwrap().len(),
        5,
        "nothing should have been reclaimed with no registered consumers"
    );
}

#[test]
fn vacuum_reclaims_up_to_min_offset_when_consumers_advance() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine);
    insert_events(&mut engine, 10);

    let xid = engine.begin().unwrap();
    let all = engine.poll_events(xid, "a", 100).unwrap();
    // "a" acks through seq 7, "b" acks through seq 3 — min is 3.
    engine.ack_events(xid, "a", all[6].seq).unwrap();
    engine.ack_events(xid, "b", all[2].seq).unwrap();
    engine.commit(xid).unwrap();

    let vacuum_xid = engine.begin().unwrap();
    let reclaimed = engine.vacuum_events(vacuum_xid).unwrap();
    engine.commit(vacuum_xid).unwrap();
    assert_eq!(reclaimed, 3, "must reclaim exactly seq <= min(offsets) = 3");

    let check_xid = engine.begin().unwrap();
    let remaining = engine.poll_events(check_xid, "never-acked", 100).unwrap();
    assert_eq!(
        remaining.len(),
        7,
        "rows above min_offset must still be poll-able"
    );
    assert!(remaining.iter().all(|e| e.seq > all[2].seq));
}
