// Item 148: enums + domains (named types v1).
//
// `CREATE TYPE ... AS ENUM` / `CREATE DOMAIN` are catalog-registered named
// types that desugar at `CREATE TABLE` time into (base `ColumnType` +
// synthesized CHECK + `ColumnDef.type_name`) — see `docs/backlog/
// 148_enums_domains.md` for the locked design and `sql/executor.rs::
// resolve_named_type_column` for the desugar itself. Enforcement is entirely
// the pre-existing CHECK-constraint machinery; this suite exists to prove the
// desugar produces the right CHECK, not to test CHECK itself (see
// `tests/constraints.rs` for that).
//
// Engine-level SQL test, not feature-gated — mirrors `tests/constraints.rs`'s
// harness pattern exactly (no server needed for SQL-only DDL/DML coverage).

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

/// Run one SQL statement inside its own committed transaction. Mirrors
/// `tests/constraints.rs::run` exactly.
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

fn select_texts(engine: &mut Engine, sql: &str) -> Vec<String> {
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match &results[0] {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Literal::Text(s) => s.clone(),
                Literal::Null => "<NULL>".to_string(),
                other => panic!("expected Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

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

// ── 1. Enum end-to-end ──────────────────────────────────────────────────────

#[test]
fn enum_end_to_end_insert_and_update() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped')",
    )
    .unwrap();
    run(
        &mut engine,
        "CREATE TABLE orders (id INT, status order_status)",
    )
    .unwrap();

    // Valid values insert fine.
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (1, 'pending')",
    )
    .unwrap();
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (2, 'paid')",
    )
    .unwrap();

    // An invalid value is rejected on INSERT with a CHECK-shaped error.
    let err = run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (3, 'bogus')",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::CheckViolation { .. }),
        "expected CheckViolation, got {err:?}"
    );

    // ... and on UPDATE.
    let err = run(
        &mut engine,
        "UPDATE orders SET status = 'cancelled' WHERE id = 1",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");

    // Only the valid rows/values landed.
    let mut statuses = select_texts(&mut engine, "SELECT status FROM orders");
    statuses.sort();
    assert_eq!(statuses, vec!["paid".to_string(), "pending".to_string()]);
}

// ── 2. Domain with CHECK ────────────────────────────────────────────────────

#[test]
fn domain_with_check_rejects_invalid_and_accepts_valid() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE DOMAIN email AS TEXT CHECK (VALUE LIKE '%@%')",
    )
    .unwrap();
    run(&mut engine, "CREATE TABLE users (id INT, contact email)").unwrap();

    run(
        &mut engine,
        "INSERT INTO users (id, contact) VALUES (1, 'alice@example.com')",
    )
    .unwrap();

    let err = run(
        &mut engine,
        "INSERT INTO users (id, contact) VALUES (2, 'not-an-email')",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");

    // UPDATE is checked too.
    let err = run(
        &mut engine,
        "UPDATE users SET contact = 'nope' WHERE id = 1",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");

    assert_eq!(
        select_texts(&mut engine, "SELECT contact FROM users WHERE id = 1"),
        vec!["alice@example.com".to_string()]
    );
}

#[test]
fn domain_without_check_is_plain_base_type_alias() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE DOMAIN us_text AS TEXT").unwrap();
    run(&mut engine, "CREATE TABLE t (id INT, note us_text)").unwrap();

    // No CHECK at all — any TEXT value is accepted, including NULL.
    run(
        &mut engine,
        "INSERT INTO t (id, note) VALUES (1, 'anything at all')",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO t (id, note) VALUES (2, NULL)").unwrap();

    let mut notes = select_texts(&mut engine, "SELECT note FROM t");
    notes.sort();
    assert_eq!(
        notes,
        vec!["<NULL>".to_string(), "anything at all".to_string()]
    );
}

// ── 3. Persistence across reopen ────────────────────────────────────────────

#[test]
fn named_type_enforcement_and_introspection_survive_reopen() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        run(
            &mut engine,
            "CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped')",
        )
        .unwrap();
        run(
            &mut engine,
            "CREATE TABLE orders (id INT, status order_status)",
        )
        .unwrap();
        run(
            &mut engine,
            "INSERT INTO orders (id, status) VALUES (1, 'pending')",
        )
        .unwrap();
    } // engine dropped, files closed

    let mut engine2 = Engine::open(dir.path(), 0).unwrap();

    // Enforcement still applies after reopen.
    let err = run(
        &mut engine2,
        "INSERT INTO orders (id, status) VALUES (2, 'bogus')",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "{err:?}");

    // The pre-reopen row is still there.
    assert_eq!(
        select_texts(&mut engine2, "SELECT status FROM orders"),
        vec!["pending".to_string()]
    );

    // `type_name` introspection survives (Rust-level; item 148's spec
    // explicitly defers information_schema/SQL-level exposure).
    let table = engine2
        .table_defs()
        .into_iter()
        .find(|t| t.name == "orders")
        .expect("orders table survives reopen");
    let status_col = table
        .columns
        .iter()
        .find(|c| c.name == "status")
        .expect("status column survives reopen");
    assert_eq!(status_col.type_name.as_deref(), Some("order_status"));
    assert_eq!(status_col.ty, unidb::catalog::ColumnType::Text);
}

