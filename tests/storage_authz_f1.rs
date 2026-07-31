//! Item 120, Workstream F1 — per-object storage authorization over HTTP.
//!
//! Covers the acceptance list from the F1 task brief:
//!  (a) the owner can read/write/delete their own object in a private bucket.
//!  (b) a different authenticated user CANNOT read or delete it (403).
//!  (c) a public bucket's objects are readable by anyone.
//!  (d) `list_objects` filters to only the caller's readable objects (never
//!      errors).
//!  (e) presign issuance is denied for an object the caller cannot read.
//!  (f) a superuser and a `service_role` token both bypass the gate, and the
//!      bypass is recorded in `audit.log`.
//!  (g) the pre-existing (F1-agnostic) storage routes still pass — see
//!      `tests/storage_routes.rs`, unmodified by this work and still green.

#![cfg(feature = "server")]

#[path = "server_common/mod.rs"]
mod server_common;

use std::{net::SocketAddr, sync::Arc};

use serde_json::json;
use server_common::{metrics_pair, token_for, token_with_claims, valid_token};
use tempfile::TempDir;
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};
use unidb::storage_api::StorageApi;
use unidb_storage::{MemoryObjectStore, StorageConfig, StorageService};

// ---------------------------------------------------------------------------
// Storage-specific test server (mirrors `tests/storage_routes.rs`'s harness,
// plus a `data_dir` accessor so tests can read `audit.log` directly — item-103
// lesson: a bypass must be provable, not just "the call succeeded").
// ---------------------------------------------------------------------------

struct StorageTestServer {
    addr: SocketAddr,
    _tempdir: TempDir,
    data_dir: std::path::PathBuf,
    _task: tokio::task::JoinHandle<()>,
}

impl StorageTestServer {
    async fn spawn(inline_threshold: usize) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let handle = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let engine_arc = handle.engine_arc().unwrap();

        let store = Arc::new(MemoryObjectStore::new("unidb"));
        let mut cfg = StorageConfig::memory();
        cfg.inline_threshold = inline_threshold;
        let svc = StorageService::new(engine_arc, store.clone(), cfg)
            .await
            .unwrap();

        let state = AppState::new(Arc::new(handle))
            .with_storage(Some(Arc::new(svc) as Arc<dyn StorageApi>));
        let jwt = JwtConfig::new(server_common::TEST_JWT_SECRET);
        let (prom_layer, prom_handle) = metrics_pair().clone();
        let router = build_router(state, jwt, prom_layer, prom_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            addr,
            _tempdir: tempdir,
            data_dir,
            _task: task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn audit_log(&self) -> String {
        std::fs::read_to_string(self.data_dir.join("audit.log")).unwrap_or_default()
    }
}

impl Drop for StorageTestServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

async fn sql(client: &reqwest::Client, url: String, token: &str, sql: &str) -> reqwest::Response {
    client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
}

/// Bootstrap: `CREATE USER root SUPERUSER` while still in open/bootstrap mode
/// (any token is an implicit superuser until a user exists), which flips
/// `has_users()` true so that `alice`/`bob` tokens below are ordinary
/// non-superuser `authenticated` callers, not implicit superusers.
async fn bootstrap_root(client: &reqwest::Client, url: String) {
    let admin = valid_token();
    let resp = sql(client, url, &admin, "CREATE USER root SUPERUSER").await;
    assert_eq!(resp.status().as_u16(), 200, "bootstrap CREATE USER root");
}

async fn create_bucket(
    client: &reqwest::Client,
    server: &StorageTestServer,
    token: &str,
    name: &str,
    is_public: bool,
) {
    let resp = client
        .post(server.url("/storage/buckets"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "name": name, "is_public": is_public }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "create bucket {name} (is_public={is_public})"
    );
}

async fn put_object(
    client: &reqwest::Client,
    server: &StorageTestServer,
    token: &str,
    bucket: &str,
    key: &str,
    body: &[u8],
) -> reqwest::Response {
    client
        .put(server.url(&format!("/storage/{bucket}/objects/{key}")))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Length", body.len().to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// (a) owner can read/write/delete own object in a private bucket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_can_read_write_delete_own_private_object() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    bootstrap_root(&client, server.url("/sql")).await;
    let alice = token_for("alice");

    // Private by default (is_public: false).
    create_bucket(&client, &server, &alice, "priv", false).await;
    let resp = put_object(
        &client,
        &server,
        &alice,
        "priv",
        "mine.txt",
        b"alice's data",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201, "alice writes her own object");

    // Read (via presign issuance — the only HTTP read surface for objects).
    let resp = client
        .get(server.url("/storage/priv/presign/mine.txt"))
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "owner can presign-read");

    // Delete.
    let resp = client
        .delete(server.url("/storage/priv/objects/mine.txt"))
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "owner can delete her own object"
    );
}

// ---------------------------------------------------------------------------
// (b) a different user cannot read/delete a private object (403)
// (e) presign issuance denied for an unreadable object
// ---------------------------------------------------------------------------

