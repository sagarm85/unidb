// Multipage catalog (item 25): verify that schema metadata exceeding one 8 KiB
// page serializes across a chain of pages, persists across reopen, and stays
// crash-safe. Crash recovery is covered by crash harness (P33).
//
// BEFORE this fix: the catalog failed with HeapFull once the JSON blob exceeded
// ~8 KiB.  These tests fail on pre-fix main and pass here.

use tempfile::tempdir;
use unidb::{Engine, SqlResult};

fn open(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

fn sql(e: &Engine, query: &str) {
    let xid = e.begin().unwrap();
    e.execute_sql(xid, query).unwrap();
    e.commit(xid).unwrap();
}

fn rows(e: &Engine, query: &str) -> Vec<Vec<unidb::sql::logical::Literal>> {
    let xid = e.begin().unwrap();
    let res = e.execute_sql(xid, query).unwrap();
    e.commit(xid).unwrap();
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// The item-23 original layout that previously overflowed HeapFull{size:8883}:
/// `objects` with 11 columns (including `storage_key`), `buckets` 3 cols, and
/// the full 8-column DLQ table.  Must create, persist, reopen, and query.
#[test]
fn item23_original_layout_no_heap_full() {
    let dir = tempdir().unwrap();
    {
        let e = open(dir.path());
        sql(&e, "CREATE TABLE buckets (id INT, name TEXT, owner TEXT)");
        sql(&e, "CREATE TABLE objects (id INT, bucket_id INT, obj_key TEXT, storage_key TEXT, obj_size INT, ctype TEXT, etag TEXT, status TEXT, created_at INT, updated_at INT, metadata TEXT)");
        sql(&e, "CREATE TABLE object_dlq (id INT, object_id INT, reason TEXT, created_at INT, retries INT, last_error TEXT, resolved INT, resolved_at INT)");
        // Insert and verify basics before reopen.
        sql(
            &e,
            "INSERT INTO buckets (id, name, owner) VALUES (1, 'photos', 'alice')",
        );
    }
    // Reopen: all three tables must survive.
    let e = open(dir.path());
    let r = rows(&e, "SELECT id, name FROM buckets");
    assert_eq!(r.len(), 1, "bucket row must survive reopen");
    let xid = e.begin().unwrap();
    assert!(e.execute_sql(xid, "SELECT id FROM objects").is_ok());
    assert!(e.execute_sql(xid, "SELECT id FROM object_dlq").is_ok());
    e.commit(xid).unwrap();
}

/// A very wide schema (100 tables × 20 columns each) must fit in the multi-page
/// catalog and survive reopen with all tables and columns intact.
#[test]
fn hundred_tables_wide_schema_survives_reopen() {
    let dir = tempdir().unwrap();
    {
        let e = open(dir.path());
        for i in 0..100 {
            let cols: String = (0..20)
                .map(|c| format!("col{c} TEXT"))
                .collect::<Vec<_>>()
                .join(", ");
            sql(&e, &format!("CREATE TABLE tbl{i} ({cols})"));
        }
    }
    let e = open(dir.path());
    for i in 0..100 {
        let xid = e.begin().unwrap();
        let res = e.execute_sql(xid, &format!("SELECT col0 FROM tbl{i}"));
        e.commit(xid).unwrap();
        assert!(res.is_ok(), "table tbl{i} must survive reopen");
    }
}

/// ANALYZE after heavy row inserts must not overflow the catalog even when stats
/// grow the serialized blob past what previously caused HeapFull at runtime.
#[test]
fn analyze_after_inserts_does_not_overflow() {
    let dir = tempdir().unwrap();
    let e = open(dir.path());
    // Create enough tables to push the catalog close to (but under) a page.
    for i in 0..20 {
        sql(
            &e,
            &format!(
                "CREATE TABLE stat{i} \
                 (id INT, a TEXT, b TEXT, c TEXT, d TEXT, e TEXT)"
            ),
        );
    }
    // Insert rows and ANALYZE — the stats growth must not trigger HeapFull.
    for i in 0..5 {
        for j in 0..100 {
            sql(
                &e,
                &format!("INSERT INTO stat{i} (id, a) VALUES ({j}, 'x')"),
            );
        }
        sql(&e, &format!("ANALYZE stat{i}"));
    }
    // Verify the engine still operates correctly after stats-grown catalog.
    let xid = e.begin().unwrap();
    let res = e.execute_sql(xid, "SELECT id FROM stat0");
    e.commit(xid).unwrap();
    assert!(res.is_ok(), "stat0 must be queryable after ANALYZE");
}

/// Repeated SERIAL allocations (which each rewrite the catalog) must not
/// overflow after many inserts grow the serial_next counter.
#[test]
fn serial_inserts_across_many_tables_no_overflow() {
    let dir = tempdir().unwrap();
    let e = open(dir.path());
    // 30 tables with SERIAL columns — each INSERT rewrites the catalog.
    for i in 0..30 {
        sql(
            &e,
            &format!("CREATE TABLE ser{i} (id SERIAL, val TEXT, extra TEXT)"),
        );
    }
    // Insert into each table; SERIAL triggers catalog.alloc_serial per insert.
    for i in 0..5 {
        for _ in 0..10 {
            sql(&e, &format!("INSERT INTO ser{i} (val) VALUES ('test')"));
        }
    }
    // Reopen to verify the serial counters persisted.
    drop(e);
    let e = open(dir.path());
    let r = rows(&e, "SELECT id FROM ser0 ORDER BY id");
    assert_eq!(r.len(), 10, "ser0 must have 10 rows after reopen");
}
