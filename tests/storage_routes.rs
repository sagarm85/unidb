//! Storage HTTP routes integration test (backlog item 31, Phase D).
//!
//! Covers:
//!  1. All 7 `/storage/*` routes return 503 when storage is not configured.
//!  2. Bucket CRUD round-trip + 409 on non-empty delete.
//!  3. Inline object put/list round-trip (body.len() < inline_threshold).
//!  4. Presigned-ticket shape for large objects (body.len() >= threshold).
//!  5. Virtual-folder listing with prefix + delimiter.
//!
//! ## Live-MinIO tests
//!
//! Tests tagged `#[ignore]` require a real MinIO instance and are gated
//! behind `MINIO_ENDPOINT`/`MINIO_ACCESS_KEY`/`MINIO_SECRET_KEY`. Run them
//! explicitly after starting MinIO:
//!
//! ```bash
//! docker compose -f docker/docker-compose.minio.yml up -d
//! MINIO_ENDPOINT=http://localhost:9000 \
//!   MINIO_ACCESS_KEY=minioadmin MINIO_SECRET_KEY=minioadmin \
//!   cargo test -p unidb --features server --test storage_routes -- --include-ignored
//! ```
//!
//! All tests below (without `#[ignore]`) use a memory-backed `StorageService`
//! and require no Docker.

#![cfg(feature = "server")]

#[path = "server_common/mod.rs"]
mod server_common;

use std::{net::SocketAddr, sync::Arc};

use server_common::{metrics_pair, valid_token, TEST_JWT_SECRET};
use tempfile::TempDir;
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};
use unidb::storage_api::StorageApi;
use unidb_storage::{MemoryObjectStore, StorageConfig, StorageService};

// ---------------------------------------------------------------------------
// Storage-specific test server
// ---------------------------------------------------------------------------

struct StorageTestServer {
    addr: SocketAddr,
    pub store: Arc<MemoryObjectStore>,
    _tempdir: TempDir,
    _task: tokio::task::JoinHandle<()>,
}