#[tokio::test]
async fn other_user_cannot_read_or_delete_private_object() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    bootstrap_root(&client, server.url("/sql")).await;
    let alice = token_for("alice");
    let bob = token_for("bob");

    create_bucket(&client, &server, &alice, "priv2", false).await;
    let resp = put_object(
        &client,
        &server,
        &alice,
        "priv2",
        "secret.txt",
        b"alice only",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);

    // bob cannot presign-read alice's object (e: issuance denied).
    let resp = client
        .get(server.url("/storage/priv2/presign/secret.txt"))
        .header("Authorization", format!("Bearer {bob}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "bob must not be able to mint a presigned URL for alice's private object"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "STORAGE_FORBIDDEN");

    // bob cannot delete it either.
    let resp = client
        .delete(server.url("/storage/priv2/objects/secret.txt"))
        .header("Authorization", format!("Bearer {bob}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "bob must not be able to delete alice's object"
    );

    // bob cannot overwrite it either (write gate, not just delete).
    let resp = put_object(
        &client,
        &server,
        &bob,
        "priv2",
        "secret.txt",
        b"bob overwrites",
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "bob must not be able to overwrite alice's object"
    );

    // The object is unharmed: alice can still read it.
    let resp = client
        .get(server.url("/storage/priv2/presign/secret.txt"))
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "alice's object survives bob's denied attempts"
    );
}

// ---------------------------------------------------------------------------
// (c) public bucket objects readable by anyone
// ---------------------------------------------------------------------------

#[tokio::test]
async fn public_bucket_objects_readable_by_anyone() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    bootstrap_root(&client, server.url("/sql")).await;
    let alice = token_for("alice");
    let bob = token_for("bob");

    create_bucket(&client, &server, &alice, "pub", true).await;
    let resp = put_object(
        &client,
        &server,
        &alice,
        "pub",
        "shared.txt",
        b"public data",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);

    // bob (not the owner) CAN presign-read a public bucket's object.
    let resp = client
        .get(server.url("/storage/pub/presign/shared.txt"))
        .header("Authorization", format!("Bearer {bob}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "public bucket objects are readable by any caller"
    );

    // But bob still cannot WRITE or DELETE it — public exempts reads only.
    let resp = put_object(
        &client,
        &server,
        &bob,
        "pub",
        "shared.txt",
        b"bob overwrites",
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "public bucket status does not exempt writes"
    );
    let resp = client
        .delete(server.url("/storage/pub/objects/shared.txt"))
        .header("Authorization", format!("Bearer {bob}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "public bucket status does not exempt deletes"
    );
}

// ---------------------------------------------------------------------------
// (d) list_objects filters to only the caller's readable objects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_objects_filters_to_readable_objects() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    bootstrap_root(&client, server.url("/sql")).await;
    let alice = token_for("alice");
    let bob = token_for("bob");

    create_bucket(&client, &server, &alice, "mixed", false).await;
    assert_eq!(
        put_object(&client, &server, &alice, "mixed", "a1.txt", b"alice 1")
            .await
            .status()
            .as_u16(),
        201
    );
    assert_eq!(
        put_object(&client, &server, &alice, "mixed", "a2.txt", b"alice 2")
            .await
            .status()
            .as_u16(),
        201
    );

    // Alice sees both of her objects.
    let resp = client
        .get(server.url("/storage/mixed/objects"))
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["objects"].as_array().unwrap().len(),
        2,
        "alice sees her own 2 objects"
    );

    // Bob sees NONE of them (filtered, not an error).
    let resp = client
        .get(server.url("/storage/mixed/objects"))
        .header("Authorization", format!("Bearer {bob}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "list_objects must filter, never error, for an unreadable bucket"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["objects"].as_array().unwrap().len(),
        0,
        "bob must not see alice's private objects in the list"
    );
}

// ---------------------------------------------------------------------------
// (f) superuser and service_role both bypass, and the bypass is audited
// ---------------------------------------------------------------------------

#[tokio::test]
async fn superuser_and_service_role_bypass_are_audited() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    bootstrap_root(&client, server.url("/sql")).await;
    let root = token_for("root"); // created SUPERUSER by bootstrap_root
    let alice = token_for("alice");
    let service_role = token_with_claims(None, &[("role", json!("service_role"))]);

    create_bucket(&client, &server, &alice, "bypass", false).await;
    let resp = put_object(
        &client,
        &server,
        &alice,
        "bypass",
        "x.txt",
        b"alice's object",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);

    // Superuser (root) can read alice's private object.
    let resp = client
        .get(server.url("/storage/bypass/presign/x.txt"))
        .header("Authorization", format!("Bearer {root}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "superuser bypasses the F1 gate"
    );

    // service_role (no `sub`, `role: service_role` claim) can also read it.
    let resp = client
        .get(server.url("/storage/bypass/presign/x.txt"))
        .header("Authorization", format!("Bearer {service_role}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "service_role bypasses the F1 gate"
    );

    // service_role can also delete it (write-path bypass).
    let resp = client
        .delete(server.url("/storage/bypass/objects/x.txt"))
        .header("Authorization", format!("Bearer {service_role}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "service_role bypasses the write gate too"
    );

    // Both bypasses are in the audit trail (item-103 lesson: distinct from the
    // unaudited implicit-superuser/no-token path).
    let audit_log = server.audit_log();
    assert!(
        audit_log.contains("superuser_storage_bypass"),
        "superuser storage bypass must be audited, got: {audit_log}"
    );
    assert!(
        audit_log.contains("service_role_storage_bypass"),
        "service_role storage bypass must be audited, got: {audit_log}"
    );
    assert!(
        audit_log.contains("\"root\""),
        "the audited superuser event must name the acting subject, got: {audit_log}"
    );
}
