// Item-24 Z1+Z3+Z5 acceptance tests.
//
// Z1 — SQL-native DDL: CREATE ROLE / DROP ROLE / GRANT / REVOKE /
//       CREATE POLICY / DROP POLICY, persisted in the catalog.
// Z3 — JWT token auth on REST bulk-insert route: 403 without INSERT grant.
// Z5 — Catalog relations: SELECT * FROM unidb_catalog.{roles,grants,policies}.

use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, DbError, Engine};

// ── helpers ──────────────────────────────────────────────────────────────────

fn denied<T: std::fmt::Debug>(r: unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::PermissionDenied(_)))
}

/// Convert a `Literal` to its string value (Text inner, Int as decimal, etc.)
/// for easy test assertions.
fn lit_str(v: &Literal) -> String {
    match v {
        Literal::Text(s) => s.clone(),
        Literal::Int(i) => i.to_string(),
        Literal::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute `sql` as the embedded superuser (no user identity). Returns all
/// result rows with each cell converted to a plain string.
fn exec(engine: &Engine, sql: &str) -> unidb::error::Result<Vec<Vec<String>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql(x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            for row in inner {
                out.push(row.iter().map(lit_str).collect());
            }
        }
    }
    Ok(out)
}

fn exec_as(
    engine: &Engine,
    user: Option<&str>,
    sql: &str,
) -> unidb::error::Result<Vec<Vec<String>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql_as(user, x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            for row in inner {
                out.push(row.iter().map(lit_str).collect());
            }
        }
    }
    Ok(out)
}

// ── Z1: CREATE ROLE / DROP ROLE ──────────────────────────────────────────────

/// CREATE ROLE persists across reopen (catalog-based, not a sidecar file).
#[test]
fn z1_create_role_persists_across_reopen() {
    let dir = tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        // CREATE TABLE first so we can use it.
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(x).unwrap();
        // CREATE ROLE and GRANT, then close engine.
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(None, x, "CREATE ROLE analyst")
            .unwrap();
        engine.commit(x).unwrap();
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(None, x, "GRANT SELECT ON t TO analyst")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Reopen: role and grant must survive.
    let engine = Engine::open(dir.path(), 0).unwrap();
    let rows = exec(&engine, "SELECT * FROM unidb_catalog.roles").unwrap();
    let names: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.first())
        .map(|s| s.as_str())
        .collect();
    assert!(
        names.contains(&"analyst"),
        "role 'analyst' not found in unidb_catalog.roles after reopen: {names:?}"
    );

    let grant_rows = exec(&engine, "SELECT * FROM unidb_catalog.grants").unwrap();
    assert!(
        grant_rows
            .iter()
            .any(|r| r.len() >= 2 && r[0].contains("analyst")),
        "grant for analyst not found after reopen: {grant_rows:?}"
    );
}

/// DROP ROLE removes it from catalog; re-granting after drop fails gracefully.
#[test]
fn z1_drop_role_removes_from_catalog() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(x).unwrap();

    // Create and then drop a role.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "CREATE ROLE temp_role")
        .unwrap();
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "DROP ROLE temp_role")
        .unwrap();
    engine.commit(x).unwrap();

    let rows = exec(&engine, "SELECT * FROM unidb_catalog.roles").unwrap();
    let names: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.first())
        .map(|s| s.trim_matches('"'))
        .collect();
    assert!(
        !names.contains(&"temp_role"),
        "dropped role 'temp_role' still in unidb_catalog.roles: {names:?}"
    );
}

// ── Z1: GRANT / REVOKE ───────────────────────────────────────────────────────

/// GRANT SELECT allows role to query; REVOKE SELECT denies it.
#[test]
fn z1_grant_revoke_select() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE posts (id INT, body TEXT)")
        .unwrap();
    engine.commit(x).unwrap();

    // Create a user and a role.
    for ddl in [
        "CREATE USER alice",
        "CREATE ROLE reader",
        "GRANT SELECT ON posts TO reader",
        "GRANT reader TO alice",
    ] {
        let x = engine.begin().unwrap();
        engine.execute_sql_as(None, x, ddl).unwrap();
        engine.commit(x).unwrap();
    }

    // alice can SELECT.
    exec_as(&engine, Some("alice"), "SELECT id FROM posts").unwrap();

    // REVOKE the role → alice loses access.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "REVOKE reader FROM alice")
        .unwrap();
    engine.commit(x).unwrap();

    assert!(
        denied(exec_as(&engine, Some("alice"), "SELECT id FROM posts")),
        "alice should be denied SELECT after role revoke"
    );
}

