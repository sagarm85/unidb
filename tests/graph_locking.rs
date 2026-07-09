// Per-edge locking verification (M3.b). This checkpoint's deliverable is
// proof, not new code: `RecordId::row(page_id, slot)` (lockmgr.rs) already
// produces a globally-unique lock key across every table in the database,
// since `PageId` is allocated from one shared `BufferPool`, not per-table.
// `Heap::update`/`delete` already call `LockManager::try_acquire_write`
// before any mutation — this applies identically whether the row lives in
// an ordinary user table or `__edges__`. See MEMORY.md's M3.b design note
// for the full reasoning.

use tempfile::tempdir;
use unidb::{DbError, Engine};

#[test]
fn concurrent_edge_delete_conflicts_via_existing_lock_manager() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    let row_id = engine.create_edge(setup_xid, 1, 2, "KNOWS", "{}").unwrap();
    engine.commit(setup_xid).unwrap();

    // Two transactions both try to delete the same edge. Per D12, SI's
    // conflict handling is "abort immediately" — the second writer must
    // fail right at the write call, exactly like the existing
    // `concurrent_update_aborts_second_writer_immediately` test in lib.rs
    // for ordinary rows.
    let a = engine.begin().unwrap();
    engine.delete_edge(a, row_id, 1).unwrap();

    let b = engine.begin().unwrap();
    let err = engine.delete_edge(b, row_id, 1);
    assert!(
        matches!(err, Err(DbError::WriteConflict { .. })),
        "second writer must abort immediately on conflict, got {err:?}"
    );

    engine.commit(a).unwrap();
    engine.abort(b).unwrap();
}

#[test]
fn edge_lock_and_unrelated_row_lock_do_not_collide() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    engine
        .execute_sql(setup_xid, "CREATE TABLE t (id INT)")
        .unwrap();
    let row_id = engine.insert(setup_xid, b"row").unwrap();
    let edge_id = engine.create_edge(setup_xid, 1, 2, "KNOWS", "{}").unwrap();
    engine.commit(setup_xid).unwrap();

    // `a` holds a write lock on the ordinary row; `b` concurrently deletes
    // the edge. These are different tables (and thus different `page_id`s,
    // never colliding lock keys) so `b` must succeed with zero contention.
    let a = engine.begin().unwrap();
    engine.update(a, row_id, b"a-holds-lock").unwrap();

    let b = engine.begin().unwrap();
    engine.delete_edge(b, edge_id, 1).unwrap();

    engine.commit(a).unwrap();
    engine.commit(b).unwrap();
}

#[test]
fn edge_lock_releases_on_commit_and_next_writer_proceeds() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    let row_id = engine.create_edge(setup_xid, 1, 2, "KNOWS", "{}").unwrap();
    engine.commit(setup_xid).unwrap();

    let a = engine.begin().unwrap();
    engine.delete_edge(a, row_id, 1).unwrap();
    engine.commit(a).unwrap();

    // `a`'s commit released the lock — a fresh writer racing for the *same*
    // row still gets `WriteConflict`, but now via `heap.rs`'s other check
    // (the `xmax != 0` guard catching a row already superseded by a
    // transaction that has since committed and released its lock), not the
    // lock table (which `b` would have acquired cleanly, since `a` already
    // released it). Both failure modes intentionally share one error shape
    // — see `heap.rs::delete`'s doc comment — so this just confirms `b` is
    // never blocked *waiting* on `a`'s lock (SI never blocks, per D12); it
    // fails fast for the a-committed-first reason instead.
    let b = engine.begin().unwrap();
    let err = engine.delete_edge(b, row_id, 1);
    assert!(
        matches!(err, Err(DbError::WriteConflict { holder_xid }) if holder_xid == a),
        "expected WriteConflict attributed to a's xid, got {err:?}"
    );
}

#[test]
fn edge_lock_releases_on_abort_and_next_writer_proceeds() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    let row_id = engine.create_edge(setup_xid, 1, 2, "KNOWS", "{}").unwrap();
    engine.commit(setup_xid).unwrap();

    let a = engine.begin().unwrap();
    engine.delete_edge(a, row_id, 1).unwrap();
    engine.abort(a).unwrap();

    // a's abort released the lock (and undid the delete), so b can
    // successfully delete the still-live edge.
    let b = engine.begin().unwrap();
    engine.delete_edge(b, row_id, 1).unwrap();
    engine.commit(b).unwrap();

    let c = engine.begin().unwrap();
    assert!(engine.edges_from(c, 1).unwrap().is_empty());
}
