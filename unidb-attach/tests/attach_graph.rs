// Graph route tests via AttachClient (M8.c).

#[path = "attach_common/mod.rs"]
mod attach_common;

use attach_common::{valid_token, TestServer};
use unidb_attach::AttachClient;

fn client(server: &TestServer) -> AttachClient {
    AttachClient::new(&server.base_url, valid_token()).unwrap()
}

#[test]
fn create_edge_and_traverse() {
    let server = TestServer::spawn();
    let c = client(&server);

    let row_id = c
        .create_edge(1, 2, "KNOWS", serde_json::json!({"since": 2020}))
        .unwrap();

    let edges = c.edges_from(1).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].to_id, 2);
    assert_eq!(edges[0].edge_type, "KNOWS");
    assert_eq!(edges[0].row_id, row_id);
}

#[test]
fn delete_edge_removes_from_traversal() {
    let server = TestServer::spawn();
    let c = client(&server);

    let row_id = c
        .create_edge(5, 6, "FRIENDS", serde_json::json!({}))
        .unwrap();

    // Edge is visible before delete.
    assert_eq!(c.edges_from(5).unwrap().len(), 1);

    c.delete_edge(row_id, 5).unwrap();

    // Edge must not appear after delete.
    assert!(c.edges_from(5).unwrap().is_empty());
}

#[test]
fn edges_from_with_no_edges_returns_empty() {
    let server = TestServer::spawn();
    let c = client(&server);

    let edges = c.edges_from(999).unwrap();
    assert!(edges.is_empty());
}

#[test]
fn multiple_edges_from_same_node() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.create_edge(1, 2, "KNOWS", serde_json::json!({})).unwrap();
    c.create_edge(1, 3, "KNOWS", serde_json::json!({})).unwrap();
    c.create_edge(1, 4, "FOLLOWS", serde_json::json!({}))
        .unwrap();

    let edges = c.edges_from(1).unwrap();
    assert_eq!(edges.len(), 3);

    let to_ids: Vec<i64> = edges.iter().map(|e| e.to_id).collect();
    assert!(to_ids.contains(&2));
    assert!(to_ids.contains(&3));
    assert!(to_ids.contains(&4));
}
