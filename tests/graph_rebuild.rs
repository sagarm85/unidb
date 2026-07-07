// Edge-list index rebuild correctness (M3.d). Mirrors `tests/
// index_rebuild.rs`'s structure, but unlike M2's secondary indexes, no
// `wait_for_ready`-style polling is needed anywhere here: `EdgeIndex`
// (M3.a) is rebuilt synchronously, inline, before `Engine::open` returns
// — there is no background worker to race against.

use tempfile::tempdir;
use unidb::index_worker::IndexStatus;
use unidb::Engine;

fn wait_for_csr_ready(engine: &Engine) {
    let start = std::time::Instant::now();
    loop {
        if engine.index_status("__edges__", "from_id") == Some(IndexStatus::Ready) {
            return;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("CSR index never reached Ready within timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn engine_restart_rebuilds_edge_index_and_traversal_still_works() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.create_edge(xid, 5, 6, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }

    // Fresh process-equivalent open: `EdgeIndex` from the first `Engine`
    // is gone. Only the `__edges__` heap's committed rows survived —
    // rebuild-on-open must reconstruct the index from those alone, with
    // no transient "not ready yet" window to account for.
    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    let xid = engine2.begin().unwrap();
    let mut edges = engine2.edges_from(xid, 1).unwrap();
    edges.sort_by_key(|e| e.to_id);
    assert_eq!(edges.len(), 2);
    assert_eq!(edges[0].to_id, 2);
    assert_eq!(edges[1].to_id, 3);

    let hub5 = engine2.edges_from(xid, 5).unwrap();
    assert_eq!(hub5.len(), 1);
    assert_eq!(hub5[0].to_id, 6);
}

#[test]
fn engine_restart_reflects_deletes_from_before_close() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let row_id = engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        engine.delete_edge(xid2, row_id, 1).unwrap();
        engine.commit(xid2).unwrap();
        engine.flush().unwrap();
    }

    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    let xid = engine2.begin().unwrap();
    let edges = engine2.edges_from(xid, 1).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].to_id, 3);
}

/// M7's CSR analogue of `engine_restart_rebuilds_edge_index_and_traversal_
/// still_works` — unlike `EdgeIndex`, CSR rebuilds asynchronously
/// (`rebuild_csr_index` in `lib.rs`, run during `Engine::open` alongside
/// the other secondary-index backfills), so this explicitly waits for
/// `Ready` before asserting, to provably exercise the CSR-preferring path
/// rather than an ambient "might have used EdgeIndex" outcome.
#[test]
fn engine_restart_rebuilds_csr_index_and_traversal_still_works() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.create_edge(xid, 5, 6, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }

    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    wait_for_csr_ready(&engine2);

    let xid = engine2.begin().unwrap();
    let mut edges = engine2.edges_from(xid, 1).unwrap();
    edges.sort_by_key(|e| e.to_id);
    assert_eq!(edges.len(), 2);
    assert_eq!(edges[0].to_id, 2);
    assert_eq!(edges[1].to_id, 3);

    let hub5 = engine2.edges_from(xid, 5).unwrap();
    assert_eq!(hub5.len(), 1);
    assert_eq!(hub5[0].to_id, 6);
}

/// CSR's rebuild-on-open backfill scans `__edges__`'s *currently
/// committed* rows (via an ordinary MVCC snapshot) — a row deleted before
/// close is simply absent from that scan, no special CSR-side delete
/// handling needed (matching the existing "deletion is implicit" pattern
/// every other secondary index already has).
#[test]
fn engine_restart_csr_reflects_deletes_from_before_close() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let row_id = engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        engine.delete_edge(xid2, row_id, 1).unwrap();
        engine.commit(xid2).unwrap();
        engine.flush().unwrap();
    }

    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    wait_for_csr_ready(&engine2);

    let xid = engine2.begin().unwrap();
    let edges = engine2.edges_from(xid, 1).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].to_id, 3);
}

#[test]
fn engine_restart_rebuild_also_serves_cypher_queries() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }

    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    let xid = engine2.begin().unwrap();
    let results = engine2
        .execute_cypher(xid, "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b")
        .unwrap();
    match &results[0] {
        unidb::sql::executor::ExecResult::Rows(rows) => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows, got {other:?}"),
    }
}
