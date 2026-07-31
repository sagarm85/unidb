// Item 126 (Workstream I4) — SQL schema-migrations tooling acceptance tests.
// See `src/migrations.rs` for the module doc (algorithm, naming convention,
// and the verified non-transactional-DDL caveat these tests also probe).

use std::fs;
use std::path::Path;

use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::{DbError, Engine};

fn write_migration(dir: &Path, filename: &str, sql: &str) {
    fs::write(dir.join(filename), sql).unwrap();
}

/// Run a read-only SQL statement and return its first result's rows.
fn query_rows(engine: &Engine, sql: &str) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, sql);
    match res {
        Ok(mut results) => {
            engine.commit(xid).unwrap();
            match results.pop() {
                Some(ExecResult::Rows { rows, .. }) => Ok(rows),
                other => panic!("expected Rows, got {other:?}"),
            }
        }
        Err(e) => {
            let _ = engine.abort(xid);
            Err(e)
        }
    }
}

fn table_exists(engine: &Engine, table: &str) -> bool {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, &format!("SELECT * FROM {table}"));
    let exists = !matches!(res, Err(DbError::TableNotFound(_)));
    match res {
        Ok(_) => {
            engine.commit(xid).unwrap();
        }
        Err(_) => {
            let _ = engine.abort(xid);
        }
    }
    exists
}