// ── 4. DROP TYPE / DROP DOMAIN in-use rejection ─────────────────────────────

#[test]
fn drop_type_in_use_is_rejected_then_succeeds_after_table_drop() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TYPE order_status AS ENUM ('pending', 'paid')",
    )
    .unwrap();
    run(
        &mut engine,
        "CREATE TABLE orders (id INT, status order_status)",
    )
    .unwrap();

    let err = run(&mut engine, "DROP TYPE order_status").unwrap_err();
    match err {
        DbError::TypeInUse {
            ref name,
            ref table,
            ref column,
        } => {
            assert_eq!(name, "order_status");
            assert_eq!(table, "orders");
            assert_eq!(column, "status");
        }
        other => panic!("expected TypeInUse, got {other:?}"),
    }

    // After dropping the referencing table, DROP TYPE succeeds.
    run(&mut engine, "DROP TABLE orders").unwrap();
    run(&mut engine, "DROP TYPE order_status").unwrap();

    // And the type is really gone: re-declaring a table against it now fails.
    let err = run(
        &mut engine,
        "CREATE TABLE orders2 (id INT, status order_status)",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::UnknownType(_)), "{err:?}");
}

#[test]
fn drop_domain_in_use_is_rejected() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE DOMAIN email AS TEXT").unwrap();
    run(&mut engine, "CREATE TABLE users (id INT, contact email)").unwrap();

    let err = run(&mut engine, "DROP DOMAIN email").unwrap_err();
    assert!(matches!(err, DbError::TypeInUse { .. }), "{err:?}");

    run(&mut engine, "DROP TABLE users").unwrap();
    run(&mut engine, "DROP DOMAIN email").unwrap();
}

#[test]
fn drop_type_if_exists_is_idempotent() {
    let (mut engine, _dir) = fresh();
    // Absent name without IF EXISTS is an error (mirrors DROP TABLE).
    let err = run(&mut engine, "DROP TYPE never_declared").unwrap_err();
    assert!(matches!(err, DbError::UnknownType(_)), "{err:?}");

    // IF EXISTS makes it a no-op success.
    run(&mut engine, "DROP TYPE IF EXISTS never_declared").unwrap();
}

// ── 5. Duplicate / shadow / invalid-definition rejections ──────────────────

#[test]
fn duplicate_create_type_name_is_rejected() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TYPE color AS ENUM ('red', 'blue')").unwrap();
    let err = run(&mut engine, "CREATE TYPE color AS ENUM ('green')").unwrap_err();
    assert!(matches!(err, DbError::TypeAlreadyExists(_)), "{err:?}");

    // The shared namespace: a DOMAIN can't reuse an ENUM's name either.
    let err = run(&mut engine, "CREATE DOMAIN color AS TEXT").unwrap_err();
    assert!(matches!(err, DbError::TypeAlreadyExists(_)), "{err:?}");
}

#[test]
fn create_type_shadowing_builtin_is_rejected() {
    let (mut engine, _dir) = fresh();
    let err = run(&mut engine, "CREATE TYPE int AS ENUM ('a', 'b')").unwrap_err();
    assert!(matches!(err, DbError::InvalidNamedType(_)), "{err:?}");

    let err = run(&mut engine, "CREATE DOMAIN text AS INT").unwrap_err();
    assert!(matches!(err, DbError::InvalidNamedType(_)), "{err:?}");
}

#[test]
fn empty_enum_labels_rejected() {
    let (mut engine, _dir) = fresh();
    let err = run(&mut engine, "CREATE TYPE empty_enum AS ENUM ()").unwrap_err();
    assert!(matches!(err, DbError::InvalidNamedType(_)), "{err:?}");
}

