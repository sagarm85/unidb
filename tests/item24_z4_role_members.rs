// Item-24 Z4: unidb_catalog.role_members + unidb_catalog.users virtual tables.
//
// Z4 exposes role membership and user metadata via SQL-queryable catalog
// relations. Column-level grants are explicitly deferred.

use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, Engine};

// ── helpers ──────────────────────────────────────────────────────────────────

fn lit_str(v: &Literal) -> String {
    match v {
        Literal::Text(s) => s.clone(),
        Literal::Int(i) => i.to_string(),
        Literal::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute `sql` as the embedded superuser. Returns all result rows with each
/// cell as a plain string.
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

/// Execute `ddl` as superuser (None = embedded superuser).
fn ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// GRANT <role> TO <user> shows up in unidb_catalog.role_members.
#[test]
fn role_members_catalog_shows_grants() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE ROLE analyst");
    ddl(&engine, "CREATE USER bob");
    ddl(&engine, "GRANT analyst TO bob");

    let rows = exec(
        &engine,
        "SELECT role, member FROM unidb_catalog.role_members",
    )
    .unwrap();
    let found = rows.iter().any(|r| {
        r.get(0).map(|s| s == "analyst").unwrap_or(false)
            && r.get(1).map(|s| s == "bob").unwrap_or(false)
    });
    assert!(
        found,
        "expected (analyst, bob) in unidb_catalog.role_members, got: {rows:?}"
    );
}

/// unidb_catalog.users lists all created users.
#[test]
fn users_catalog_shows_all_users() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE USER alice SUPERUSER");
    ddl(&engine, "CREATE USER bob");

    let rows = exec(&engine, "SELECT name FROM unidb_catalog.users").unwrap();
    let names: Vec<String> = rows.iter().filter_map(|r| r.first()).cloned().collect();
    assert!(
        names.contains(&"alice".to_string()),
        "alice not in unidb_catalog.users: {names:?}"
    );
    assert!(
        names.contains(&"bob".to_string()),
        "bob not in unidb_catalog.users: {names:?}"
    );
}

/// The `is_superuser` column is `true` for SUPERUSER users and `false` for regular users.
#[test]
fn users_catalog_superuser_flag() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE USER alice SUPERUSER");
    ddl(&engine, "CREATE USER bob");

    let rows = exec(
        &engine,
        "SELECT name, is_superuser FROM unidb_catalog.users",
    )
    .unwrap();

    // Find alice's row and check is_superuser is truthy.
    let alice_row = rows
        .iter()
        .find(|r| r.first().map(|s| s == "alice").unwrap_or(false));
    assert!(alice_row.is_some(), "alice row not found: {rows:?}");
    let alice_flag = alice_row.unwrap().get(1).cloned().unwrap_or_default();
    assert!(
        alice_flag == "true" || alice_flag == "1",
        "alice should have is_superuser=true, got: {alice_flag:?}"
    );

    // Find bob's row and check is_superuser is falsy.
    let bob_row = rows
        .iter()
        .find(|r| r.first().map(|s| s == "bob").unwrap_or(false));
    assert!(bob_row.is_some(), "bob row not found: {rows:?}");
    let bob_flag = bob_row.unwrap().get(1).cloned().unwrap_or_default();
    assert!(
        bob_flag == "false" || bob_flag == "0",
        "bob should have is_superuser=false, got: {bob_flag:?}"
    );
}

/// unidb_catalog.role_members returns 0 rows when no memberships have been granted.
#[test]
fn role_members_empty_when_no_memberships() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Create roles and users but don't grant any memberships.
    ddl(&engine, "CREATE ROLE r1");
    ddl(&engine, "CREATE USER u1");

    let rows = exec(
        &engine,
        "SELECT role, member FROM unidb_catalog.role_members",
    )
    .unwrap();
    assert!(
        rows.is_empty(),
        "expected 0 rows in role_members when no memberships granted, got: {rows:?}"
    );
}

/// Multiple users granted the same role all appear in unidb_catalog.role_members.
#[test]
fn role_members_multiple_grants() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    ddl(&engine, "CREATE ROLE r1");
    ddl(&engine, "CREATE USER u1");
    ddl(&engine, "CREATE USER u2");
    ddl(&engine, "GRANT r1 TO u1");
    ddl(&engine, "GRANT r1 TO u2");

    let rows = exec(
        &engine,
        "SELECT role, member FROM unidb_catalog.role_members",
    )
    .unwrap();

    let has_u1 = rows.iter().any(|r| {
        r.get(0).map(|s| s == "r1").unwrap_or(false) && r.get(1).map(|s| s == "u1").unwrap_or(false)
    });
    let has_u2 = rows.iter().any(|r| {
        r.get(0).map(|s| s == "r1").unwrap_or(false) && r.get(1).map(|s| s == "u2").unwrap_or(false)
    });

    assert!(
        has_u1,
        "expected (r1, u1) in unidb_catalog.role_members: {rows:?}"
    );
    assert!(
        has_u2,
        "expected (r1, u2) in unidb_catalog.role_members: {rows:?}"
    );
}