fn text(lit: &Literal) -> String {
    match lit {
        Literal::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// (a) Applying a set of migrations creates the objects + populates
// `schema_migrations`.
#[test]
fn applying_migrations_creates_objects_and_populates_tracking_table() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    write_migration(
        mig_dir.path(),
        "0001_init.sql",
        "CREATE TABLE widgets (id INT PRIMARY KEY, name TEXT);",
    );
    write_migration(
        mig_dir.path(),
        "0002_add_gadgets.sql",
        "CREATE TABLE gadgets (id INT PRIMARY KEY);",
    );

    let engine = Engine::open(data_dir.path(), 0).unwrap();
    let report = engine.apply_migrations(mig_dir.path()).unwrap();

    assert_eq!(report.applied, vec!["0001".to_string(), "0002".to_string()]);
    assert_eq!(report.skipped, 0);

    // The objects exist.
    assert!(table_exists(&engine, "widgets"));
    assert!(table_exists(&engine, "gadgets"));

    // The tracking table is populated.
    let rows = query_rows(
        &engine,
        "SELECT version, name, checksum FROM schema_migrations",
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let mut versions: Vec<String> = rows.iter().map(|r| text(&r[0])).collect();
    versions.sort();
    assert_eq!(versions, vec!["0001".to_string(), "0002".to_string()]);
    for row in &rows {
        // checksum is a 64-char lower-hex SHA-256 digest.
        let checksum = text(&row[2]);
        assert_eq!(checksum.len(), 64);
        assert!(checksum.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

// (b) Re-running is a no-op (idempotent, skipped count correct).
#[test]
fn rerunning_apply_migrations_is_idempotent() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    write_migration(
        mig_dir.path(),
        "0001_init.sql",
        "CREATE TABLE widgets (id INT PRIMARY KEY);",
    );
    write_migration(
        mig_dir.path(),
        "0002_more.sql",
        "CREATE TABLE gadgets (id INT PRIMARY KEY);",
    );

    let engine = Engine::open(data_dir.path(), 0).unwrap();
    let first = engine.apply_migrations(mig_dir.path()).unwrap();
    assert_eq!(first.applied.len(), 2);
    assert_eq!(first.skipped, 0);

    let second = engine.apply_migrations(mig_dir.path()).unwrap();
    assert!(
        second.applied.is_empty(),
        "expected no new migrations applied"
    );
    assert_eq!(second.skipped, 2);

    // Tracking table still has exactly 2 rows (no duplicate inserts).
    let rows = query_rows(&engine, "SELECT version FROM schema_migrations").unwrap();
    assert_eq!(rows.len(), 2);
}

// (c) Editing an applied migration -> checksum-drift error.
#[test]
fn editing_an_applied_migration_causes_checksum_drift_error() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    write_migration(
        mig_dir.path(),
        "0001_init.sql",
        "CREATE TABLE widgets (id INT PRIMARY KEY);",
    );

    let engine = Engine::open(data_dir.path(), 0).unwrap();
    let report = engine.apply_migrations(mig_dir.path()).unwrap();
    assert_eq!(report.applied, vec!["0001".to_string()]);

    // Edit the already-applied file.
    write_migration(
        mig_dir.path(),
        "0001_init.sql",
        "CREATE TABLE widgets (id INT PRIMARY KEY, extra TEXT);",
    );

    let err = engine.apply_migrations(mig_dir.path()).unwrap_err();
    match &err {
        DbError::Migration(msg) => {
            assert!(msg.contains("checksum drift") || msg.contains("checksum"));
            assert!(msg.contains("0001"));
        }
        other => panic!("expected DbError::Migration, got {other:?}"),
    }
}

// (d) Versions applied in order.
#[test]
fn migrations_apply_in_ascending_version_order_even_when_dependent() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    // 0002 depends on the table 0001 creates; if applied out of order this
    // would fail with TableNotFound. Also add a third, "0010", to check
    // lexicographic-not-numeric-surprise ordering isn't accidentally relied
    // on beyond what the module doc promises (zero-padded here, so it's
    // still correct: "0002" < "0010" lexicographically).
    write_migration(
        mig_dir.path(),
        "0001_create_orders.sql",
        "CREATE TABLE orders (id INT PRIMARY KEY, total INT);",
    );
    write_migration(
        mig_dir.path(),
        "0002_seed_orders.sql",
        "INSERT INTO orders (id, total) VALUES (1, 100);",
    );
    write_migration(
        mig_dir.path(),
        "0010_more_orders.sql",
        "INSERT INTO orders (id, total) VALUES (2, 200);",
    );

    let engine = Engine::open(data_dir.path(), 0).unwrap();
    let report = engine.apply_migrations(mig_dir.path()).unwrap();
    assert_eq!(
        report.applied,
        vec!["0001".to_string(), "0002".to_string(), "0010".to_string()]
    );

    let rows = query_rows(&engine, "SELECT id FROM orders").unwrap();
    assert_eq!(rows.len(), 2);
}

// (e) A migration with a bad statement stops and is NOT recorded, and
// earlier ones stay applied. Also verifies the empirically-confirmed
// intra-file atomicity: the failing file's own partial DDL is rolled back
// (unidb's request-scoped DDL rollback covers every statement of one
// `execute_sql` call), so nothing from the bad file leaks either.
#[test]
fn bad_migration_stops_is_not_recorded_and_earlier_ones_stay_applied() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    write_migration(
        mig_dir.path(),
        "0001_good.sql",
        "CREATE TABLE good_table (id INT PRIMARY KEY);",
    );
    // The second statement is invalid SQL, so the whole file fails.
    write_migration(
        mig_dir.path(),
        "0002_bad.sql",
        "CREATE TABLE leftover (id INT PRIMARY KEY); THIS IS NOT VALID SQL AT ALL;",
    );
    write_migration(
        mig_dir.path(),
        "0003_unreached.sql",
        "CREATE TABLE never_created (id INT PRIMARY KEY);",
    );

    let engine = Engine::open(data_dir.path(), 0).unwrap();
    let err = engine.apply_migrations(mig_dir.path()).unwrap_err();
    assert!(
        matches!(err, DbError::Migration(_)),
        "expected DbError::Migration, got {err:?}"
    );

    // 0001 stayed applied.
    assert!(table_exists(&engine, "good_table"));
    let rows = query_rows(&engine, "SELECT version FROM schema_migrations").unwrap();
    let versions: Vec<String> = rows.iter().map(|r| text(&r[0])).collect();
    assert_eq!(versions, vec!["0001".to_string()]);

    // 0002 was not recorded, and its own partial DDL did not leak (the
    // intra-file rollback covers the file's own earlier statement).
    assert!(!table_exists(&engine, "leftover"));

    // 0003 was never reached.
    assert!(!table_exists(&engine, "never_created"));

    // Fixing 0002 and re-running picks up where it left off.
    write_migration(
        mig_dir.path(),
        "0002_bad.sql",
        "CREATE TABLE leftover (id INT PRIMARY KEY);",
    );
    let report = engine.apply_migrations(mig_dir.path()).unwrap();
    assert_eq!(report.applied, vec!["0002".to_string(), "0003".to_string()]);
    assert!(table_exists(&engine, "leftover"));
    assert!(table_exists(&engine, "never_created"));
}

// Bonus: a bad migration filename is rejected up front (before anything is
// applied), and duplicate versions are rejected too.
#[test]
fn malformed_filenames_and_duplicate_versions_are_rejected() {
    let data_dir = tempdir().unwrap();
    let mig_dir = tempdir().unwrap();
    write_migration(mig_dir.path(), "noversion.sql", "SELECT 1;");
    let engine = Engine::open(data_dir.path(), 0).unwrap();
    assert!(matches!(
        engine.apply_migrations(mig_dir.path()),
        Err(DbError::Migration(_))
    ));

    let mig_dir2 = tempdir().unwrap();
    write_migration(mig_dir2.path(), "0001_a.sql", "SELECT 1;");
    write_migration(mig_dir2.path(), "0001_b.sql", "SELECT 1;");
    assert!(matches!(
        engine.apply_migrations(mig_dir2.path()),
        Err(DbError::Migration(_))
    ));
}
