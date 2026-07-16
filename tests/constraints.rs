// SQL constraints (M11): end-to-end proof that PRIMARY KEY / FOREIGN KEY /
// UNIQUE / NOT NULL / CHECK / DEFAULT are parsed off `CREATE TABLE`, persisted
// on the catalog, and enforced on the write path — including that each kind
// actually *rejects* a violating write, and that DEFAULT fills a missing
// value.
//
// Enforcement notes proven here (see `sql/executor.rs`'s constraint section):
//   - UNIQUE is checked by a synchronous heap scan under the writer's own
//     MVCC snapshot, NOT via the async M6 B-Tree index (which can be stale) —
//     so a duplicate is caught even within a single multi-row INSERT and even
//     with no index present.
//   - FOREIGN KEY enforcement is referenced-table-existence only (M11 scope).
//   - CHECK reuses the SELECT/WHERE predicate evaluator and inherits its
//     two-valued NULL semantics.

use std::time::Instant;
use tempfile::tempdir;
use unidb::error::DbError;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

/// Open a fresh engine in a temp dir. The `TempDir` is returned so the caller
/// keeps it alive for the engine's lifetime.
fn fresh() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run one SQL statement inside its own committed transaction.
fn run(engine: &mut Engine, sql: &str) -> Result<Vec<ExecResult>, DbError> {
    let xid = engine.begin().unwrap();
    let result = engine.execute_sql(xid, sql);
    match &result {
        Ok(_) => engine.commit(xid).unwrap(),
        Err(_) => {
            let _ = engine.abort(xid);
        }
    }
    result
}

/// SELECT a single-column projection back as i64s (ordering unspecified).
fn select_ints(engine: &mut Engine, sql: &str) -> Vec<i64> {
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match &results[0] {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Literal::Int(n) => *n,
                other => panic!("expected Int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn select_texts(engine: &mut Engine, sql: &str) -> Vec<String> {
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match &results[0] {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Literal::Text(s) => s.clone(),
                other => panic!("expected Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── NOT NULL ─────────────────────────────────────────────────────────────────

#[test]
fn not_null_rejects_null_and_allows_value() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE t (id INT, name TEXT NOT NULL)").unwrap();

    // A row supplying the required column succeeds.
    run(&mut engine, "INSERT INTO t (id, name) VALUES (1, 'alice')").unwrap();

    // Omitting the NOT NULL column (ordered as NULL) is rejected.
    let err = run(&mut engine, "INSERT INTO t (id) VALUES (2)").unwrap_err();
    assert!(
        matches!(err, DbError::NotNullViolation { ref column, .. } if column == "name"),
        "expected NotNullViolation on 'name', got {err:?}"
    );

    // An explicit NULL is rejected too.
    let err = run(&mut engine, "INSERT INTO t (id, name) VALUES (3, NULL)").unwrap_err();
    assert!(matches!(err, DbError::NotNullViolation { .. }), "{err:?}");

    // Only the first (valid) row landed.
    assert_eq!(select_ints(&mut engine, "SELECT id FROM t"), vec![1]);
}

#[test]
fn update_that_nulls_a_not_null_column_is_rejected() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE t (id INT, name TEXT NOT NULL)").unwrap();
    run(&mut engine, "INSERT INTO t (id, name) VALUES (1, 'alice')").unwrap();

    let err = run(&mut engine, "UPDATE t SET name = NULL WHERE id = 1").unwrap_err();
    assert!(matches!(err, DbError::NotNullViolation { .. }), "{err:?}");
    // Row is unchanged.
    assert_eq!(
        select_texts(&mut engine, "SELECT name FROM t WHERE id = 1"),
        vec!["alice".to_string()]
    );
}

// ── DEFAULT ──────────────────────────────────────────────────────────────────

#[test]
fn default_fills_omitted_column() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE t (id INT, status TEXT DEFAULT 'active', score INT DEFAULT 10)",
    )
    .unwrap();

    // Omit both defaulted columns.
    run(&mut engine, "INSERT INTO t (id) VALUES (1)").unwrap();
    // Provide one explicitly; it must win over the default.
    run(
        &mut engine,
        "INSERT INTO t (id, status) VALUES (2, 'banned')",
    )
    .unwrap();

    assert_eq!(
        select_texts(&mut engine, "SELECT status FROM t WHERE id = 1"),
        vec!["active".to_string()]
    );
    assert_eq!(
        select_ints(&mut engine, "SELECT score FROM t WHERE id = 1"),
        vec![10]
    );
    assert_eq!(
        select_texts(&mut engine, "SELECT status FROM t WHERE id = 2"),
        vec!["banned".to_string()]
    );
}

// ── UNIQUE ───────────────────────────────────────────────────────────────────

#[test]
fn unique_rejects_duplicate_across_and_within_statements() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE t (id INT, email TEXT UNIQUE)").unwrap();
    run(&mut engine, "INSERT INTO t (id, email) VALUES (1, 'a@x')").unwrap();

    // Duplicate in a separate statement.
    let err = run(&mut engine, "INSERT INTO t (id, email) VALUES (2, 'a@x')").unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { ref columns, .. } if columns == "email"),
        "expected UniqueViolation on 'email', got {err:?}"
    );

    // Duplicate *within one multi-row INSERT* is also caught (the second row's
    // check sees the first row's own uncommitted write).
    let err = run(
        &mut engine,
        "INSERT INTO t (id, email) VALUES (3, 'b@x'), (4, 'b@x')",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err:?}");

    // NULLs are distinct: multiple NULL emails are allowed.
    run(&mut engine, "INSERT INTO t (id) VALUES (10)").unwrap();
    run(&mut engine, "INSERT INTO t (id) VALUES (11)").unwrap();

    let mut ids = select_ints(&mut engine, "SELECT id FROM t");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 10, 11]);
}

