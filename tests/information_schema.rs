//! Milestone 18, Epic C — system-catalog introspection (`information_schema.*`,
//! `unidb_catalog.*`) as synthesized virtual relations queried over the ordinary
//! SQL surface. Proves C1 (tables/columns), C2 (table_constraints/
//! key_column_usage), C3 (referential_constraints), and C4 (unidb_catalog.
//! indexes), including the spec's worked-example ERD queries running over a
//! schema with **composite** primary and foreign keys and returning correctly
//! paired PK/FK rows.

use tempfile::tempdir;
use unidb::error::DbError;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn fresh() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run one committed statement.
fn run(engine: &Engine, sql: &str) -> Result<Vec<ExecResult>, DbError> {
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

/// Run a query and return `(columns, rows-as-strings)` for easy assertions.
/// Every `Literal` is rendered to a canonical string so tests read cleanly.
fn query(engine: &Engine, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let results = run(engine, sql).expect("query failed");
    match results.into_iter().next().expect("one result") {
        ExecResult::Rows { columns, rows } => {
            let srows = rows
                .into_iter()
                .map(|r| r.iter().map(lit_str).collect::<Vec<_>>())
                .collect();
            (columns, srows)
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn lit_str(l: &Literal) -> String {
    match l {
        Literal::Text(s) | Literal::Json(s) => s.clone(),
        Literal::Int(i) => i.to_string(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

/// A schema with real composite keys, exercised by most tests below:
/// `orders(region, order_no)` is a composite PK; `line_items(o_region,
/// o_order_no)` is a composite FK back to it.
fn seed_erd(engine: &Engine) {
    run(
        engine,
        "CREATE TABLE orders (
            region TEXT NOT NULL,
            order_no INT NOT NULL,
            customer TEXT,
            PRIMARY KEY (region, order_no)
        )",
    )
    .unwrap();
    run(
        engine,
        "CREATE TABLE line_items (
            id INT PRIMARY KEY,
            o_region TEXT,
            o_order_no INT,
            sku TEXT NOT NULL,
            qty INT DEFAULT 1,
            FOREIGN KEY (o_region, o_order_no) REFERENCES orders (region, order_no)
        )",
    )
    .unwrap();
}

#[test]
fn tables_lists_user_tables_not_internal() {
    let (engine, _d) = fresh();
    run(&engine, "CREATE TABLE alpha (id INT)").unwrap();
    run(&engine, "CREATE TABLE beta (id INT)").unwrap();

    let (cols, rows) = query(
        &engine,
        "SELECT table_name, table_type, table_schema FROM information_schema.tables",
    );
    assert_eq!(cols, vec!["table_name", "table_type", "table_schema"]);
    let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"], "sorted, user tables only");
    assert!(rows.iter().all(|r| r[1] == "BASE TABLE"));
    assert!(rows.iter().all(|r| r[2] == "public"));
    // No engine-internal `__…__` tables leak in.
    assert!(!names.iter().any(|n| n.starts_with("__")));
}

#[test]
fn columns_reports_type_nullability_ordinal_default() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (cols, rows) = query(
        &engine,
        "SELECT column_name, data_type, is_nullable, ordinal_position, column_default \
         FROM information_schema.columns WHERE table_name = 'line_items'",
    );
    assert_eq!(
        cols,
        vec![
            "column_name",
            "data_type",
            "is_nullable",
            "ordinal_position",
            "column_default"
        ]
    );
    // Ordinal is 1-based in declaration order.
    let by_name: std::collections::HashMap<&str, &Vec<String>> =
        rows.iter().map(|r| (r[0].as_str(), r)).collect();
    let id = by_name["id"];
    assert_eq!(id[1], "bigint");
    assert_eq!(id[2], "NO"); // PRIMARY KEY ⇒ NOT NULL
    assert_eq!(id[3], "1");
    assert_eq!(id[4], "NULL"); // no default
    let sku = by_name["sku"];
    assert_eq!(sku[1], "text");
    assert_eq!(sku[2], "NO"); // NOT NULL
    let qty = by_name["qty"];
    assert_eq!(qty[2], "YES");
    assert_eq!(qty[4], "1"); // DEFAULT 1
    assert_eq!(qty[3], "5"); // 5th declared column
}

#[test]
fn table_constraints_reports_pk_and_fk_types() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (_c, rows) = query(
        &engine,
        "SELECT constraint_name, constraint_type, table_name \
         FROM information_schema.table_constraints",
    );
    let has = |name: &str, ty: &str, table: &str| {
        rows.iter()
            .any(|r| r[0] == name && r[1] == ty && r[2] == table)
    };
    assert!(has("orders_pkey", "PRIMARY KEY", "orders"));
    assert!(has("line_items_pkey", "PRIMARY KEY", "line_items"));
    assert!(has(
        "line_items_o_region_o_order_no_fkey",
        "FOREIGN KEY",
        "line_items"
    ));
}

#[test]
fn key_column_usage_orders_composite_pk_columns() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (_c, rows) = query(
        &engine,
        "SELECT column_name, ordinal_position FROM information_schema.key_column_usage \
         WHERE constraint_name = 'orders_pkey' ORDER BY ordinal_position",
    );
    assert_eq!(
        rows,
        vec![
            vec!["region".to_string(), "1".to_string()],
            vec!["order_no".to_string(), "2".to_string()],
        ]
    );
}

#[test]
fn referential_constraints_links_fk_to_referenced_pk() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (_c, rows) = query(
        &engine,
        "SELECT constraint_name, unique_constraint_name, update_rule, delete_rule \
         FROM information_schema.referential_constraints",
    );
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r[0], "line_items_o_region_o_order_no_fkey");
    assert_eq!(r[1], "orders_pkey"); // points at the referenced PK
    assert_eq!(r[2], "NO ACTION");
    assert_eq!(r[3], "NO ACTION");
}

