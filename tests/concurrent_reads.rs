//! 6b concurrency correctness: readers on a `ReadHandle` run in parallel with
//! the single writer and with each other, and must always observe
//! MVCC-consistent, non-torn committed data. If the shared mmap's `RwLock`
//! weren't excluding readers during a page write, a reader could observe a
//! half-written page and this test's exact-value assertions would fail.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::{Engine, RowId};

/// Committed (RowId -> exact value) pairs, published only after commit.
type Published = Arc<Mutex<Vec<(RowId, Vec<u8>)>>>;

#[test]
fn concurrent_readers_see_consistent_committed_rows_while_writer_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    let committed: Published = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    // Four reader threads, each with its own `ReadHandle` clone, hammering the
    // set of already-committed rows. A committed row is immutable here (INSERT
    // creates a fresh version), so every read of a published RowId must return
    // its exact committed bytes — never a torn page, never a wrong value.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let read = engine.read_handle();
        let committed = Arc::clone(&committed);
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let known: Vec<(RowId, Vec<u8>)> = committed.lock().unwrap().clone();
                for (rid, expected) in &known {
                    let got = read.get(*rid).expect("published row must be visible");
                    assert_eq!(&got, expected, "concurrent read returned torn/wrong bytes");
                    reads += 1;
                }
            }
            reads
        }));
    }

    // Writer: insert + commit many rows across many pages, publishing each
    // RowId only after its commit is recorded in shared txn state.
    for i in 0..1000u32 {
        let val = format!("committed-row-value-{i}").into_bytes();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, &val).unwrap();
        engine.commit(xid).unwrap();
        committed.lock().unwrap().push((rid, val));
    }

    stop.store(true, Ordering::Relaxed);
    let total_reads: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total_reads > 0, "readers should have run concurrently");

    // Every committed row is still readable with its exact value afterwards.
    let final_rows = committed.lock().unwrap().clone();
    assert_eq!(final_rows.len(), 1000);
    for (rid, expected) in final_rows {
        assert_eq!(engine.read_handle().get(rid).unwrap(), expected);
    }
}

/// 6b concurrent SQL SELECT: readers run `SELECT` on the `ReadHandle`, off the
/// writer thread, while the writer inserts. Each returned row's `name` must
/// pair with its `id` (`name-<id>`) — a torn page or an inconsistent catalog/
/// snapshot view would break that pairing.
#[test]
fn concurrent_sql_select_sees_consistent_rows_while_writer_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    {
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine.commit(xid).unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let read = engine.read_handle();
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut queries = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut results = read.execute_sql("SELECT id, name FROM t").unwrap();
                let ExecResult::Rows(rows) = results.remove(0) else {
                    panic!("expected Rows");
                };
                for row in rows {
                    let (Literal::Int(id), Literal::Text(name)) = (&row[0], &row[1]) else {
                        panic!("unexpected row shape: {row:?}");
                    };
                    assert_eq!(name, &format!("name-{id}"), "torn/inconsistent SELECT row");
                }
                queries += 1;
            }
            queries
        }));
    }

    for i in 0..500i64 {
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, name) VALUES ({i}, 'name-{i}')"),
            )
            .unwrap();
        engine.commit(xid).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let total: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0, "readers should have run concurrently");

    let mut results = engine
        .read_handle()
        .execute_sql("SELECT id, name FROM t")
        .unwrap();
    let ExecResult::Rows(rows) = results.remove(0) else {
        panic!("expected Rows");
    };
    assert_eq!(
        rows.len(),
        500,
        "all committed rows visible after the writer"
    );
}