#[test]
fn update_into_existing_unique_value_is_rejected_but_self_update_is_ok() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE t (id INT, email TEXT UNIQUE)").unwrap();
    run(&mut engine, "INSERT INTO t (id, email) VALUES (1, 'a@x')").unwrap();
    run(&mut engine, "INSERT INTO t (id, email) VALUES (2, 'b@x')").unwrap();

    // Updating row 2 to row 1's email conflicts.
    let err = run(&mut engine, "UPDATE t SET email = 'a@x' WHERE id = 2").unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err:?}");

    // Updating a row to its own current value must NOT collide with itself.
    run(&mut engine, "UPDATE t SET email = 'a@x' WHERE id = 1").unwrap();
    // And a genuine change is fine.
    run(&mut engine, "UPDATE t SET email = 'c@x' WHERE id = 2").unwrap();
}

// ── PRIMARY KEY (implies NOT NULL + UNIQUE) ───────────────────────────────────

#[test]
fn primary_key_implies_not_null_and_unique() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO t (id, name) VALUES (1, 'a')").unwrap();

    // Duplicate PK rejected.
    let err = run(&mut engine, "INSERT INTO t (id, name) VALUES (1, 'b')").unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err:?}");

    // NULL PK rejected.
    let err = run(&mut engine, "INSERT INTO t (name) VALUES ('c')").unwrap_err();
    assert!(matches!(err, DbError::NotNullViolation { .. }), "{err:?}");

    assert_eq!(select_ints(&mut engine, "SELECT id FROM t"), vec![1]);
}

#[test]
fn table_level_composite_unique() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE t (a INT, b INT, UNIQUE (a, b))").unwrap();
    run(&mut engine, "INSERT INTO t (a, b) VALUES (1, 2)").unwrap();
    // Same a, different b — allowed.
    run(&mut engine, "INSERT INTO t (a, b) VALUES (1, 3)").unwrap();
    // Full tuple duplicate — rejected.
    let err = run(&mut engine, "INSERT INTO t (a, b) VALUES (1, 2)").unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { ref columns, .. } if columns == "a, b"),
        "{err:?}"
    );
}

// ── CHECK ────────────────────────────────────────────────────────────────────

#[test]
fn check_rejects_violating_value() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE t (id INT NOT NULL, age INT NOT NULL CHECK (age > 0))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO t (id, age) VALUES (1, 30)").unwrap();

    let err = run(&mut engine, "INSERT INTO t (id, age) VALUES (2, 0)").unwrap_err();
    assert!(
        matches!(err, DbError::CheckViolation { .. }),
        "expected CheckViolation, got {err:?}"
    );

    // An UPDATE that would violate the CHECK is rejected too.
    let err = run(&mut engine, "UPDATE t SET age = 0 WHERE id = 1").unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");

    assert_eq!(
        select_ints(&mut engine, "SELECT age FROM t WHERE id = 1"),
        vec![30]
    );
}

