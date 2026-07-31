// Item 112 (item-122 Workstream B5) — column-level GRANT/REVOKE acceptance
// tests: narrowing semantics, error-not-mask, write-side enforcement,
// RETURNING, the RLS policy-column exemption, the item-111
// `information_schema.columns` filter, and whole-table back-compat.

use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::{DbError, Engine};

fn denied<T: std::fmt::Debug>(r: unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::PermissionDenied(_)))
}

fn rows_of(
    engine: &Engine,
    user: Option<&str>,
    sql: &str,
) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql_as(user, x, sql);
    match res {
        Ok(results) => {
            engine.commit(x).unwrap();
            match results.into_iter().next().unwrap() {
                ExecResult::Rows { rows, .. } => Ok(rows),
                other => panic!("expected Rows, got {other:?}"),
            }
        }
        Err(e) => {
            engine.abort(x).unwrap();
            Err(e)
        }
    }
}

fn ddl(engine: &Engine, user: Option<&str>, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(user, x, sql).unwrap();
    engine.commit(x).unwrap();
}

fn text(lit: &Literal) -> String {
    match lit {
        Literal::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── (a) SELECT column grant: admits only listed columns, errors (not
//    masks) on an ungranted column or a SELECT * that isn't fully granted ──

#[test]
fn select_column_grant_admits_only_listed_columns_select_star_requires_all() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(
        &engine,
        None,
        "CREATE TABLE users (id INT, email TEXT, name TEXT, password_hash TEXT)",
    );
    ddl(&engine, None, "CREATE USER support");
    ddl(
        &engine,
        None,
        "GRANT SELECT (email, name) ON users TO support",
    );
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO users (id, email, name, password_hash) VALUES (1, 'a@x.com', 'Ann', 'h1')",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Granted column: OK.
    let rows = rows_of(&engine, Some("support"), "SELECT email FROM users").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(text(&rows[0][0]), "a@x.com");

    // Ungranted column: PERMISSION_DENIED, not a masked/NULL result.
    assert!(denied(rows_of(
        &engine,
        Some("support"),
        "SELECT password_hash FROM users"
    )));

    // A projection mixing a granted and an ungranted column: denied.
    assert!(denied(rows_of(
        &engine,
        Some("support"),
        "SELECT email, password_hash FROM users"
    )));

    // SELECT * requires every column granted: denied (not a partial/NULL-
    // filled row).
    assert!(denied(rows_of(
        &engine,
        Some("support"),
        "SELECT * FROM users"
    )));

    // A WHERE-clause reference to an ungranted column is checked too (reads
    // outside the projection still count).
    assert!(denied(rows_of(
        &engine,
        Some("support"),
        "SELECT email FROM users WHERE password_hash = 'h1'"
    )));
}

// ── (b) column UPDATE/INSERT enforcement ──────────────────────────────────

#[test]
fn update_column_grant_enforces_assignment_targets() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(
        &engine,
        None,
        "CREATE TABLE tickets (id INT, status TEXT, owner_id INT)",
    );
    ddl(&engine, None, "CREATE USER agent");
    ddl(&engine, None, "GRANT SELECT ON tickets TO agent");
    ddl(&engine, None, "GRANT UPDATE (status) ON tickets TO agent");
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO tickets (id, status, owner_id) VALUES (1, 'open', 42)",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Updating the granted column succeeds.
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(
                Some("agent"),
                x,
                "UPDATE tickets SET status = 'closed' WHERE id = 1",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Updating an ungranted column is denied (write-side narrowing).
    {
        let x = engine.begin().unwrap();
        assert!(denied(engine.execute_sql_as(
            Some("agent"),
            x,
            "UPDATE tickets SET owner_id = 99 WHERE id = 1"
        )));
        engine.abort(x).unwrap();
    }

    // Confirm the denied UPDATE made no change.
    let rows = rows_of(&engine, None, "SELECT owner_id FROM tickets WHERE id = 1").unwrap();
    assert_eq!(rows[0][0], Literal::Int(42));
}

#[test]
fn insert_column_grant_enforces_column_list() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t (a INT, b INT, c INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT SELECT ON t TO u");
    ddl(&engine, None, "GRANT INSERT (a, b) ON t TO u");

    // Inserting only granted columns succeeds.
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(Some("u"), x, "INSERT INTO t (a, b) VALUES (1, 2)")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Naming an ungranted column in the INSERT list is denied.
    {
        let x = engine.begin().unwrap();
        assert!(denied(engine.execute_sql_as(
            Some("u"),
            x,
            "INSERT INTO t (a, b, c) VALUES (2, 3, 4)"
        )));
        engine.abort(x).unwrap();
    }

    // Omitting the column list (implicitly every column) also requires
    // every column granted — same error-not-mask contract as `SELECT *`.
    {
        let x = engine.begin().unwrap();
        assert!(denied(engine.execute_sql_as(
            Some("u"),
            x,
            "INSERT INTO t VALUES (5, 6, 7)"
        )));
        engine.abort(x).unwrap();
    }
}

// ── (c) RETURNING an ungranted column is denied ───────────────────────────

#[test]
fn returning_ungranted_column_denied() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t (a INT, b INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT INSERT (a, b) ON t TO u");
    ddl(&engine, None, "GRANT SELECT (a) ON t TO u");

    // RETURNING a column the caller can SELECT: OK.
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(
                Some("u"),
                x,
                "INSERT INTO t (a, b) VALUES (1, 2) RETURNING a",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // RETURNING a column the caller can INSERT but not SELECT: denied —
    // RETURNING is a read, checked against the SELECT column grant.
    {
        let x = engine.begin().unwrap();
        assert!(denied(engine.execute_sql_as(
            Some("u"),
            x,
            "INSERT INTO t (a, b) VALUES (3, 4) RETURNING b"
        )));
        engine.abort(x).unwrap();
    }

    // RETURNING * requires every column SELECT-granted: denied.
    {
        let x = engine.begin().unwrap();
        assert!(denied(engine.execute_sql_as(
            Some("u"),
            x,
            "INSERT INTO t (a, b) VALUES (5, 6) RETURNING *"
        )));
        engine.abort(x).unwrap();
    }
}

// ── (d) a policy referencing an ungranted column still evaluates ─────────

#[test]
fn rls_policy_referencing_ungranted_column_still_evaluates() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(
        &engine,
        None,
        "CREATE TABLE docs (id INT, owner TEXT, secret TEXT, title TEXT)",
    );
    ddl(&engine, None, "CREATE USER alice");
    // Alice may SELECT only `title` — she has NO grant whatsoever on
    // `owner`, the column the RLS policy filters on.
    ddl(&engine, None, "GRANT SELECT (title) ON docs TO alice");
    ddl(
        &engine,
        None,
        "CREATE POLICY p ON docs FOR SELECT USING (owner = current_user)",
    );
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO docs (id, owner, secret, title) VALUES (1, 'alice', 's1', 'mine')",
            )
            .unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO docs (id, owner, secret, title) VALUES (2, 'bob', 's2', 'not mine')",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Alice's own SQL only ever references `title` — never `owner` — so her
    // column-scoped SELECT(title) grant must be sufficient even though the
    // RLS rewrite ANDs in `owner = current_user()` behind the scenes: the
    // policy-column exemption (item 112 Step 0) means the *injected*
    // predicate is never checked against her column grants at all.
    let rows = rows_of(&engine, Some("alice"), "SELECT title FROM docs").unwrap();
    assert_eq!(rows.len(), 1, "RLS still filters to alice's own row");
    assert_eq!(text(&rows[0][0]), "mine");

    // If alice's own SQL explicitly references `owner` (not just the
    // injected policy), that read IS checked normally and denied.
    assert!(denied(rows_of(
        &engine,
        Some("alice"),
        "SELECT title FROM docs WHERE owner = 'alice'"
    )));
}

// ── (e) information_schema.columns shows only granted columns ────────────

#[test]
fn information_schema_columns_filtered_by_select_column_grant() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(
        &engine,
        None,
        "CREATE TABLE users (id INT, email TEXT, password_hash TEXT)",
    );
    ddl(&engine, None, "CREATE USER support");
    ddl(&engine, None, "GRANT SELECT (email) ON users TO support");

    let rows = rows_of(
        &engine,
        Some("support"),
        "SELECT * FROM information_schema.columns",
    )
    .unwrap();
    // Schema: table_catalog(0) table_schema(1) table_name(2) column_name(3)
    // ordinal_position(4) column_default(5) is_nullable(6) data_type(7).
    let cols: Vec<String> = rows.iter().map(|r| text(&r[3])).collect();
    assert_eq!(
        cols,
        vec!["email".to_string()],
        "only the SELECT-granted column is visible: {rows:?}"
    );

    // ordinal_position reflects the column's REAL position (2nd column,
    // 1-based -> 2), not a renumbering among only the visible columns.
    assert_eq!(rows[0][4], Literal::Int(2));
}

