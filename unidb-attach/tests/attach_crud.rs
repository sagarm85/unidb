// Raw row CRUD via AttachClient (M8.c).

#[path = "attach_common/mod.rs"]
mod attach_common;

use attach_common::{valid_token, TestServer};
use unidb_attach::{AttachClient, AttachError};

fn client(server: &TestServer) -> AttachClient {
    AttachClient::new(&server.base_url, valid_token()).unwrap()
}

#[test]
fn insert_get_update_delete_round_trip() {
    let server = TestServer::spawn();
    let c = client(&server);

    let row_id = c.insert(b"hello world".to_vec()).unwrap();

    let data = c.get(row_id).unwrap();
    assert_eq!(data, b"hello world");

    let new_row_id = c.update(row_id, b"updated".to_vec()).unwrap();
    assert_ne!(
        new_row_id, row_id,
        "MVCC update returns a new RowId for the new version"
    );

    let data = c.get(new_row_id).unwrap();
    assert_eq!(data, b"updated");

    c.delete(new_row_id).unwrap();

    let err = c.get(new_row_id).unwrap_err();
    assert!(
        matches!(err, AttachError::NotFound(_)),
        "expected NotFound after delete, got {err}"
    );
}

#[test]
fn get_on_old_row_id_after_update_returns_not_found() {
    let server = TestServer::spawn();
    let c = client(&server);

    let old_id = c.insert(b"v1".to_vec()).unwrap();
    let _new_id = c.update(old_id, b"v2".to_vec()).unwrap();

    // The old RowId is superseded by MVCC — it should no longer be visible.
    let err = c.get(old_id).unwrap_err();
    assert!(
        matches!(err, AttachError::NotFound(_)),
        "old RowId must not be visible after update"
    );
}

#[test]
fn delete_then_get_returns_not_found() {
    let server = TestServer::spawn();
    let c = client(&server);

    let row_id = c.insert(b"transient".to_vec()).unwrap();
    c.delete(row_id).unwrap();

    let err = c.get(row_id).unwrap_err();
    assert!(matches!(err, AttachError::NotFound(_)));
}