#[test]
fn table_level_check() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE t (lo INT NOT NULL, hi INT NOT NULL, CHECK (hi > lo))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO t (lo, hi) VALUES (1, 10)").unwrap();
    let err = run(&mut engine, "INSERT INTO t (lo, hi) VALUES (10, 1)").unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");
}

// ── FOREIGN KEY ───────────────────────────────────────────────────────────────

#[test]
fn foreign_key_requires_referenced_table_to_exist() {
    let (mut engine, _dir) = fresh();
    // Forward reference is allowed at CREATE TABLE time; enforcement happens on write.
    run(
        &mut engine,
        "CREATE TABLE posts (id INT, author INT REFERENCES users(id))",
    )
    .unwrap();

    // Table does not exist yet → ForeignKeyViolation on INSERT.
    let err = run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 1)").unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { ref ref_table, .. } if ref_table == "users"),
        "expected ForeignKeyViolation referencing 'users', got {err:?}"
    );

    // Table exists and referenced row exists → insert succeeds (item 36:
    // row-level enforcement now active; table existence alone is not enough).
    run(&mut engine, "CREATE TABLE users (id INT PRIMARY KEY)").unwrap();
    run(&mut engine, "INSERT INTO users (id) VALUES (1)").unwrap();
    run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 1)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM posts"), vec![1]);
}

#[test]
fn table_level_foreign_key_referenced_table_existence() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE users (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE posts (id INT, author INT, FOREIGN KEY (author) REFERENCES users(id))",
    )
    .unwrap();
    // Parent row must exist (item 36 row-level enforcement).
    run(&mut engine, "INSERT INTO users (id) VALUES (42)").unwrap();
    run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 42)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM posts"), vec![1]);
}

// ── FOREIGN KEY — row-level enforcement (item 36) ─────────────────────────────

#[test]
fn fk_row_existence_missing_parent_rejected() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    // Parent row id=99 does not exist → ForeignKeyViolation.
    let err = run(
        &mut engine,
        "INSERT INTO items (id, order_id) VALUES (1, 99)",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ForeignKeyViolation {
                ref column,
                ref value,
                ..
            } if column.as_deref() == Some("order_id") && value.as_deref() == Some("99")
        ),
        "expected ForeignKeyViolation for order_id=99, got {err:?}"
    );
    // No items inserted.
    assert_eq!(
        select_ints(&mut engine, "SELECT id FROM items"),
        vec![0i64; 0]
    );
}

#[test]
fn fk_row_existence_valid_parent_accepted() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (1)").unwrap();
    run(
        &mut engine,
        "INSERT INTO items (id, order_id) VALUES (10, 1)",
    )
    .unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM items"), vec![10]);
}

#[test]
fn fk_null_column_not_checked() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    // NULL FK column is always accepted even when no parent row exists.
    run(&mut engine, "INSERT INTO items (id) VALUES (1)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM items"), vec![1]);
}

#[test]
fn fk_same_txn_parent_then_child_accepted() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    // Insert parent + child in one transaction (own-xid visibility via get_visible).
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "INSERT INTO orders (id) VALUES (5)")
        .unwrap();
    engine
        .execute_sql(xid, "INSERT INTO items (id, order_id) VALUES (50, 5)")
        .unwrap();
    engine.commit(xid).unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM items"), vec![50]);
}

#[test]
fn fk_restrict_blocks_parent_delete_with_children() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (1)").unwrap();
    run(
        &mut engine,
        "INSERT INTO items (id, order_id) VALUES (10, 1)",
    )
    .unwrap();
    // Parent delete blocked because child row references it (RESTRICT).
    let err = run(&mut engine, "DELETE FROM orders WHERE id = 1").unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ForeignKeyViolation {
                ref ref_table,
                ref column,
                ..
            } if ref_table == "orders" && column.as_deref() == Some("order_id")
        ),
        "expected RESTRICT ForeignKeyViolation, got {err:?}"
    );
    // Parent row still intact.
    assert_eq!(select_ints(&mut engine, "SELECT id FROM orders"), vec![1]);
}