/// The spec's worked-example ERD foreign-key query (explicit-ON form, per the
/// design note) run over composite keys must return each FK column correctly
/// *paired* with its referenced column, in order.
#[test]
fn worked_example_fk_join_pairs_composite_columns() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (cols, mut rows) = query(
        &engine,
        "SELECT tc.table_name AS from_table, kcu.column_name AS from_col, \
                ccu.table_name AS to_table, ccu.column_name AS to_col \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
              ON kcu.constraint_name = tc.constraint_name \
         JOIN information_schema.referential_constraints rc \
              ON rc.constraint_name = tc.constraint_name \
         JOIN information_schema.key_column_usage ccu \
              ON ccu.constraint_name = rc.unique_constraint_name \
             AND ccu.ordinal_position = kcu.position_in_unique_constraint \
         WHERE tc.constraint_type = 'FOREIGN KEY'",
    );
    assert_eq!(cols, vec!["from_table", "from_col", "to_table", "to_col"]);
    rows.sort();
    let mut expected = vec![
        vec![
            "line_items".to_string(),
            "o_order_no".to_string(),
            "orders".to_string(),
            "order_no".to_string(),
        ],
        vec![
            "line_items".to_string(),
            "o_region".to_string(),
            "orders".to_string(),
            "region".to_string(),
        ],
    ];
    expected.sort();
    assert_eq!(
        rows, expected,
        "composite FK columns paired to their referents"
    );
}

