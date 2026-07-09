// The single most important test in M3 (mirroring M2.d's
// `tests/vector_mvcc.rs`): `EdgeIndex` has no concept of transactions — it
// is updated synchronously and unconditionally inside `create_edge`, with
// no abort-time cleanup hook (a known, documented gap — see MEMORY.md).
// This proves that gap never leaks into a correctness bug: an edge whose
// creating transaction aborts must never surface in traversal, even though
// the index still references its (now permanently dead) `RowId` forever.
//
// Unlike M2's vector_mvcc test, no deterministic-poll-before-abort dance is
// needed here: `EdgeIndex` is synchronous (M3.a/M3.b — no background
// worker to race), so the moment `create_edge` returns, the index
// unconditionally already has the entry. There is no "did the worker catch
// up yet" question to answer at all.
//
// A third test, `aborted_edge_creation_never_surfaces_via_csr_path_once_
// ready`, briefly lived here during M7 (a CSR-preferring analogue of the
// test below) and was removed during M8 merge verification: this exact
// test, run in isolation, exposed a real bug in M7's `graph_candidates`
// (CSR was preferred once `IndexStatus::Ready`, but `Ready` only means
// "the initial backfill completed," not "every subsequent live edge write
// has been incorporated into a debounced rebuild" — a query immediately
// after `create_edge` could see `Ready` with a stale, pre-this-edge CSR
// structure). `edges_from`/Cypher now use `EdgeIndex` unconditionally
// again; see `src/graph/index.rs`'s module-level comment for the full
// writeup. `CsrIndex` itself remains correct and tested — the bug was
// specifically in preferring it for this path, not in the structure.

use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::Engine;

#[test]
fn aborted_edge_creation_never_surfaces_in_traversal() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let doomed_xid = engine.begin().unwrap();
    engine
        .create_edge(doomed_xid, 1, 999, "KNOWS", "{}")
        .unwrap();

    // Self-visibility: the inserting transaction sees its own uncommitted
    // edge immediately — ordinary MVCC, confirmed as a precondition before
    // aborting (proves the index really does have the entry at this point,
    // not just that traversal happens to find nothing either way).
    let self_view = engine.edges_from(doomed_xid, 1).unwrap();
    assert!(
        self_view.iter().any(|e| e.to_id == 999),
        "expected the inserting transaction to see its own uncommitted edge"
    );

    // Abort instead of commit. The heap row is undone (self-stamped xmax,
    // per M1's abort mechanism), but `EdgeIndex` has no concept of
    // transactions and no abort hook — its stale entry for this edge is
    // never retracted.
    engine.abort(doomed_xid).unwrap();

    // A fresh transaction sees only committed data. If `edges_from` didn't
    // re-check MVCC visibility on every index-sourced candidate, this
    // aborted edge would leak through despite never having committed.
    let fresh_xid = engine.begin().unwrap();
    let fresh_view = engine.edges_from(fresh_xid, 1).unwrap();
    assert!(
        !fresh_view.iter().any(|e| e.to_id == 999),
        "aborted edge leaked into traversal: {fresh_view:?}"
    );
}

#[test]
fn aborted_edge_creation_never_surfaces_in_cypher_query() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let doomed_xid = engine.begin().unwrap();
    engine
        .create_edge(doomed_xid, 1, 999, "KNOWS", "{}")
        .unwrap();
    engine.abort(doomed_xid).unwrap();

    let fresh_xid = engine.begin().unwrap();
    let results = engine
        .execute_cypher(fresh_xid, "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b")
        .unwrap();
    match &results[0] {
        ExecResult::Rows(rows) => assert!(
            rows.is_empty(),
            "aborted edge leaked into a Cypher query: {rows:?}"
        ),
        other => panic!("expected Rows, got {other:?}"),
    }
}
