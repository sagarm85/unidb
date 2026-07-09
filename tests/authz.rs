// P6.e — users / roles / GRANT, enforced through `execute_sql_as`.

use tempfile::tempdir;
use unidb::{DbError, Engine};

fn denied<T: std::fmt::Debug>(r: unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::PermissionDenied(_)))
}

#[test]
fn grants_and_privilege_enforcement() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Superuser (embedded, None) sets up the schema and a user.
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE accounts (id INT, balance INT)")
        .unwrap();
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, "CREATE USER bob").unwrap();
    engine.commit(x).unwrap();

    // bob has no privileges yet → SELECT denied.
    let x = engine.begin().unwrap();
    assert!(denied(engine.execute_sql_as(
        Some("bob"),
        x,
        "SELECT id FROM accounts"
    )));
    engine.abort(x).unwrap();

    // Grant SELECT → bob can read, but still not write.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "GRANT SELECT ON accounts TO bob")
        .unwrap();
    engine.commit(x).unwrap();

    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("bob"), x, "SELECT id FROM accounts")
        .unwrap();
    engine.commit(x).unwrap();

    let x = engine.begin().unwrap();
    assert!(denied(engine.execute_sql_as(
        Some("bob"),
        x,
        "INSERT INTO accounts (id, balance) VALUES (1, 100)"
    )));
    engine.abort(x).unwrap();

    // A named non-superuser cannot run auth DDL or schema DDL.
    let x = engine.begin().unwrap();
    assert!(denied(engine.execute_sql_as(
        Some("bob"),
        x,
        "CREATE USER carol"
    )));
    engine.abort(x).unwrap();
    let x = engine.begin().unwrap();
    assert!(denied(engine.execute_sql_as(
        Some("bob"),
        x,
        "CREATE TABLE t2 (id INT)"
    )));
    engine.abort(x).unwrap();
}

#[test]
fn role_grants_inherited_and_persist() {
    let dir = tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(x).unwrap();
        // Role that can read `t`, granted to alice.
        for ddl in [
            "CREATE USER alice",
            "CREATE ROLE reader",
            "GRANT SELECT ON t TO reader",
            "GRANT reader TO alice",
        ] {
            let x = engine.begin().unwrap();
            engine.execute_sql_as(None, x, ddl).unwrap();
            engine.commit(x).unwrap();
        }
        let x = engine.begin().unwrap();
        engine
            .execute_sql_as(Some("alice"), x, "SELECT id FROM t")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // Reopen: roles/grants persist, so alice still reads `t`.
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("alice"), x, "SELECT id FROM t")
        .unwrap();
    engine.commit(x).unwrap();

    // REVOKE the role → alice loses access.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "REVOKE reader FROM alice")
        .unwrap();
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    assert!(denied(engine.execute_sql_as(
        Some("alice"),
        x,
        "SELECT id FROM t"
    )));
    engine.abort(x).unwrap();
}

// A named SUPERUSER can administer + read/write everything.
#[test]
fn named_superuser_has_full_access() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "CREATE USER root SUPERUSER")
        .unwrap();
    engine.commit(x).unwrap();

    // root can create schema, other users, and read/write.
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("root"), x, "CREATE TABLE t (id INT)")
        .unwrap();
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("root"), x, "INSERT INTO t (id) VALUES (1)")
        .unwrap();
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("root"), x, "CREATE USER dave")
        .unwrap();
    engine.commit(x).unwrap();
}
