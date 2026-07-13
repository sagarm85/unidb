//! Presign generation is a hard requirement (browsers move bytes directly). It
//! is SigV4-signed *locally* by `aws-sdk-s3`, so we prove it **offline** — no
//! MinIO/S3 needed. The live MinIO round-trip is gated behind
//! `STORAGE_S3_ENDPOINT` so a plain `cargo test` (no Docker) still passes.

use std::time::Duration;

use unidb_storage::config::Backend;
use unidb_storage::{ObjectStore, S3ObjectStore, StorageConfig};

fn minio_style_config() -> StorageConfig {
    StorageConfig {
        backend: Backend::Minio,
        endpoint: Some("http://localhost:9000".to_string()),
        region: "us-east-1".to_string(),
        bucket: "unidb".to_string(),
        access_key: Some("minioadmin".to_string()),
        secret_key: Some("minioadmin".to_string()),
        force_path_style: true,
        ..StorageConfig::default()
    }
}

#[tokio::test]
async fn s3_store_generates_offline_presigned_sigv4_urls() {
    let store = S3ObjectStore::from_config(&minio_style_config()).unwrap();

    let put = store
        .presign_put("b/k", Duration::from_secs(600))
        .await
        .unwrap();
    let get = store
        .presign_get("b/k", Duration::from_secs(600))
        .await
        .unwrap();

    for (label, url) in [("put", &put), ("get", &get)] {
        // Path-style endpoint: bucket + key in the path, not a vhost subdomain.
        assert!(
            url.contains("localhost:9000/unidb/b/k"),
            "{label} url should be path-style: {url}"
        );
        // SigV4 query params prove it is a real presigned URL.
        assert!(
            url.contains("X-Amz-Signature="),
            "{label} url unsigned: {url}"
        );
        assert!(url.contains("X-Amz-Expires=600"), "{label} url ttl: {url}");
        assert!(
            url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
            "{label} url alg: {url}"
        );
    }
}

#[test]
fn s3_backend_requires_credentials() {
    let mut cfg = minio_style_config();
    cfg.access_key = None;
    cfg.secret_key = None;
    assert!(
        S3ObjectStore::from_config(&cfg).is_err(),
        "missing creds must be a config error, not a silent unsigned client"
    );
}

#[test]
fn memory_config_defaults() {
    let c = StorageConfig::memory();
    assert_eq!(c.backend, Backend::Memory);
    assert_eq!(c.inline_threshold, 1024 * 1024);
    assert!(!c.force_path_style);
}

/// Live MinIO/S3 round-trip. Skipped unless `STORAGE_S3_ENDPOINT` is set (so a
/// plain `cargo test` without Docker passes). See `unidb-storage/README.md`.
#[tokio::test]
async fn live_store_round_trip_when_configured() {
    let Ok(endpoint) = std::env::var("STORAGE_S3_ENDPOINT") else {
        eprintln!("skipping live_store_round_trip: STORAGE_S3_ENDPOINT not set");
        return;
    };

    let cfg = StorageConfig {
        backend: Backend::Minio,
        endpoint: Some(endpoint),
        region: std::env::var("STORAGE_REGION").unwrap_or_else(|_| "us-east-1".into()),
        bucket: std::env::var("STORAGE_BUCKET").unwrap_or_else(|_| "unidb".into()),
        access_key: Some(
            std::env::var("STORAGE_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into()),
        ),
        secret_key: Some(
            std::env::var("STORAGE_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into()),
        ),
        force_path_style: true,
        ..StorageConfig::default()
    };
    let store = S3ObjectStore::from_config(&cfg).unwrap();

    let key = "itest/roundtrip.bin";
    let body = b"live minio round trip".to_vec();
    store
        .put(key, body.clone(), Some("application/octet-stream"))
        .await
        .unwrap();
    assert!(store.head(key).await.unwrap().is_some());
    assert_eq!(store.get(key).await.unwrap(), body);
    assert!(store
        .list("itest/")
        .await
        .unwrap()
        .iter()
        .any(|e| e.key == key));
    store.delete(key).await.unwrap();
    assert!(store.head(key).await.unwrap().is_none());
}
