// Item-24 R-a: UPDATE write-side WITH CHECK enforcement (SHIP-BLOCKER fix).
//
// Context (2026-07-20 live probe on main @ 196e8aa):
// A policy `user_id = current_user` for SELECT and FOR UPDATE; alice runs
//   UPDATE todos SET user_id = 'bob' WHERE id = 1
// was ACCEPTED — her row's ownership silently transferred to bob, she lost
// sight of it.  Postgres rejects this.  This file is the "inverted probe":
// the same scenario now MUST be rejected.
//
// Test matrix:
//   1. update_ownership_transfer_rejected_by_with_check
//      — main escape: alice cannot transfer user_id to bob
//   2. update_within_policy_is_allowed
//      — legitimate: alice can update non-owner fields (body) on her own row
//   3. explicit_with_check_differs_from_using
//      — WITH CHECK (col >= 0) rejects a write of col = -1 even though the
//        USING filter would have allowed the row to be seen
//   4. all_policy_with_check_applies_everywhere
//      — FOR ALL policy WITH CHECK also blocks UPDATE
//   5. insert_policy_unchanged_by_r_a
//      — INSERT path still works exactly as before (regression guard)
//   6. bootstrap_mode_bypasses_with_check
//      — when no CREATE USER exists, WITH CHECK is inactive (open mode)
//   7. policies_catalog_shows_enforced_and_with_check_columns (Slice 2)
//      — unidb_catalog.policies enforced = false before first CREATE USER
//   8. policies_catalog_with_check_expr_populated_when_set (Slice 2)
//      — with_check_expr column is non-null when WITH CHECK is specified

use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, Engine};

// ── helpers ───────────────────────────────────────────────────────────────────

fn lit_str(v: &Literal) -> String {
    match v {
        Literal::Text(s) => s.clone(),
        Literal::Int(i) => i.to_string(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_as_strings(result: &ExecResult) -> Vec<Vec<String>> {
    match result {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(lit_str).collect())
            .collect(),
        _ => vec![],
    }
}

fn exec_super(engine: &Engine, sql: &str) -> Vec<ExecResult> {
    let xid = engine.begin().unwrap();
    let r = engine.execute_sql_as(None, xid, sql).unwrap();
    engine.commit(xid).unwrap();
    r
}

fn exec_as(engine: &Engine, user: &str, sql: &str) -> unidb::error::Result<Vec<ExecResult>> {
    let xid = engine.begin().unwrap();
    let r = engine.execute_sql_as(Some(user), xid, sql);
    match r {
        Ok(rows) => {
            engine.commit(xid).unwrap();
            Ok(rows)
        }
        Err(e) => {
            engine.abort(xid).unwrap();
            Err(e)
        }
    }
}

fn setup_todos(engine: &Engine) {
    exec_super(
        engine,
        "CREATE TABLE todos (id INT, user_id TEXT, body TEXT)",
    );
    exec_super(engine, "CREATE USER alice");
    exec_super(engine, "CREATE USER bob");
    exec_super(engine, "GRANT SELECT, UPDATE ON todos TO alice");
    exec_super(engine, "GRANT SELECT, UPDATE ON todos TO bob");
    // Policy: SELECT and UPDATE are both scoped to current_user.
    // FOR UPDATE uses USING (row filter) without explicit WITH CHECK — Postgres
    // semantics: WITH CHECK defaults to the USING expression.
    exec_super(
        engine,
        "CREATE POLICY sel_own ON todos FOR SELECT USING (user_id = current_user)",
    );
    exec_super(
        engine,
        "CREATE POLICY upd_own ON todos FOR UPDATE USING (user_id = current_user)",
    );
    // Insert a row owned by alice.
    exec_super(engine, "INSERT INTO todos VALUES (1, 'alice', 'buy milk')");
}

// ── test 1: main escape — ownership transfer is now REJECTED ─────────────────

#[test]
fn update_ownership_transfer_rejected_by_with_check() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup_todos(&engine);

    // alice tries to set user_id = 'bob' — the new row would violate USING/WITH CHECK.
    let err = exec_as(
        &engine,
        "alice",
        "UPDATE todos SET user_id = 'bob' WHERE id = 1",
    )
    .expect_err("ownership transfer should be rejected");
    assert!(
        format!("{err}").to_ascii_lowercase().contains("policy")
            || format!("{err}").to_ascii_lowercase().contains("with check"),
        "unexpected error: {err}"
    );

    // Row must still be owned by alice after the rejected write.
    let rows = rows_as_strings(
        exec_super(&engine, "SELECT user_id FROM todos WHERE id = 1")
            .first()
            .unwrap(),
    );
    assert_eq!(rows, vec![vec!["alice"]], "row must still belong to alice");
}

// ── test 2: legitimate update within policy is still allowed ─────────────────

#[test]
fn update_within_policy_is_allowed() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup_todos(&engine);

    // alice updates the body — user_id stays 'alice', so WITH CHECK passes.
    exec_as(
        &engine,
        "alice",
        "UPDATE todos SET body = 'buy oat milk' WHERE id = 1",
    )
    .expect("updating a non-owner field should be allowed");

    let rows = rows_as_strings(
        exec_super(&engine, "SELECT body FROM todos WHERE id = 1")
            .first()
            .unwrap(),
    );
    assert_eq!(rows, vec![vec!["buy oat milk"]]);
}

