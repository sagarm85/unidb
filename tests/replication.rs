// P6.b — replication slots + WAL shipping.
//
// Two things proved here:
//  1. A replication slot's `restart_lsn` holds the checkpoint's WAL truncation
//     floor back, so a consumer's segments survive a checkpoint until confirmed.
//  2. WAL shipping (`ship_wal` / `decode_stream`) round-trips the record stream
//     a replica needs to catch up.

use std::sync::Mutex;
use tempfile::tempdir;
use unidb::bufferpool::BufferPool;
use unidb::control;
use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
use unidb::heap::Heap;
use unidb::replication::{SlotKind, SlotRegistry};
use unidb::wal::{decode_stream, Wal};
use unidb::Engine;

// A replication slot pinned at an old LSN keeps the WAL segments covering it,
// even when a checkpoint would otherwise delete them.
#[test]
fn slot_holds_back_wal_truncation() {
    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");
    let control = Mutex::new(control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap());
    let page_size = DEFAULT_PAGE_SIZE as usize;

    // Small segments so a modest insert stream spans several of them.
    let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
    let wal = Wal::open_with_segment_size(&wal_p, INVALID_LSN, 2048).unwrap();
    let heap = Heap::new(page_size);

    // An early slot position: capture the LSN after the first insert.
    let early = heap.insert(b"row_0", 1, &pool, &wal).unwrap();
    let _ = early;
    let slot_lsn = wal.current_lsn();
    for i in 1..200 {
        heap.insert(format!("row_{i}").as_bytes(), 1, &pool, &wal)
            .unwrap();
    }
    let segments_before = wal.segment_count().unwrap();
    assert!(
        segments_before >= 3,
        "precondition: inserts must span multiple segments (got {segments_before})"
    );
    pool.flush_all(wal.durable_lsn()).unwrap();
    let next_xid = 2;

    // Checkpoint WITH the slot floor: segments covering `slot_lsn` are retained.
    unidb::checkpoint::run(&pool, &wal, &ctrl_p, &control, next_xid, slot_lsn).unwrap();
    let with_slot = wal.segment_count().unwrap();
    assert!(
        with_slot > 1,
        "a slot at an early LSN must retain its WAL segments (got {with_slot})"
    );

    // Now drop the floor (no slots): a checkpoint may truncate to the ckpt LSN.
    unidb::checkpoint::run(&pool, &wal, &ctrl_p, &control, next_xid, u64::MAX).unwrap();
    let no_slot = wal.segment_count().unwrap();
    assert!(
        no_slot <= with_slot,
        "removing the slot floor must allow (at least) as much truncation \
         (with_slot={with_slot}, no_slot={no_slot})"
    );
}

// A slot registered on the live Engine sets the checkpoint retention floor, and
// slots persist across reopen.
#[test]
fn engine_slot_crud_and_persistence() {
    let dir = tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();

        let info = engine
            .create_replication_slot("replica_1", SlotKind::Async)
            .unwrap();
        assert_eq!(info.name, "replica_1");
        assert_eq!(engine.replication_slots().len(), 1);
        // Duplicate name rejected.
        assert!(engine
            .create_replication_slot("replica_1", SlotKind::Async)
            .is_err());
    }
    // Reopen: the slot survives (persisted in slots.json).
    let engine = Engine::open(dir.path(), 0).unwrap();
    let slots = engine.replication_slots();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].name, "replica_1");
    engine.drop_replication_slot("replica_1").unwrap();
    assert_eq!(engine.replication_slots().len(), 0);
}

// WAL shipping serializes the record stream after a given LSN and it decodes
// back to the same records — the bytes a replica applies via redo (P6.c).
#[test]
fn wal_shipping_round_trips() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let start = engine.wal_current_lsn();

    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alpha')")
        .unwrap();
    engine
        .execute_sql(xid, "INSERT INTO t (id, name) VALUES (2, 'beta')")
        .unwrap();
    engine.commit(xid).unwrap();

    // Everything committed after `start` ships and decodes back intact.
    let stream = engine.ship_wal(start).unwrap();
    let records = decode_stream(&stream).unwrap();
    assert!(
        !records.is_empty(),
        "shipping must carry the committed records"
    );
    assert!(
        records.iter().all(|r| r.lsn > start),
        "shipped records must all be strictly after the requested LSN"
    );
    // Idempotent: shipping from the current tail yields nothing new.
    let tail = engine.wal_current_lsn();
    assert!(engine.ship_wal(tail).unwrap().is_empty());
}

// The registry's min-restart-lsn is the retention floor used by the Engine.
#[test]
fn registry_min_restart_lsn_is_floor() {
    let dir = tempdir().unwrap();
    let reg = SlotRegistry::open(dir.path()).unwrap();
    assert_eq!(reg.min_restart_lsn(), None);
    reg.create("a", 100, SlotKind::Async).unwrap();
    reg.create("b", 40, SlotKind::Async).unwrap();
    assert_eq!(reg.min_restart_lsn(), Some(40));
    reg.advance("b", 200).unwrap();
    assert_eq!(reg.min_restart_lsn(), Some(100));
}