#[test]
fn duplicate_enum_labels_rejected() {
    let (mut engine, _dir) = fresh();
    let err = run(&mut engine, "CREATE TYPE dup_enum AS ENUM ('a', 'b', 'a')").unwrap_err();
    assert!(matches!(err, DbError::InvalidNamedType(_)), "{err:?}");
}

// ── 6. Unknown named type in CREATE TABLE ───────────────────────────────────

#[test]
fn unknown_named_type_in_create_table_is_a_clear_error() {
    let (mut engine, _dir) = fresh();
    let err = run(
        &mut engine,
        "CREATE TABLE t (id INT, status not_a_real_type)",
    )
    .unwrap_err();
    match err {
        DbError::UnknownType(ref msg) => {
            assert!(
                msg.contains("not_a_real_type"),
                "error should name the unresolved type: {msg}"
            );
        }
        other => panic!("expected UnknownType, got {other:?}"),
    }
}

// ── 7. Enum column composes with existing machinery ─────────────────────────

#[test]
fn enum_column_supports_btree_index_and_where_equality() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped')",
    )
    .unwrap();
    run(
        &mut engine,
        "CREATE TABLE orders (id INT, status order_status)",
    )
    .unwrap();
    // A B-tree index on an enum column works exactly like on any TEXT column
    // (the desugar makes it plain TEXT under the hood).
    run(
        &mut engine,
        "CREATE INDEX idx_status ON orders USING BTREE (status)",
    )
    .unwrap();

    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (1, 'pending')",
    )
    .unwrap();
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (2, 'paid')",
    )
    .unwrap();
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (3, 'pending')",
    )
    .unwrap();

    let ids = {
        let mut v = select_ints(
            &mut engine,
            "SELECT id FROM orders WHERE status = 'pending'",
        );
        v.sort();
        v
    };
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn nullable_enum_column_accepts_null_check_does_not_reject_it() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TYPE order_status AS ENUM ('pending', 'paid')",
    )
    .unwrap();
    // No NOT NULL — the column is nullable.
    run(
        &mut engine,
        "CREATE TABLE orders (id INT, status order_status)",
    )
    .unwrap();

    // NULL must pass the synthesized CHECK (SQL CHECK semantics: NULL passes).
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (1, NULL)",
    )
    .unwrap();
    run(&mut engine, "INSERT INTO orders (id) VALUES (2)").unwrap();

    assert_eq!(select_ints(&mut engine, "SELECT id FROM orders"), {
        let mut v = vec![1i64, 2];
        v.sort();
        v
    });
}

#[test]
fn not_null_enum_column_still_rejects_null() {
    let (mut engine, _dir) = fresh();
    run(&mut engine, "CREATE TYPE color AS ENUM ('red', 'blue')").unwrap();
    run(&mut engine, "CREATE TABLE t (id INT, c color NOT NULL)").unwrap();

    let err = run(&mut engine, "INSERT INTO t (id, c) VALUES (1, NULL)").unwrap_err();
    // NOT NULL fires first/independently — the synthesized CHECK passing NULL
    // does not undermine an explicit NOT NULL on the same column.
    assert!(matches!(err, DbError::NotNullViolation { .. }), "{err:?}");
}

// ── 8. RLS/grants unaffected smoke ──────────────────────────────────────────

#[test]
fn enum_column_behaves_like_text_under_rls_policy() {
    let (mut engine, _dir) = fresh();
    run(
        &mut engine,
        "CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped')",
    )
    .unwrap();
    run(
        &mut engine,
        "CREATE TABLE orders (id INT, status order_status)",
    )
    .unwrap();
    // CREATE POLICY is auth-DDL, handled by `execute_sql_as`'s
    // `authz::parse_auth_stmt` pre-parse — not by the `LogicalPlan` pipeline
    // `run()`/`engine.execute_sql` drives (mirrors `tests/
    // item24_z2_per_op_policies.rs`'s `ddl()` helper).
    let xid = engine.begin().unwrap();
    engine
        .execute_sql_as(
            None,
            xid,
            "CREATE POLICY only_paid ON orders FOR SELECT USING (status = 'paid')",
        )
        .unwrap();
    engine.commit(xid).unwrap();

    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (1, 'pending')",
    )
    .unwrap();
    run(
        &mut engine,
        "INSERT INTO orders (id, status) VALUES (2, 'paid')",
    )
    .unwrap();

    // Same behavior a plain `status TEXT` column + policy would give: only
    // the row matching the policy predicate is visible.
    let ids = select_ints(&mut engine, "SELECT id FROM orders");
    assert_eq!(ids, vec![2]);
}