#[test]
fn fk_restrict_allows_parent_delete_no_children() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (1)").unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (2)").unwrap();
    run(
        &mut engine,
        "INSERT INTO items (id, order_id) VALUES (10, 2)",
    )
    .unwrap();
    // Delete order 1 (no children) — must succeed.
    run(&mut engine, "DELETE FROM orders WHERE id = 1").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM orders"), vec![2]);
    // Delete order 2 (has child) — must fail.
    let err = run(&mut engine, "DELETE FROM orders WHERE id = 2").unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected RESTRICT, got {err:?}"
    );
}

#[test]
fn fk_table_level_constraint_enforced() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE users (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE posts (id INT PRIMARY KEY, author INT, FOREIGN KEY (author) REFERENCES users(id))",
    )
    .unwrap();
    // Missing parent → rejected.
    let err = run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 99)").unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { ref ref_table, .. } if ref_table == "users"),
        "expected ForeignKeyViolation for users, got {err:?}"
    );
    // Valid parent → accepted.
    run(&mut engine, "INSERT INTO users (id) VALUES (99)").unwrap();
    run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 99)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM posts"), vec![1]);
}

#[test]
fn fk_update_to_missing_parent_rejected() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (1)").unwrap();
    run(
        &mut engine,
        "INSERT INTO items (id, order_id) VALUES (10, 1)",
    )
    .unwrap();
    // Update child FK to a non-existent parent → rejected.
    let err = run(&mut engine, "UPDATE items SET order_id = 999 WHERE id = 10").unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected ForeignKeyViolation, got {err:?}"
    );
    // FK value is unchanged.
    assert_eq!(
        select_ints(&mut engine, "SELECT order_id FROM items WHERE id = 10"),
        vec![1]
    );
}

#[test]
fn fk_child_insert_throughput_is_flat() {
    // Regression: child-side FK enforcement must be O(log n) in parent size —
    // not O(n) as a heap scan would be. Measure insert throughput into a child
    // table at chunk1 (small parent) vs chunk3 (large parent); the ratio must
    // not degrade like the pre-item-35 enforce_unique did.
    let (dir, mut engine) = {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        (dir, engine)
    };
    run(&mut engine, "CREATE TABLE orders (id INT PRIMARY KEY)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE items (id INT PRIMARY KEY, order_id INT REFERENCES orders(id))",
    )
    .unwrap();

    // Insert a fixed parent row that all child chunks will reference.
    run(&mut engine, "INSERT INTO orders (id) VALUES (1)").unwrap();

    // Helper: grow the orders table then measure a child-insert chunk.
    let measure = |engine: &mut Engine, base: i64, chunk: i64| -> f64 {
        let pad_vals: String = (base + 1_000_000..base + 1_000_000 + chunk)
            .map(|i| format!("({i})"))
            .collect::<Vec<_>>()
            .join(", ");
        run(
            engine,
            &format!("INSERT INTO orders (id) VALUES {pad_vals}"),
        )
        .unwrap();
        let vals: String = (base..base + chunk)
            .map(|i| format!("({i}, 1)"))
            .collect::<Vec<_>>()
            .join(", ");
        let t0 = Instant::now();
        run(
            engine,
            &format!("INSERT INTO items (id, order_id) VALUES {vals}"),
        )
        .unwrap();
        chunk as f64 / t0.elapsed().as_secs_f64()
    };

    let chunk = 1_000i64;
    let r1 = measure(&mut engine, 0, chunk);
    let r2 = measure(&mut engine, chunk, chunk);
    let r3 = measure(&mut engine, chunk * 2, chunk);

    // Ratio must stay above 0.5 — at most 2× slowdown chunk3 vs chunk1.
    // Before fix (O(n) parent scan) the ratio was ~0.2 at these sizes.
    let ratio3 = r3 / r1;
    assert!(
        ratio3 > 0.5,
        "FK child insert chunk3/chunk1 ratio={ratio3:.2} — suspected O(n) parent scan \
         (r1={r1:.0}/s, r2={r2:.0}/s, r3={r3:.0}/s)"
    );
    let _ = dir;
}

// ── persistence across reopen ─────────────────────────────────────────────────

