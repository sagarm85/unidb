// Item-24 Z6 acceptance tests.
//
// Z6 — `current_user` SQL function in RLS policies:
//   * SELECT policy using `current_user` filters rows per identity.
//   * INSERT policy using `current_user` rejects rows whose owner != user.
//   * Superuser (None identity) bypasses RLS entirely — no false-negative.
//   * `POST /auth/preview` server route is covered in tests/server_authz.rs.

use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, DbError, Engine};

// ── helpers ──────────────────────────────────────────────────────────────────

#[allow(dead_code)]
fn denied<T: std::fmt::Debug>(r: &unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::PermissionDenied(_)))
}

fn policy_violation<T: std::fmt::Debug>(r: &unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::SqlPlan(ref msg)) if msg.contains("violates policy"))
}

/// Execute SQL as the embedded superuser (None identity).
fn exec(engine: &Engine, sql: &str) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql(x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    Ok(out)
}

/// Execute SQL as a named user, returning the raw Literal rows.
fn exec_as(
    engine: &Engine,
    user: Option<&str>,
    sql: &str,
) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql_as(user, x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    Ok(out)
}

/// Execute auth DDL (CREATE USER / GRANT / CREATE POLICY) as the embedded
/// superuser. Helper to avoid repetition in every test.
fn auth_ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

/// Text value from a Literal (panics if not Text).
fn text(lit: &Literal) -> &str {
    match lit {
        Literal::Text(s) => s.as_str(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Z6.1: SELECT policy with current_user ────────────────────────────────────

/// A SELECT policy `owner = current_user` automatically filters rows so
/// "alice" sees only alice's rows and "bob" sees only bob's rows.
#[test]
fn current_user_in_select_policy() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Schema + data.
    exec(
        &engine,
        "CREATE TABLE posts (id INT, owner TEXT, body TEXT)",
    )
    .unwrap();
    // Use CREATE USER to exit bootstrap mode so privileges are enforced.
    auth_ddl(&engine, "CREATE USER alice");
    auth_ddl(&engine, "CREATE USER bob");
    auth_ddl(&engine, "GRANT SELECT ON posts TO alice");
    auth_ddl(&engine, "GRANT SELECT ON posts TO bob");

    // Insert rows as superuser (bypasses RLS).
    exec(
        &engine,
        "INSERT INTO posts (id, owner, body) VALUES (1, 'alice', 'a post')",
    )
    .unwrap();
    exec(
        &engine,
        "INSERT INTO posts (id, owner, body) VALUES (2, 'bob', 'b post')",
    )
    .unwrap();
    exec(
        &engine,
        "INSERT INTO posts (id, owner, body) VALUES (3, 'alice', 'a post 2')",
    )
    .unwrap();

    // CREATE POLICY: owner must match the executing user.
    auth_ddl(
        &engine,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = current_user)",
    );

    // alice sees only her 2 rows.
    let alice_rows = exec_as(&engine, Some("alice"), "SELECT id, owner FROM posts").unwrap();
    assert_eq!(
        alice_rows.len(),
        2,
        "alice should see 2 rows, got {alice_rows:?}"
    );
    for row in &alice_rows {
        assert_eq!(text(&row[1]), "alice", "alice got unexpected row: {row:?}");
    }

    // bob sees only his 1 row.
    let bob_rows = exec_as(&engine, Some("bob"), "SELECT id, owner FROM posts").unwrap();
    assert_eq!(bob_rows.len(), 1, "bob should see 1 row, got {bob_rows:?}");
    assert_eq!(text(&bob_rows[0][1]), "bob");

    // Superuser (None) bypasses RLS — sees all 3 rows.
    let super_rows = exec(&engine, "SELECT id FROM posts").unwrap();
    assert_eq!(
        super_rows.len(),
        3,
        "superuser should see all 3 rows, got {super_rows:?}"
    );
}

// ── Z6.2: INSERT policy with current_user ────────────────────────────────────

/// A `FOR INSERT` policy `owner = current_user` rejects rows whose owner
/// field doesn't match the executing user.
#[test]
fn current_user_in_insert_policy() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE items (id INT, owner TEXT)").unwrap();

    // Use CREATE USER to exit bootstrap mode.
    auth_ddl(&engine, "CREATE USER alice");
    auth_ddl(&engine, "GRANT INSERT ON items TO alice");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON items FOR INSERT USING (owner = current_user)",
    );

    // alice inserts a row with owner='alice' — must succeed.
    let x = engine.begin().unwrap();
    let ok = engine.execute_sql_as(
        Some("alice"),
        x,
        "INSERT INTO items (id, owner) VALUES (1, 'alice')",
    );
    engine.commit(x).unwrap();
    assert!(ok.is_ok(), "alice inserting own row must succeed: {ok:?}");

    // alice inserts a row with owner='bob' — must be rejected by the policy.
    let x = engine.begin().unwrap();
    let bad = engine.execute_sql_as(
        Some("alice"),
        x,
        "INSERT INTO items (id, owner) VALUES (2, 'bob')",
    );
    engine.abort(x).unwrap();
    assert!(
        policy_violation(&bad),
        "alice inserting bob's row must violate policy, got {bad:?}"
    );
}

// ── Z6.3: Superuser bypass ───────────────────────────────────────────────────

/// The embedded path (`execute_sql`, user = None) must bypass RLS
/// completely — even a policy containing `current_user` must not block it.
#[test]
fn current_user_superuser_bypass() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE docs (id INT, owner TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO docs (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .unwrap();

    // Install a current_user SELECT policy. Engine is still in bootstrap mode
    // (no users created) — this tests that the bypass works regardless.
    auth_ddl(
        &engine,
        "CREATE POLICY p ON docs FOR SELECT USING (owner = current_user)",
    );

    // `execute_sql` (no user context) bypasses RLS → all 2 rows visible.
    let rows = exec(&engine, "SELECT id FROM docs").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "superuser must bypass RLS and see all 2 rows, got {rows:?}"
    );

    // `execute_sql_as(None, ...)` also bypasses RLS.
    let all = exec_as(&engine, None, "SELECT id FROM docs").unwrap();
    assert_eq!(
        all.len(),
        2,
        "execute_sql_as(None) must bypass RLS, got {all:?}"
    );

    // A named superuser (SUPERUSER flag) also bypasses RLS.
    auth_ddl(&engine, "CREATE USER root SUPERUSER");
    let root_rows = exec_as(&engine, Some("root"), "SELECT id FROM docs").unwrap();
    assert_eq!(
        root_rows.len(),
        2,
        "named superuser must bypass RLS, got {root_rows:?}"
    );
}

// ── Z6.4: current_user in user-supplied WHERE clause ─────────────────────────

/// `current_user` is also valid in a user-supplied WHERE clause, not just
/// in stored RLS policies.
#[test]
fn current_user_in_where_clause() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE data (id INT, owner TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO data (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .unwrap();

    // Use CREATE USER to exit bootstrap mode so grant check fires.
    auth_ddl(&engine, "CREATE USER alice");
    auth_ddl(&engine, "GRANT SELECT ON data TO alice");

    // alice queries with explicit WHERE owner = current_user.
    let rows = exec_as(
        &engine,
        Some("alice"),
        "SELECT id FROM data WHERE owner = current_user",
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "explicit WHERE current_user: {rows:?}");
    assert_eq!(rows[0][0], Literal::Int(1));
}