// ── (f) whole-table grants are unaffected (100% back-compat) ─────────────

#[test]
fn whole_table_grant_still_admits_every_column_and_select_star() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t (a INT, b INT, c INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT ALL ON t TO u");
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (a, b, c) VALUES (1, 2, 3)")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // SELECT * works with no column restriction whatsoever.
    let rows = rows_of(&engine, Some("u"), "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);

    // Every individual column read works too.
    rows_of(&engine, Some("u"), "SELECT a, b, c FROM t").unwrap();

    // Whole-table UPDATE/INSERT/RETURNING all still unrestricted.
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(Some("u"), x, "UPDATE t SET a = 10, b = 20 WHERE c = 3")
            .unwrap();
        engine.commit(x).unwrap();
    }
    rows_of(&engine, Some("u"), "SELECT a FROM t WHERE a = 10").unwrap();

    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(
                Some("u"),
                x,
                "INSERT INTO t (a, b, c) VALUES (4, 5, 6) RETURNING *",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }
}

#[test]
fn revoke_narrows_and_table_level_revoke_clears() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t (a INT, b INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT SELECT (a, b) ON t TO u");
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (a, b) VALUES (1, 2)")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Both columns readable before the revoke.
    rows_of(&engine, Some("u"), "SELECT a, b FROM t").unwrap();

    // Column-scoped REVOKE narrows.
    ddl(&engine, None, "REVOKE SELECT (b) ON t FROM u");
    rows_of(&engine, Some("u"), "SELECT a FROM t").unwrap();
    assert!(denied(rows_of(&engine, Some("u"), "SELECT b FROM t")));

    // Re-grant, then a table-level REVOKE (no column list) clears entirely.
    ddl(&engine, None, "GRANT SELECT (b) ON t TO u");
    rows_of(&engine, Some("u"), "SELECT b FROM t").unwrap();
    ddl(&engine, None, "REVOKE SELECT ON t FROM u");
    assert!(denied(rows_of(&engine, Some("u"), "SELECT a FROM t")));
}