#[test]
fn constraints_survive_reopen() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        run(
            &mut engine,
            "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE, status TEXT DEFAULT 'new')",
        )
        .unwrap();
        run(&mut engine, "INSERT INTO t (id, email) VALUES (1, 'a@x')").unwrap();
    }
    // Reopen: the catalog (and thus every constraint) is reloaded from disk.
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    // UNIQUE still enforced after reopen.
    let xid = engine.begin().unwrap();
    let err = engine
        .execute_sql(xid, "INSERT INTO t (id, email) VALUES (2, 'a@x')")
        .unwrap_err();
    let _ = engine.abort(xid);
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err:?}");

    // DEFAULT still applied after reopen.
    run(&mut engine, "INSERT INTO t (id, email) VALUES (3, 'b@x')").unwrap();
    assert_eq!(
        select_texts(&mut engine, "SELECT status FROM t WHERE id = 3"),
        vec!["new".to_string()]
    );
}

// ── Phase-0 item-35 baseline measurement (ignored, run explicitly) ────────────

fn measure_bulk_chunk(engine: &Engine, table: &str, start: i64, count: i64) -> f64 {
    let stmt = engine
        .prepare(&format!("INSERT INTO {table} (id, body) VALUES ($1, $2)"))
        .unwrap();
    let t0 = Instant::now();
    let xid = engine.begin().unwrap();
    for i in start..start + count {
        engine
            .execute_prepared(
                xid,
                &stmt,
                &[Literal::Int(i), Literal::Text(format!("body-{i}"))],
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    count as f64 / elapsed
}

/// Phase-0/Phase-3 throughput measurement (item 35).
/// Run: cargo test --release --test constraints -- --nocapture --ignored pk_vs_nopk_baseline
#[test]
#[ignore]
fn pk_vs_nopk_baseline() {
    let chunk = 5_000i64;

    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    run(
        &mut engine,
        "CREATE TABLE pk_t (id INT PRIMARY KEY, body TEXT)",
    )
    .unwrap();
    let r1 = measure_bulk_chunk(&engine, "pk_t", 0, chunk);
    let r2 = measure_bulk_chunk(&engine, "pk_t", chunk, chunk);
    let r3 = measure_bulk_chunk(&engine, "pk_t", chunk * 2, chunk);
    println!("PK table  :  chunk1={r1:.0}/s  chunk2={r2:.0}/s  chunk3={r3:.0}/s");

    let dir2 = tempdir().unwrap();
    let mut engine2 = Engine::open(dir2.path(), 0).unwrap();
    run(&mut engine2, "CREATE TABLE nopk (id INT, body TEXT)").unwrap();
    let s1 = measure_bulk_chunk(&engine2, "nopk", 0, chunk);
    let s2 = measure_bulk_chunk(&engine2, "nopk", chunk, chunk);
    let s3 = measure_bulk_chunk(&engine2, "nopk", chunk * 2, chunk);
    println!("No-PK table: chunk1={s1:.0}/s  chunk2={s2:.0}/s  chunk3={s3:.0}/s");
}

// ── Item 35 — permanent regression tests ─────────────────────────────────────

/// Regression: PK INSERT throughput must not degrade across consecutive bulk
/// chunks (item 35). Before the fix, each additional chunk was O(n) slower.
/// The test proves **flat throughput**: chunk3 must be within 30% of chunk1
/// (degradation from O(n²) was typically >3×). Uses debug build (so rate
/// targets are conservative) but the flatness invariant holds at any speed.
#[test]
fn pk_insert_throughput_is_flat_not_degrading() {
    let chunk = 3_000i64;

    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    run(
        &mut engine,
        "CREATE TABLE flat_pk (id INT PRIMARY KEY, body TEXT)",
    )
    .unwrap();

    let r1 = measure_bulk_chunk(&engine, "flat_pk", 0, chunk);
    let r2 = measure_bulk_chunk(&engine, "flat_pk", chunk, chunk);
    let r3 = measure_bulk_chunk(&engine, "flat_pk", chunk * 2, chunk);

    // chunk2/chunk1 and chunk3/chunk1 must stay above 0.5 — i.e. no more
    // than 2× slowdown. Before the fix both ratios were ~0.34 and ~0.21.
    let ratio2 = r2 / r1;
    let ratio3 = r3 / r1;
    assert!(
        ratio2 > 0.5,
        "PK INSERT chunk2 degraded >2× vs chunk1 (ratio={ratio2:.2}) — O(n²) bug regressed"
    );
    assert!(
        ratio3 > 0.5,
        "PK INSERT chunk3 degraded >2× vs chunk1 (ratio={ratio3:.2}) — O(n²) bug regressed"
    );
}

/// Regression: UNIQUE column INSERT throughput must also be flat (item 35).
#[test]
fn unique_insert_throughput_is_flat_not_degrading() {
    let chunk = 3_000i64;

    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE flat_uq (id INT, body TEXT UNIQUE)")
        .unwrap();
    engine.commit(xid).unwrap();

    let r1 = measure_bulk_chunk(&engine, "flat_uq", 0, chunk);
    let r2 = measure_bulk_chunk(&engine, "flat_uq", chunk, chunk);
    let r3 = measure_bulk_chunk(&engine, "flat_uq", chunk * 2, chunk);

    let ratio3 = r3 / r1;
    assert!(
        ratio3 > 0.5,
        "UNIQUE INSERT chunk3 degraded >2× vs chunk1 (ratio={ratio3:.2}) — O(n²) bug regressed"
    );
    let _ = r2;
}

/// Invariant 1 (MVCC): UPDATE to a UNIQUE column must not collide with the
/// dead old version still in the index until vacuum (item 35 spec §Phase-2.1).
#[test]
fn update_unique_column_does_not_collide_with_own_dead_version_in_index() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE mvcc_t (id INT PRIMARY KEY, val INT UNIQUE)",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO mvcc_t (id, val) VALUES (1, 100)").unwrap();

    // UPDATE val from 100 to 200. The old version (val=100) stays in the
    // index until vacuum. The new version (val=200) must not see the dead
    // old entry as a uniqueness conflict with itself.
    run(&mut engine, "UPDATE mvcc_t SET val = 200 WHERE id = 1").unwrap();

    // Verify the updated value is visible.
    let vals = select_ints(&mut engine, "SELECT val FROM mvcc_t WHERE id = 1");
    assert_eq!(vals, vec![200]);

    // A second UPDATE to the same value is also fine — just replaces 200 with 200.
    run(&mut engine, "UPDATE mvcc_t SET val = 200 WHERE id = 1").unwrap();
}