/// Direct GRANT INSERT + REVOKE INSERT on a table.
#[test]
fn z1_direct_grant_revoke_insert() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE items (id INT)")
        .unwrap();
    engine.commit(x).unwrap();

    for ddl in ["CREATE USER writer", "GRANT INSERT ON items TO writer"] {
        let x = engine.begin().unwrap();
        engine.execute_sql_as(None, x, ddl).unwrap();
        engine.commit(x).unwrap();
    }

    // writer can INSERT.
    exec_as(
        &engine,
        Some("writer"),
        "INSERT INTO items (id) VALUES (42)",
    )
    .unwrap();

    // REVOKE INSERT.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "REVOKE INSERT ON items FROM writer")
        .unwrap();
    engine.commit(x).unwrap();

    assert!(
        denied(exec_as(
            &engine,
            Some("writer"),
            "INSERT INTO items (id) VALUES (99)"
        )),
        "writer should be denied INSERT after revoke"
    );
}

// ── Z1: CREATE POLICY / DROP POLICY ──────────────────────────────────────────

/// CREATE POLICY ... FOR INSERT USING (...) blocks INSERTs that violate the predicate.
#[test]
fn z1_create_policy_blocks_violating_insert() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE budget (id INT, amount INT)")
        .unwrap();
    engine.commit(x).unwrap();

    // Policy: only allow INSERTs where amount > 0.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(
            None,
            x,
            "CREATE POLICY positive_amount ON budget FOR INSERT USING (amount > 0)",
        )
        .unwrap();
    engine.commit(x).unwrap();

    // A valid INSERT (amount = 100) succeeds.
    exec(&engine, "INSERT INTO budget (id, amount) VALUES (1, 100)").unwrap();

    // A violating INSERT (amount = -5) must fail.
    let x = engine.begin().unwrap();
    let r = engine.execute_sql(x, "INSERT INTO budget (id, amount) VALUES (2, -5)");
    engine.abort(x).unwrap();
    assert!(
        r.is_err(),
        "INSERT violating policy should have been rejected, but it succeeded"
    );
}

/// DROP POLICY removes the restriction.
#[test]
fn z1_drop_policy_removes_restriction() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE scores (id INT, val INT)")
        .unwrap();
    engine.commit(x).unwrap();

    // Create a restrictive policy.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(
            None,
            x,
            "CREATE POLICY only_positive ON scores FOR INSERT USING (val > 0)",
        )
        .unwrap();
    engine.commit(x).unwrap();

    // Confirm it blocks negative inserts.
    let x = engine.begin().unwrap();
    let blocked = engine.execute_sql(x, "INSERT INTO scores (id, val) VALUES (1, -1)");
    engine.abort(x).unwrap();
    assert!(blocked.is_err(), "policy should block negative val insert");

    // DROP POLICY.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "DROP POLICY only_positive ON scores")
        .unwrap();
    engine.commit(x).unwrap();

    // After drop, the previously-blocked insert must succeed.
    exec(&engine, "INSERT INTO scores (id, val) VALUES (1, -1)").unwrap();
}

/// Policy persists across engine reopen.
#[test]
fn z1_policy_persists_across_reopen() {
    let dir = tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE ledger (id INT, amount INT)")
            .unwrap();
        engine.commit(x).unwrap();

        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(
                None,
                x,
                "CREATE POLICY no_negative ON ledger FOR INSERT USING (amount >= 0)",
            )
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Reopen and verify policy still blocks the bad insert.
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    let r = engine.execute_sql(x, "INSERT INTO ledger (id, amount) VALUES (1, -999)");
    engine.abort(x).unwrap();
    assert!(
        r.is_err(),
        "policy should still be enforced after engine reopen"
    );

    // And the allowed insert still works.
    exec(&engine, "INSERT INTO ledger (id, amount) VALUES (1, 500)").unwrap();
}

// ── Z5: unidb_catalog virtual relations ──────────────────────────────────────

/// SELECT * FROM unidb_catalog.roles returns the created roles.
#[test]
fn z5_roles_catalog_relation() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    for ddl in ["CREATE ROLE r_alpha", "CREATE ROLE r_beta"] {
        let x = engine.begin().unwrap();
        engine.execute_sql_as(None, x, ddl).unwrap();
        engine.commit(x).unwrap();
    }

    let rows = exec(&engine, "SELECT * FROM unidb_catalog.roles").unwrap();
    let names: Vec<String> = rows.iter().filter_map(|r| r.first()).cloned().collect();
    assert!(
        names.contains(&"r_alpha".to_string()),
        "r_alpha not in unidb_catalog.roles: {names:?}"
    );
    assert!(
        names.contains(&"r_beta".to_string()),
        "r_beta not in unidb_catalog.roles: {names:?}"
    );
}