impl StorageTestServer {
    /// Spawn a server with a memory-backed `StorageService`. `inline_threshold`
    /// controls where inline LOB vs presigned-PUT split occurs (bytes).
    async fn spawn(inline_threshold: usize) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
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
        let jwt = JwtConfig::new(TEST_JWT_SECRET);
        let (prom_layer, prom_handle) = metrics_pair().clone();
        let router = build_router(state, jwt, prom_layer, prom_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            addr,
            store,
            _tempdir: tempdir,
            _task: task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for StorageTestServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

// ---------------------------------------------------------------------------
// Test 1: all routes → 503 when storage is not configured
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unconfigured_storage_returns_503_for_all_routes() {
    // Default TestServer has no storage → AppState::storage is None.
    let server = server_common::TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // GET routes
    for path in [
        "/storage/buckets",
        "/storage/mybucket/objects",
        "/storage/mybucket/presign/mykey",
    ] {
        let resp = client
            .get(server.url(path))
            .header("Authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            503,
            "GET {path} should be 503 when storage unconfigured"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "STORAGE_NOT_AVAILABLE");
    }

    // DELETE routes
    for path in ["/storage/buckets/test", "/storage/mybucket/objects/mykey"] {
        let resp = client
            .delete(server.url(path))
            .header("Authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503, "DELETE {path} should be 503");
    }

    // POST /storage/buckets with valid body (so JSON extractor doesn't fail first)
    let resp = client
        .post(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"name": "test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        503,
        "POST /storage/buckets should be 503"
    );

    // PUT /storage/{bucket}/objects/{*key}
    let resp = client
        .put(server.url("/storage/mybucket/objects/mykey"))
        .header("Authorization", &auth)
        .header("Content-Length", "5")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503, "PUT object should be 503");
}

// ---------------------------------------------------------------------------
// Test 2: bucket CRUD + 409 on non-empty delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bucket_crud_and_409_on_non_empty_delete() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Initially no buckets
    let resp = client
        .get(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["buckets"].as_array().unwrap().len(), 0);

    // Create bucket
    let resp = client
        .post(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"name": "photos"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create bucket");

    // List → one bucket
    let resp = client
        .get(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let buckets = body["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0]["name"], "photos");

    // Put a small object into the bucket (body < 1024 bytes → inline)
    let content = b"tiny image data";
    let resp = client
        .put(server.url("/storage/photos/objects/cat.jpg"))
        .header("Authorization", &auth)
        .header("Content-Type", "image/jpeg")
        .header("Content-Length", content.len().to_string())
        .body(content.as_slice())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "put object");

    // Delete bucket → 409 BUCKET_NOT_EMPTY
    let resp = client
        .delete(server.url("/storage/buckets/photos"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "non-empty bucket delete should be 409"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BUCKET_NOT_EMPTY");

    // Delete the object first
    let resp = client
        .delete(server.url("/storage/photos/objects/cat.jpg"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "delete object");

    // Now delete bucket → 204
    let resp = client
        .delete(server.url("/storage/buckets/photos"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "delete empty bucket");

    // List → empty
    let resp = client
        .get(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["buckets"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Test 3: inline object round-trip (body < threshold)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inline_object_round_trip() {
    let server = StorageTestServer::spawn(1024).await; // 1 KiB threshold
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Create bucket
    client
        .post(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"name": "docs"}))
        .send()
        .await
        .unwrap();

    // PUT small inline object (body = 18 bytes < 1024)
    let content = b"hello inline world";
    let resp = client
        .put(server.url("/storage/docs/objects/hello.txt"))
        .header("Authorization", &auth)
        .header("Content-Type", "text/plain")
        .header("Content-Length", content.len().to_string())
        .body(content.as_slice())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "put inline object");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["tier"], "inline");
    assert_eq!(body["size"], content.len() as i64);

    // Nothing went to the memory store (bytes are engine LOBs)
    assert!(
        server.store.is_empty(),
        "inline object must not touch the object store"
    );

    // List objects → 1 object
    let resp = client
        .get(server.url("/storage/docs/objects"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let objects = body["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0]["object_key"], "hello.txt");
    assert_eq!(objects[0]["tier"], "inline");
    assert_eq!(objects[0]["status"], "ready");
}

// ---------------------------------------------------------------------------
// Test 4: presigned-ticket shape for large objects (body >= threshold)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presigned_ticket_shape_for_large_objects() {
    // Tiny threshold (10 bytes) so our test payload is "large"
    let server = StorageTestServer::spawn(10).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    client
        .post(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"name": "uploads"}))
        .send()
        .await
        .unwrap();

    // PUT with body > 10 bytes → server returns presigned ticket (200, not 201)
    let content = b"this payload is larger than ten bytes";
    let resp = client
        .put(server.url("/storage/uploads/objects/large.bin"))
        .header("Authorization", &auth)
        .header("Content-Length", content.len().to_string())
        .body(content.as_slice())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "large object → presign path (200)"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["presigned_put_url"].is_string(),
        "response must contain presigned_put_url"
    );
    assert!(
        body["storage_key"].is_string(),
        "response must contain storage_key"
    );
    // Memory store's stub URL contains the expected marker
    let url = body["presigned_put_url"].as_str().unwrap();
    assert!(!url.is_empty(), "presigned_put_url must be non-empty");
}

// ---------------------------------------------------------------------------
// Test 5: virtual-folder listing with prefix + delimiter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn virtual_folder_listing_with_prefix_and_delimiter() {
    let server = StorageTestServer::spawn(1024).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Setup bucket
    client
        .post(server.url("/storage/buckets"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"name": "files"}))
        .send()
        .await
        .unwrap();

    // Upload objects with nested keys
    for key in [
        "photos/cat.jpg",
        "photos/dog.jpg",
        "photos/vacation/beach.jpg",
        "photos/vacation/paris.jpg",
        "docs/readme.txt",
    ] {
        let content = b"data";
        client
            .put(server.url(&format!("/storage/files/objects/{key}")))
            .header("Authorization", &auth)
            .header("Content-Length", content.len().to_string())
            .body(content.as_slice())
            .send()
            .await
            .unwrap();
    }

    // List with prefix="photos/" delimiter="/"
    let resp = client
        .get(server.url("/storage/files/objects"))
        .header("Authorization", &auth)
        .query(&[("prefix", "photos/"), ("delimiter", "/")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let objects = body["objects"].as_array().unwrap();
    let prefixes = body["prefixes"].as_array().unwrap();

    // Direct children under "photos/": cat.jpg and dog.jpg
    let keys: Vec<&str> = objects
        .iter()
        .map(|o| o["object_key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"photos/cat.jpg"),
        "cat.jpg should be a direct object"
    );
    assert!(
        keys.contains(&"photos/dog.jpg"),
        "dog.jpg should be a direct object"
    );
    // vacation/ items should NOT appear as direct objects
    assert!(
        !keys.iter().any(|k| k.contains("vacation")),
        "vacation items should be folded into a prefix, not appear as direct objects"
    );

    // One virtual folder: "photos/vacation/"
    let prefix_strs: Vec<&str> = prefixes.iter().map(|p| p.as_str().unwrap()).collect();
    assert!(
        prefix_strs.contains(&"photos/vacation/"),
        "photos/vacation/ should appear as a prefix (virtual folder)"
    );

    // docs/readme.txt is outside "photos/" prefix — must not appear
    assert!(
        !keys.contains(&"docs/readme.txt"),
        "docs/readme.txt is outside the prefix and must not appear"
    );
}