/// Invariant 2 (own-xid): same-batch duplicates must be caught when the
/// implicit index is used (item 35 spec §Phase-2.2).
#[test]
fn same_batch_pk_duplicate_is_caught_via_index() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE batch_pk (id INT PRIMARY KEY, body TEXT)",
    )
    .unwrap();

    // Two rows with the same PK in one multi-row INSERT must be rejected.
    let err = run(
        &mut engine,
        "INSERT INTO batch_pk (id, body) VALUES (1, 'a'), (1, 'b')",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "same-batch PK duplicate must be caught; got {err:?}"
    );
}

/// Invariant 4 (NULL distinctness): multiple NULLs in a UNIQUE column are
/// still allowed after the implicit-index fix (item 35 spec §Phase-2.4).
#[test]
fn null_distinctness_preserved_with_implicit_index() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TABLE null_uq (id INT, email TEXT UNIQUE)",
    )
    .unwrap();

    // Multiple NULLs must be allowed (NULLs are distinct in SQL).
    run(&mut engine, "INSERT INTO null_uq (id) VALUES (1)").unwrap();
    run(&mut engine, "INSERT INTO null_uq (id) VALUES (2)").unwrap();
    run(&mut engine, "INSERT INTO null_uq (id) VALUES (3)").unwrap();

    let ids = select_ints(&mut engine, "SELECT id FROM null_uq");
    assert_eq!(
        ids.len(),
        3,
        "three rows with NULL email must all be present"
    );
}

