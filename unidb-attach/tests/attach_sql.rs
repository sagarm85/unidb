// `execute_sql` and `execute_cypher` integration tests (M8.c).
// AttachClient is blocking; tests are plain #[test] with the async runtime
// hidden inside TestServer.

#[path = "attach_common/mod.rs"]
mod attach_common;

use attach_common::{valid_token, TestServer};
use unidb_attach::{AttachClient, AttachError, ExecResult};

fn client(server: &TestServer) -> AttachClient {
    AttachClient::new(&server.base_url, valid_token()).unwrap()
}

#[test]
fn execute_sql_create_insert_select_round_trip() {
    let server = TestServer::spawn();
    let c = client(&server);

    let r = c.execute_sql("CREATE TABLE t (id INT, name TEXT)").unwrap();
    assert!(matches!(r[0], ExecResult::CreatedTable));

    c.execute_sql("INSERT INTO t (id, name) VALUES (1, 'alice')")
        .unwrap();

    let r = c.execute_sql("SELECT * FROM t").unwrap();
    let ExecResult::Rows { rows, .. } = &r[0] else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], serde_json::json!(1));
    assert_eq!(rows[0][1], serde_json::json!("alice"));
}

#[test]
fn execute_sql_update_and_delete() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT, val TEXT)").unwrap();
    c.execute_sql("INSERT INTO t (id, val) VALUES (1, 'first')")
        .unwrap();
    c.execute_sql("UPDATE t SET val = 'second' WHERE id = 1")
        .unwrap();

    let r = c.execute_sql("SELECT * FROM t WHERE id = 1").unwrap();
    let ExecResult::Rows { rows, .. } = &r[0] else {
        panic!()
    };
    assert_eq!(rows[0][1], serde_json::json!("second"));

    c.execute_sql("DELETE FROM t WHERE id = 1").unwrap();
    let r = c.execute_sql("SELECT * FROM t").unwrap();
    let ExecResult::Rows { rows, .. } = &r[0] else {
        panic!()
    };
    assert!(rows.is_empty());
}

#[test]
fn execute_sql_table_not_found_maps_to_typed_error() {
    let server = TestServer::spawn();
    let c = client(&server);

    let err = c
        .execute_sql("SELECT * FROM nonexistent_table")
        .unwrap_err();
    assert!(
        matches!(err, AttachError::TableNotFound(_)),
        "expected TableNotFound, got {err}"
    );
}

#[test]
fn execute_sql_parse_error_maps_to_typed_error() {
    let server = TestServer::spawn();
    let c = client(&server);

    let err = c.execute_sql("this is not sql").unwrap_err();
    assert!(
        matches!(err, AttachError::SqlParse(_)),
        "expected SqlParse, got {err}"
    );
}

#[test]
fn execute_sql_multi_statement_is_atomic() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT)").unwrap();

    // The second statement fails (table doesn't exist) — the INSERT must roll back.
    let err = c
        .execute_sql("INSERT INTO t (id) VALUES (1); SELECT * FROM nope")
        .unwrap_err();
    assert!(matches!(err, AttachError::TableNotFound(_)));

    let r = c.execute_sql("SELECT * FROM t").unwrap();
    let ExecResult::Rows { rows, .. } = &r[0] else {
        panic!()
    };
    assert!(rows.is_empty(), "aborted INSERT must not be visible");
}

#[test]
fn execute_cypher_returns_matching_edges() {
    let server = TestServer::spawn();
    let c = client(&server);

    // Set up a directed edge using create_edge.
    // Grammar note: `MATCH (a)-[:TYPE]->(b) WHERE a = <from_id> RETURN b`
    // — variable `a` maps to from_id, `b` maps to to_id.  Property access
    // like `a.name` is not supported (nodes are opaque IDs in this engine).
    c.create_edge(10, 20, "KNOWS", serde_json::json!({}))
        .unwrap();

    let r = c
        .execute_cypher("MATCH (a)-[:KNOWS]->(b) WHERE a = 10 RETURN b")
        .unwrap();

    let ExecResult::Rows { rows, .. } = &r[0] else {
        panic!("expected Rows, got {:?}", r);
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], serde_json::json!(20), "to_id must be 20");
}