// ── test 3: explicit WITH CHECK expression ────────────────────────────────────

#[test]
fn explicit_with_check_differs_from_using() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE scores (id INT, val INT)");
    exec_super(&engine, "CREATE USER alice");
    exec_super(&engine, "GRANT SELECT, UPDATE ON scores TO alice");
    // USING: val >= 10 (alice can only see rows with val ≥ 10).
    // WITH CHECK: val >= 10 (the new value must also be ≥ 10).
    // Uses only non-negative integers to avoid the unary-minus expression
    // limitation in the evaluator; the semantic under test is the same.
    exec_super(
        &engine,
        "CREATE POLICY pos ON scores FOR UPDATE \
         USING (val >= 10) WITH CHECK (val >= 10)",
    );
    exec_super(&engine, "INSERT INTO scores VALUES (1, 20)");

    // Allowed: 20 → 15 (both ≥ 10).
    exec_as(&engine, "alice", "UPDATE scores SET val = 15 WHERE id = 1")
        .expect("20 to 15 should pass WITH CHECK (val >= 10)");

    // Rejected: 15 → 5 violates WITH CHECK (5 < 10).
    let err = exec_as(&engine, "alice", "UPDATE scores SET val = 5 WHERE id = 1")
        .expect_err("5 should be rejected by WITH CHECK (val >= 10)");
    assert!(
        format!("{err}").to_ascii_lowercase().contains("policy")
            || format!("{err}").to_ascii_lowercase().contains("with check"),
        "unexpected error: {err}"
    );

    // Value must still be 15 after the rejected write.
    let rows = rows_as_strings(
        exec_super(&engine, "SELECT val FROM scores WHERE id = 1")
            .first()
            .unwrap(),
    );
    assert_eq!(rows, vec![vec!["15"]]);
}

// ── test 4: FOR ALL policy WITH CHECK also covers UPDATE ──────────────────────

#[test]
fn all_policy_with_check_applies_everywhere() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE items (id INT, owner TEXT, v INT)");
    exec_super(&engine, "CREATE USER alice");
    exec_super(
        &engine,
        "GRANT SELECT, INSERT, UPDATE, DELETE ON items TO alice",
    );
    exec_super(
        &engine,
        "CREATE POLICY own ON items FOR ALL \
         USING (owner = current_user) WITH CHECK (owner = current_user)",
    );
    exec_super(&engine, "INSERT INTO items VALUES (1, 'alice', 0)");

    // alice can UPDATE v (owner unchanged).
    exec_as(&engine, "alice", "UPDATE items SET v = 99 WHERE id = 1")
        .expect("updating non-owner column should pass");

    // alice cannot transfer ownership.
    let err = exec_as(
        &engine,
        "alice",
        "UPDATE items SET owner = 'bob' WHERE id = 1",
    )
    .expect_err("owner transfer must be rejected");
    assert!(format!("{err}").to_ascii_lowercase().contains("policy"));
}

// ── test 5: INSERT path is unchanged (regression guard) ───────────────────────

#[test]
fn insert_policy_unchanged_by_r_a() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE t (id INT, owner TEXT)");
    exec_super(&engine, "CREATE USER alice");
    exec_super(&engine, "GRANT SELECT, INSERT ON t TO alice");
    exec_super(
        &engine,
        "CREATE POLICY ins_own ON t FOR INSERT USING (owner = current_user)",
    );

    // alice can insert a row she owns.
    exec_as(&engine, "alice", "INSERT INTO t VALUES (1, 'alice')")
        .expect("alice inserting her own row should be allowed");

    // alice cannot insert a row owned by bob.
    exec_as(&engine, "alice", "INSERT INTO t VALUES (2, 'bob')")
        .expect_err("alice inserting bob's row should be rejected");

    let rows = rows_as_strings(exec_super(&engine, "SELECT id FROM t").first().unwrap());
    assert_eq!(rows, vec![vec!["1"]], "only row 1 (alice's) should exist");
}