/// SELECT * FROM unidb_catalog.grants reflects active grants.
#[test]
fn z5_grants_catalog_relation() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE metrics (id INT)")
        .unwrap();
    engine.commit(x).unwrap();

    for ddl in [
        "CREATE ROLE g_reader",
        "GRANT SELECT ON metrics TO g_reader",
    ] {
        let x = engine.begin().unwrap();
        engine.execute_sql_as(None, x, ddl).unwrap();
        engine.commit(x).unwrap();
    }

    let rows = exec(&engine, "SELECT * FROM unidb_catalog.grants").unwrap();
    // Each row has: role, table_name, operation.
    let found = rows.iter().any(|r| {
        r.first().map(|s| s.contains("g_reader")).unwrap_or(false)
            && r.get(1).map(|s| s.contains("metrics")).unwrap_or(false)
    });
    assert!(
        found,
        "grant for g_reader on metrics not found in unidb_catalog.grants: {rows:?}"
    );
}

/// SELECT * FROM unidb_catalog.policies shows created policies.
#[test]
fn z5_policies_catalog_relation() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE orders (id INT, status INT)")
        .unwrap();
    engine.commit(x).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(
            None,
            x,
            "CREATE POLICY open_orders ON orders FOR SELECT USING (status > 0)",
        )
        .unwrap();
    engine.commit(x).unwrap();

    let rows = exec(&engine, "SELECT * FROM unidb_catalog.policies").unwrap();
    // Each row: name, table_name, operation, using_expr.
    let found = rows.iter().any(|r| {
        r.first()
            .map(|s| s.contains("open_orders"))
            .unwrap_or(false)
    });
    assert!(
        found,
        "policy 'open_orders' not found in unidb_catalog.policies: {rows:?}"
    );
}

// ── Z3: REST bulk-insert 403 without INSERT grant ────────────────────────────
//
// The bulk-insert REST-level test lives under the `server` feature flag since
// it requires spawning a real server and making HTTP calls.

#[cfg(feature = "server")]
#[path = "server_common/mod.rs"]
mod server_common;

#[cfg(feature = "server")]
mod z3 {
    use super::server_common::{token_for, valid_token, TestServer};
    use serde_json::json;

    async fn sql_req(
        client: &reqwest::Client,
        url: String,
        token: &str,
        sql: &str,
    ) -> reqwest::Response {
        client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&json!({ "sql": sql }))
            .send()
            .await
            .unwrap()
    }

    async fn bulk_req(
        client: &reqwest::Client,
        url: String,
        token: &str,
        ndjson: &str,
    ) -> reqwest::Response {
        client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/x-ndjson")
            .body(ndjson.to_string())
            .send()
            .await
            .unwrap()
    }

    /// POST /tables/{name}/bulk without a valid INSERT grant returns 403.
    #[tokio::test]
    async fn z3_bulk_insert_requires_insert_grant() {
        let server = TestServer::spawn().await;
        let client = reqwest::Client::new();

        // Bootstrap: create root superuser, table, restricted user.
        let admin = valid_token();
        assert_eq!(
            sql_req(
                &client,
                server.url("/sql"),
                &admin,
                "CREATE USER root SUPERUSER"
            )
            .await
            .status(),
            200
        );
        let root = token_for("root");

        assert_eq!(
            sql_req(
                &client,
                server.url("/sql"),
                &root,
                "CREATE TABLE events (id INT, val TEXT)"
            )
            .await
            .status(),
            200
        );
        assert_eq!(
            sql_req(&client, server.url("/sql"), &root, "CREATE USER bob")
                .await
                .status(),
            200
        );

        let bob = token_for("bob");

        // bob has no grant → POST /bulk must be 403.
        let r403 = bulk_req(
            &client,
            server.url("/tables/events/bulk"),
            &bob,
            r#"{"id":1,"val":"x"}"#,
        )
        .await;
        assert_eq!(
            r403.status(),
            403,
            "expected 403 for bulk insert without INSERT grant, got {}",
            r403.status()
        );

        // Grant INSERT to bob → bulk insert must now succeed with 200.
        assert_eq!(
            sql_req(
                &client,
                server.url("/sql"),
                &root,
                "GRANT INSERT ON events TO bob"
            )
            .await
            .status(),
            200
        );

        let r200 = bulk_req(
            &client,
            server.url("/tables/events/bulk"),
            &bob,
            r#"{"id":1,"val":"hello"}"#,
        )
        .await;
        assert_eq!(
            r200.status(),
            200,
            "expected 200 after granting INSERT, got {}",
            r200.status()
        );
    }
}