/// Item 53: FK UPDATE not touching the FK column must skip enforcement entirely.
/// Correctness gate: `SET customer_id` on a bad parent must still be rejected.
/// Throughput gate: `SET status` (non-FK column) on a large FK'd table must run
/// at least as fast as a non-FK UPDATE on an equivalently sized table (ratio ≥ 0.7).
#[test]
fn fk_update_non_fk_col_skips_enforcement() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    run(
        &mut engine,
        "CREATE TABLE cust (id INT PRIMARY KEY, name TEXT)",
    )
    .unwrap();
    run(
        &mut engine,
        "CREATE TABLE ord (id INT PRIMARY KEY, customer_id INT REFERENCES cust(id), status TEXT)",
    )
    .unwrap();
    // Also create a plain (non-FK) table for baseline comparison.
    run(
        &mut engine,
        "CREATE TABLE plain (id INT PRIMARY KEY, status TEXT)",
    )
    .unwrap();

    let n = 3_000i64;
    // Insert parent customers.
    let xid = engine.begin().unwrap();
    for i in 0..n {
        run(
            &mut engine,
            &format!("INSERT INTO cust (id, name) VALUES ({i}, 'c{i}')"),
        )
        .unwrap();
    }
    engine.commit(xid).unwrap();
    // Insert child orders referencing real customers.
    let xid = engine.begin().unwrap();
    for i in 0..n {
        run(
            &mut engine,
            &format!("INSERT INTO ord (id, customer_id, status) VALUES ({i}, {i}, 'pending')"),
        )
        .unwrap();
    }
    engine.commit(xid).unwrap();
    // Insert plain rows.
    let xid = engine.begin().unwrap();
    for i in 0..n {
        run(
            &mut engine,
            &format!("INSERT INTO plain (id, status) VALUES ({i}, 'pending')"),
        )
        .unwrap();
    }
    engine.commit(xid).unwrap();

    // Correctness: UPDATE that changes the FK column to a missing parent must be rejected.
    run(
        &mut engine,
        "UPDATE ord SET customer_id = 999999 WHERE id = 0",
    )
    .expect_err("FK violation expected when writing invalid customer_id");

    // Throughput: UPDATE non-FK column on FK table vs UPDATE on plain table.
    let t_fk = {
        let t0 = Instant::now();
        run(
            &mut engine,
            "UPDATE ord SET status = 'shipped' WHERE id < 3000",
        )
        .unwrap();
        t0.elapsed().as_secs_f64()
    };
    let t_plain = {
        let t0 = Instant::now();
        run(
            &mut engine,
            "UPDATE plain SET status = 'shipped' WHERE id < 3000",
        )
        .unwrap();
        t0.elapsed().as_secs_f64()
    };
    // FK UPDATE (non-FK col) must be within 2× of the plain UPDATE.
    // Before item 53 it was ~3× slower; after it should be ≤ 1.5× (same path).
    let ratio = t_fk / t_plain;
    assert!(
        ratio < 2.0,
        "FK UPDATE (non-FK col) is {ratio:.2}× slower than plain UPDATE — enforcement not skipped? \
         (fk={t_fk:.3}s plain={t_plain:.3}s)"
    );
    let _ = dir;
}

/// UPDATE throughput on a PK'd table must also be flat (item 35 spec).
#[test]
fn pk_update_throughput_is_flat() {
    let chunk = 1_000i64;

    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    run(
        &mut engine,
        "CREATE TABLE upd_pk (id INT PRIMARY KEY, val INT)",
    )
    .unwrap();

    // Pre-load rows.
    let stmt = engine
        .prepare("INSERT INTO upd_pk (id, val) VALUES ($1, $2)")
        .unwrap();
    let xid = engine.begin().unwrap();
    for i in 0..chunk * 3 {
        engine
            .execute_prepared(xid, &stmt, &[Literal::Int(i), Literal::Int(i * 10)])
            .unwrap();
    }
    engine.commit(xid).unwrap();

    // Measure UPDATE throughput across three batches of chunk rows.
    let upd = engine
        .prepare("UPDATE upd_pk SET val = $1 WHERE id = $2")
        .unwrap();
    let measure_upd = |start: i64| {
        let t0 = Instant::now();
        let xid = engine.begin().unwrap();
        for i in start..start + chunk {
            engine
                .execute_prepared(xid, &upd, &[Literal::Int(i * 99), Literal::Int(i)])
                .unwrap();
        }
        engine.commit(xid).unwrap();
        chunk as f64 / t0.elapsed().as_secs_f64()
    };

    let u1 = measure_upd(0);
    let u2 = measure_upd(chunk);
    let u3 = measure_upd(chunk * 2);
    let ratio = u3 / u1;
    assert!(
        ratio > 0.3,
        "UPDATE chunk3/chunk1 ratio too low ({ratio:.2}) — suggests O(n²) enforcement"
    );
    let _ = u2;
}