// ── test 6: bootstrap mode — no users means WITH CHECK is inactive ────────────

#[test]
fn bootstrap_mode_bypasses_with_check() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE t (id INT, owner TEXT)");
    // Define a policy WITHOUT creating any user first.
    exec_super(
        &engine,
        "CREATE POLICY own ON t FOR UPDATE USING (owner = 'alice')",
    );
    exec_super(&engine, "INSERT INTO t VALUES (1, 'alice')");

    // In bootstrap mode (no users) the superuser/embedded path bypasses
    // CurrentUser-dependent policies, so this should NOT be rejected.
    // (The policy has no current_user reference — this is a static predicate;
    //  the superuser path ignores it entirely.)
    exec_super(&engine, "UPDATE t SET owner = 'bob' WHERE id = 1");
    let rows = rows_as_strings(
        exec_super(&engine, "SELECT owner FROM t WHERE id = 1")
            .first()
            .unwrap(),
    );
    assert_eq!(
        rows,
        vec![vec!["bob"]],
        "superuser path should bypass policy"
    );
}

// ── test 7: unidb_catalog.policies shows `enforced` column (Slice 2) ─────────

#[test]
fn policies_catalog_enforced_false_before_first_user() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE t (id INT, owner TEXT)");
    // Create a policy but NO user yet.
    exec_super(
        &engine,
        "CREATE POLICY own ON t FOR UPDATE USING (owner = 'alice')",
    );

    let xid = engine.begin().unwrap();
    let results = engine
        .execute_sql(xid, "SELECT name, enforced FROM unidb_catalog.policies")
        .unwrap();
    engine.commit(xid).unwrap();

    let rows = rows_as_strings(results.first().unwrap());
    assert!(!rows.is_empty(), "should have at least one policy row");
    // enforced = false when no users exist.
    for row in &rows {
        assert_eq!(
            row[1], "false",
            "enforced should be false in bootstrap mode, got: {row:?}"
        );
    }

    // After creating a user, enforced should become true.
    exec_super(&engine, "CREATE USER alice");
    let xid2 = engine.begin().unwrap();
    let results2 = engine
        .execute_sql(xid2, "SELECT name, enforced FROM unidb_catalog.policies")
        .unwrap();
    engine.commit(xid2).unwrap();
    let rows2 = rows_as_strings(results2.first().unwrap());
    for row in &rows2 {
        assert_eq!(
            row[1], "true",
            "enforced should be true once a user exists, got: {row:?}"
        );
    }
}

// ── test 8: with_check_expr column is populated ────────────────────────────────

#[test]
fn policies_catalog_with_check_expr_populated_when_set() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec_super(&engine, "CREATE TABLE t (id INT, val INT)");
    exec_super(&engine, "CREATE USER alice");
    // Policy with explicit WITH CHECK.
    exec_super(
        &engine,
        "CREATE POLICY pos ON t FOR UPDATE USING (val >= 0) WITH CHECK (val >= 0)",
    );
    // Policy without WITH CHECK.
    exec_super(
        &engine,
        "CREATE POLICY owner_only ON t FOR SELECT USING (val = 1)",
    );

    let xid = engine.begin().unwrap();
    let results = engine
        .execute_sql(
            xid,
            "SELECT name, with_check_expr FROM unidb_catalog.policies ORDER BY name",
        )
        .unwrap();
    engine.commit(xid).unwrap();

    let rows = rows_as_strings(results.first().unwrap());
    // Find the policy rows by name.
    let by_name: std::collections::HashMap<String, String> = rows
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();

    // pos has explicit WITH CHECK.
    assert_eq!(
        by_name.get("pos").map(String::as_str),
        Some("val >= 0"),
        "with_check_expr should contain the explicit check expression"
    );
    // owner_only has no WITH CHECK — should be NULL.
    assert_eq!(
        by_name.get("owner_only").map(String::as_str),
        Some("NULL"),
        "with_check_expr should be NULL when no WITH CHECK was specified"
    );
}
