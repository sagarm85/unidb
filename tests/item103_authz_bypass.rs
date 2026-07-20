// Item 103 — AuthZ v2: superuser RLS bypass correctness tests.
//
// Verifies that:
//   1. A named SUPERUSER bypasses `current_user`-referencing policies.
//   2. The no-sub (embedded, `user = None`) path bypasses those policies.
//   3. A regular named user is still correctly filtered by those policies.
//
// All three scenarios use the embedded API (`execute_sql` / `execute_sql_as`),
// which is the correct surface for unit-testing the RLS logic in isolation
// from the server / concurrent-read-handle code paths.

use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, Engine};

// ── helpers ──────────────────────────────────────────────────────────────────

fn rows(engine: &Engine, user: Option<&str>, sql: &str) -> Vec<Vec<Literal>> {
    let x = engine.begin().unwrap();
    let results = engine.execute_sql_as(user, x, sql).unwrap();
    engine.commit(x).unwrap();
    let mut out = Vec::new();
    for r in results {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    out
}

fn embedded_rows(engine: &Engine, sql: &str) -> Vec<Vec<Literal>> {
    let x = engine.begin().unwrap();
    let results = engine.execute_sql(x, sql).unwrap();
    engine.commit(x).unwrap();
    let mut out = Vec::new();
    for r in results {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    out
}

fn ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

// ── Test 1: named superuser bypasses current_user policy ─────────────────────

/// A named user created with `SUPERUSER` must see all rows from a table that
/// has a `USING (owner = current_user)` policy — the policy must be skipped
/// entirely, not evaluated against the superuser's name.
#[test]
fn superuser_bypasses_current_user_policy() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Create table and insert two rows owned by different users.
    ddl(
        &engine,
        "CREATE TABLE demo_orders (id INT, owner TEXT, amount INT)",
    );
    ddl(
        &engine,
        "INSERT INTO demo_orders VALUES (1, 'test_user', 100)",
    );
    ddl(
        &engine,
        "INSERT INTO demo_orders VALUES (2, 'someone_else', 200)",
    );

    // Create users: admin is a superuser, test_user is a regular user.
    ddl(&engine, "CREATE USER admin SUPERUSER");
    ddl(&engine, "CREATE USER test_user");
    ddl(&engine, "GRANT SELECT ON demo_orders TO test_user");
    ddl(&engine, "GRANT SELECT ON demo_orders TO admin");

    // Install the current_user policy.
    ddl(
        &engine,
        "CREATE POLICY orders_owner_only ON demo_orders FOR SELECT USING (owner = current_user)",
    );

    // Named superuser must see BOTH rows (policy bypassed).
    let admin_rows = rows(&engine, Some("admin"), "SELECT id FROM demo_orders");
    assert_eq!(
        admin_rows.len(),
        2,
        "named superuser must bypass current_user policy and see all rows; got {admin_rows:?}"
    );

    // Regular user must see only THEIR row (policy enforced).
    let user_rows = rows(&engine, Some("test_user"), "SELECT id FROM demo_orders");
    assert_eq!(
        user_rows.len(),
        1,
        "regular user must be filtered by policy; got {user_rows:?}"
    );
    assert_eq!(user_rows[0][0], Literal::Int(1));
}

// ── Test 2: no-sub (embedded / None) path bypasses current_user policy ───────

/// The embedded API path (`execute_sql` / `execute_sql_as(None, ...)`) must
/// bypass `current_user`-referencing policies. This is the implicit superuser
/// caller — inserting / administering data from within the application binary.
#[test]
fn no_sub_bypasses_current_user_policy() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE TABLE orders (id INT, owner TEXT)");
    ddl(&engine, "INSERT INTO orders VALUES (1, 'alice')");
    ddl(&engine, "INSERT INTO orders VALUES (2, 'bob')");

    // Install a current_user SELECT policy.
    ddl(
        &engine,
        "CREATE POLICY o ON orders FOR SELECT USING (owner = current_user)",
    );

    // Bootstrap mode (no users yet): embedded path must still see all rows.
    let all_embedded = embedded_rows(&engine, "SELECT id FROM orders");
    assert_eq!(
        all_embedded.len(),
        2,
        "embedded execute_sql must bypass current_user policy (bootstrap mode); got {all_embedded:?}"
    );

    // Create a user to exit bootstrap mode, then re-check.
    ddl(&engine, "CREATE USER alice");
    ddl(&engine, "GRANT SELECT ON orders TO alice");

    let all_as_none = rows(&engine, None, "SELECT id FROM orders");
    assert_eq!(
        all_as_none.len(),
        2,
        "execute_sql_as(None) must bypass current_user policy (post-user-creation); got {all_as_none:?}"
    );

    let all_embedded_post = embedded_rows(&engine, "SELECT id FROM orders");
    assert_eq!(
        all_embedded_post.len(),
        2,
        "execute_sql must bypass current_user policy (post-user-creation); got {all_embedded_post:?}"
    );
}

// ── Test 3: regular user is still filtered ───────────────────────────────────

/// A regular named user (not a superuser) must be filtered by a
/// `current_user`-referencing policy. This ensures the bypass is
/// only applied for privileged callers.
#[test]
fn regular_user_filtered_by_current_user_policy() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE TABLE items (id INT, owner TEXT)");
    ddl(&engine, "INSERT INTO items VALUES (1, 'alice')");
    ddl(&engine, "INSERT INTO items VALUES (2, 'bob')");
    ddl(&engine, "INSERT INTO items VALUES (3, 'alice')");

    ddl(&engine, "CREATE USER alice");
    ddl(&engine, "CREATE USER bob");
    ddl(&engine, "GRANT SELECT ON items TO alice");
    ddl(&engine, "GRANT SELECT ON items TO bob");

    ddl(
        &engine,
        "CREATE POLICY item_owner ON items FOR SELECT USING (owner = current_user)",
    );

    // alice sees her 2 rows only.
    let alice_rows = rows(&engine, Some("alice"), "SELECT id FROM items");
    assert_eq!(
        alice_rows.len(),
        2,
        "alice must see only her 2 rows; got {alice_rows:?}"
    );
    for row in &alice_rows {
        // All returned rows should have id 1 or 3 (alice's rows).
        let id = match &row[0] {
            Literal::Int(i) => *i,
            other => panic!("expected Int, got {other:?}"),
        };
        assert!(id == 1 || id == 3, "alice got unexpected id {id}");
    }

    // bob sees his 1 row only.
    let bob_rows = rows(&engine, Some("bob"), "SELECT id FROM items");
    assert_eq!(
        bob_rows.len(),
        1,
        "bob must see only his 1 row; got {bob_rows:?}"
    );
    assert_eq!(bob_rows[0][0], Literal::Int(2));
}