/// Column-scoped REVOKE narrowing a grantee who currently holds the WHOLE
/// table (not merely a prior column-scoped grant) — exercises
/// `Engine::exec_auth_stmt`'s `materialize_all_to_columns` step.
#[test]
fn revoke_column_from_whole_table_grant_narrows_to_the_complement() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t (a INT, b INT, c INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT SELECT ON t TO u"); // whole-table
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (a, b, c) VALUES (1, 2, 3)")
            .unwrap();
        engine.commit(x).unwrap();
    }

    ddl(&engine, None, "REVOKE SELECT (b) ON t FROM u");

    // `a` and `c` remain readable (the complement of the revoked column);
    // `b` and `SELECT *` are now denied.
    rows_of(&engine, Some("u"), "SELECT a FROM t").unwrap();
    rows_of(&engine, Some("u"), "SELECT c FROM t").unwrap();
    assert!(denied(rows_of(&engine, Some("u"), "SELECT b FROM t")));
    assert!(denied(rows_of(&engine, Some("u"), "SELECT * FROM t")));
}

// ── QuerySpec shapes (joins/aggregates) ───────────────────────────────────

#[test]
fn join_query_enforces_column_grants_per_table() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(
        &engine,
        None,
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
    );
    ddl(
        &engine,
        None,
        "CREATE TABLE customers (id INT, name TEXT, ssn TEXT)",
    );
    ddl(&engine, None, "CREATE USER analyst");
    ddl(&engine, None, "GRANT SELECT ON orders TO analyst");
    // `id` is granted too — the JOIN ON condition reads `customers.id`, and
    // that read is checked exactly like any other column reference (Postgres
    // semantics: a join key is a read, not exempt from column grants).
    ddl(
        &engine,
        None,
        "GRANT SELECT (name, id) ON customers TO analyst",
    );
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO customers (id, name, ssn) VALUES (1, 'Ann', '111-22-3333')",
            )
            .unwrap();
        engine
            .execute_sql(
                x,
                "INSERT INTO orders (id, customer_id, total) VALUES (1, 1, 100)",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Joining on the granted customers.name column succeeds.
    let rows = rows_of(
        &engine,
        Some("analyst"),
        "SELECT orders.total, customers.name FROM orders JOIN customers ON orders.customer_id = customers.id",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);

    // Selecting customers.ssn (ungranted) through the join is denied.
    assert!(denied(rows_of(
        &engine,
        Some("analyst"),
        "SELECT orders.total, customers.ssn FROM orders JOIN customers ON orders.customer_id = customers.id",
    )));

    // customers.* requires every customers column granted: denied.
    assert!(denied(rows_of(
        &engine,
        Some("analyst"),
        "SELECT customers.* FROM orders JOIN customers ON orders.customer_id = customers.id",
    )));
}

/// An unqualified column name that exists ambiguously across a join must
/// fail closed if it is ungranted on *any* candidate table it could bind to
/// — "any ambiguity resolves to deny" (item 112 hard constraint).
#[test]
fn ambiguous_unqualified_column_in_join_fails_closed() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    ddl(&engine, None, "CREATE TABLE t1 (id INT, val INT)");
    ddl(&engine, None, "CREATE TABLE t2 (id INT, val INT)");
    ddl(&engine, None, "CREATE USER u");
    ddl(&engine, None, "GRANT SELECT ON t1 TO u"); // whole-table on t1
    ddl(&engine, None, "GRANT SELECT (id) ON t2 TO u"); // column-scoped on t2 (no `val`)

    // `val` unqualified could resolve to either t1 or t2; u lacks it on t2,
    // so the reference must be denied even though t1.val alone would be fine.
    assert!(denied(rows_of(
        &engine,
        Some("u"),
        "SELECT val FROM t1 JOIN t2 ON t1.id = t2.id",
    )));

    // Qualifying it removes the ambiguity: t1.val is fine (t1 is whole-table).
    rows_of(
        &engine,
        Some("u"),
        "SELECT t1.val FROM t1 JOIN t2 ON t1.id = t2.id",
    )
    .unwrap();
}

// ── (g) existing whole-table / item-111 behavior is exercised by the full
//    authz/rls/policy/information_schema suites (unchanged, run alongside
//    this file in the gate) ────────────────────────────────────────────────
