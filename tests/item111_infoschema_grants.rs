// Item 111 — information_schema visibility follows existing table grants.
//
// Postgres semantics: the information_schema views need no grant of their
// own; a table's rows appear for a caller iff the caller holds ANY privilege
// on that table. A blanket view grant (the old workaround) is no longer
// needed — and the old unfiltered behavior, which would have revealed every
// table's existence to any view-grantee, is gone.

use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn setup(engine: &Engine) {
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE TABLE projects (id INT, name TEXT)",
        "CREATE TABLE secrets (id INT, payload TEXT)",
        "CREATE USER bob",
        "GRANT SELECT ON projects TO bob",
        "GRANT UPDATE ON projects TO bob",
        "CREATE USER nobody",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();
}

fn rows_as(engine: &Engine, user: Option<&str>, sql: &str) -> Vec<Vec<Literal>> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql_as(user, x, sql).unwrap();
    engine.commit(x).unwrap();
    match res.into_iter().next().unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// `table_name` values from information_schema.tables rows (column index 2).
fn table_names(rows: &[Vec<Literal>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[2] {
            Literal::Text(s) => s.clone(),
            other => panic!("table_name not text: {other:?}"),
        })
        .collect()
}

/// The filing's repro: a user with grants on one table reads the views with
/// NO separate information_schema grant, and sees exactly that table.
#[test]
fn grants_make_tables_visible_without_view_grant() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);

    let rows = rows_as(
        &engine,
        Some("bob"),
        "SELECT * FROM information_schema.tables",
    );
    assert_eq!(
        table_names(&rows),
        vec!["projects".to_string()],
        "bob must see exactly the table he holds a privilege on"
    );

    // columns: only projects' columns (2), never secrets'.
    let cols = rows_as(
        &engine,
        Some("bob"),
        "SELECT * FROM information_schema.columns",
    );
    assert_eq!(cols.len(), 2, "only projects' two columns visible");
}

/// Zero grants → zero rows (not an error, and not the whole schema).
#[test]
fn no_grants_sees_nothing_but_no_error() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    let rows = rows_as(
        &engine,
        Some("nobody"),
        "SELECT * FROM information_schema.tables",
    );
    assert!(
        rows.is_empty(),
        "a user with no grants must not discover any table"
    );
}

/// Superuser and bootstrap/open mode see everything (unchanged behavior).
#[test]
fn superuser_and_open_mode_see_everything() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    let rows = rows_as(&engine, None, "SELECT * FROM information_schema.tables");
    assert_eq!(table_names(&rows), vec!["projects", "secrets"]);

    // Open mode: a fresh engine with tables but NO users registered.
    let dir2 = tempdir().unwrap();
    let e2 = Engine::open(dir2.path(), 0).unwrap();
    let x = e2.begin().unwrap();
    e2.execute_sql_as(None, x, "CREATE TABLE open_t (id INT)")
        .unwrap();
    e2.commit(x).unwrap();
    let rows = rows_as(
        &e2,
        Some("anyone"),
        "SELECT * FROM information_schema.tables",
    );
    assert_eq!(table_names(&rows), vec!["open_t"]);
}

/// Constraint-shaped views follow the same visibility (same leak shape).
#[test]
fn constraint_views_follow_grants() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE TABLE parents (id INT PRIMARY KEY)",
        "CREATE TABLE children (id INT PRIMARY KEY, pid INT REFERENCES parents(id))",
        "CREATE USER carol",
        "GRANT SELECT ON children TO carol",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();

    let rows = rows_as(
        &engine,
        Some("carol"),
        "SELECT * FROM information_schema.table_constraints",
    );
    // carol sees children's constraints only — none of parents'.
    for r in &rows {
        if let Literal::Text(tname) = &r[4] {
            assert_ne!(tname, "parents", "parents' constraints must be hidden");
        }
    }
    assert!(
        !rows.is_empty(),
        "children's own constraints must be visible"
    );
}

/// unidb_catalog.* keeps its grant-gated access model (item-24 Z5 unchanged):
/// a non-superuser without a grant on the catalog view is still denied.
#[test]
fn unidb_catalog_stays_grant_gated() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    let x = engine.begin().unwrap();
    let res = engine.execute_sql_as(Some("bob"), x, "SELECT * FROM unidb_catalog.indexes");
    engine.commit(x).unwrap();
    assert!(
        res.is_err(),
        "unidb_catalog.* must still require its own grant"
    );
}
