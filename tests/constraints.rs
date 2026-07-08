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
        ExecResult::Rows(rows) => rows
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
        ExecResult::Rows(rows) => rows
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

// ── FOREIGN KEY (referenced-table existence only, M11 scope) ──────────────────

#[test]
fn foreign_key_requires_referenced_table_to_exist() {
    let (mut engine, _dir) = fresh();
    // The referenced table `users` does not exist yet — CREATE TABLE with a
    // forward reference is allowed; enforcement happens on write.
    run(
        &mut engine,
        "CREATE TABLE posts (id INT, author INT REFERENCES users(id))",
    )
    .unwrap();

    let err = run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 1)").unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { ref ref_table, .. } if ref_table == "users"),
        "expected ForeignKeyViolation referencing 'users', got {err:?}"
    );

    // Once `users` exists, the insert succeeds (referenced-table existence is
    // all M11 enforces — no referenced-row check).
    run(&mut engine, "CREATE TABLE users (id INT)").unwrap();
    run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 1)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM posts"), vec![1]);
}

#[test]
fn table_level_foreign_key_referenced_table_existence() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TABLE users (id INT)").unwrap();
    run(
        &mut engine,
        "CREATE TABLE posts (id INT, author INT, FOREIGN KEY (author) REFERENCES users(id))",
    )
    .unwrap();
    // Referenced table exists → insert is accepted.
    run(&mut engine, "INSERT INTO posts (id, author) VALUES (1, 42)").unwrap();
    assert_eq!(select_ints(&mut engine, "SELECT id FROM posts"), vec![1]);
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