/// The spec's ERD FK query in its **original `USING (constraint_name)` form** —
/// now that the planner desugars `JOIN … USING`, it runs verbatim (composite
/// keys still pair correctly via the explicit ordinal-alignment conjunct).
#[test]
fn worked_example_fk_join_using_form_runs() {
    let (engine, _d) = fresh();
    seed_erd(&engine);

    let (_c, mut rows) = query(
        &engine,
        "SELECT tc.table_name AS from_table, kcu.column_name AS from_col, \
                ccu.table_name AS to_table, ccu.column_name AS to_col \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu USING (constraint_name) \
         JOIN information_schema.referential_constraints rc USING (constraint_name) \
         JOIN information_schema.key_column_usage ccu \
              ON ccu.constraint_name = rc.unique_constraint_name \
             AND ccu.ordinal_position = kcu.position_in_unique_constraint \
         WHERE tc.constraint_type = 'FOREIGN KEY'",
    );
    rows.sort();
    let mut expected = vec![
        vec![
            "line_items".to_string(),
            "o_order_no".to_string(),
            "orders".to_string(),
            "order_no".to_string(),
        ],
        vec![
            "line_items".to_string(),
            "o_region".to_string(),
            "orders".to_string(),
            "region".to_string(),
        ],
    ];
    expected.sort();
    assert_eq!(rows, expected);
}

#[test]
fn worked_example_columns_query_runs() {
    let (engine, _d) = fresh();
    seed_erd(&engine);
    // The spec's first worked-example query verbatim (single-table path).
    let (_c, rows) = query(
        &engine,
        "SELECT table_name, column_name, data_type, is_nullable, ordinal_position \
         FROM information_schema.columns WHERE table_schema = 'public'",
    );
    // orders(3) + line_items(5) = 8 visible columns.
    assert_eq!(rows.len(), 8);
}

#[test]
fn unidb_catalog_indexes_lists_indexed_columns() {
    let (engine, _d) = fresh();
    run(&engine, "CREATE TABLE t (id INT, name TEXT)").unwrap();
    run(&engine, "CREATE INDEX ON t USING btree (id)").unwrap();

    let (cols, rows) = query(
        &engine,
        "SELECT table_name, column_name, index_type, is_unique \
         FROM unidb_catalog.indexes",
    );
    assert_eq!(
        cols,
        vec!["table_name", "column_name", "index_type", "is_unique"]
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "t");
    assert_eq!(rows[0][1], "id");
    assert_eq!(rows[0][2], "btree");
}

#[test]
fn column_level_unique_and_check_appear_as_constraints() {
    let (engine, _d) = fresh();
    run(
        &engine,
        "CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE, age INT CHECK (age >= 0))",
    )
    .unwrap();

    let (_c, rows) = query(
        &engine,
        "SELECT constraint_type, constraint_name FROM information_schema.table_constraints \
         WHERE table_name = 'u'",
    );
    let types: std::collections::HashSet<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert!(types.contains("PRIMARY KEY"));
    assert!(types.contains("UNIQUE"));
    assert!(types.contains("CHECK"));
    // The PK column does not also emit a UNIQUE row.
    let unique_cols: Vec<&str> = rows
        .iter()
        .filter(|r| r[0] == "UNIQUE")
        .map(|r| r[1].as_str())
        .collect();
    assert_eq!(unique_cols, vec!["u_email_key"]);
}

#[test]
fn dropped_column_hidden_from_columns() {
    let (engine, _d) = fresh();
    run(&engine, "CREATE TABLE d (id INT, tmp TEXT, keep TEXT)").unwrap();
    run(&engine, "ALTER TABLE d DROP COLUMN tmp").unwrap();

    let (_c, rows) = query(
        &engine,
        "SELECT column_name, ordinal_position FROM information_schema.columns \
         WHERE table_name = 'd' ORDER BY ordinal_position",
    );
    let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(names, vec!["id", "keep"]);
    // Ordinal is re-sequenced over visible columns.
    assert_eq!(rows[1][1], "2");
}

#[test]
fn catalog_survives_reopen() {
    let dir = tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        seed_erd(&engine);
    }
    let engine = Engine::open(dir.path(), 0).unwrap();
    let (_c, rows) = query(
        &engine,
        "SELECT constraint_name FROM information_schema.referential_constraints",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "line_items_o_region_o_order_no_fkey");
}
