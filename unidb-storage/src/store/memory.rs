//! In-process [`ObjectStore`] — the Docker-free test double.
//!
//! Lets the whole service + reconciler (and both AC crash directions) be tested
//! deterministically without MinIO/S3. `presign_*` returns an opaque in-process
//! stub URL (there is no HTTP server); real SigV4 presigning is unit-tested
//! offline against [`super::s3::S3ObjectStore`].

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use super::{ObjectEntry, ObjectMeta, ObjectStore, StoreError, StoreResult};

#[derive(Clone)]
struct Stored {
    bytes: Vec<u8>,
    etag: String,
    last_modified: SystemTime,
}

/// An in-memory object store. Cheap to clone-share via `Arc`.
pub struct MemoryObjectStore {
    bucket: String,
    objects: Mutex<HashMap<String, Stored>>,
}

impl MemoryObjectStore {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            objects: Mutex::new(HashMap::new()),
        }
    }

    /// Test helper: seed a key directly (simulates a byte that reached the store
    /// with no committed metadata — the orphan direction).
    pub fn seed(&self, key: &str, bytes: &[u8]) {
        let etag = etag_of(bytes);
        self.objects.lock().expect("memory store lock").insert(
            key.to_string(),
            Stored {
                bytes: bytes.to_vec(),
                etag,
                last_modified: SystemTime::now(),
            },
        );
    }

    /// Test helper: how many objects the store currently holds.
    pub fn len(&self) -> usize {
        self.objects.lock().expect("memory store lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test helper: does the store hold `key`?
    pub fn contains(&self, key: &str) -> bool {
        self.objects
            .lock()
            .expect("memory store lock")
            .contains_key(key)
    }
}

fn etag_of(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[async_trait]
impl ObjectStore for MemoryObjectStore {
    async fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        _content_type: Option<&str>,
    ) -> StoreResult<ObjectMeta> {
        let etag = etag_of(&bytes);
        let size = bytes.len() as u64;
        let now = SystemTime::now();
        self.objects.lock().expect("memory store lock").insert(
            key.to_string(),
            Stored {
                bytes,
                etag: etag.clone(),
                last_modified: now,
            },
        );
        Ok(ObjectMeta {
            size,
            etag: Some(etag),
            last_modified: Some(now),
        })
    }

    async fn get(&self, key: &str) -> StoreResult<Vec<u8>> {
        self.objects
            .lock()
            .expect("memory store lock")
            .get(key)
            .map(|s| s.bytes.clone())
            .ok_or_else(|| StoreError::NotFound(key.to_string()))
    }

    async fn head(&self, key: &str) -> StoreResult<Option<ObjectMeta>> {
        Ok(self
            .objects
            .lock()
            .expect("memory store lock")
            .get(key)
            .map(|s| ObjectMeta {
                size: s.bytes.len() as u64,
                etag: Some(s.etag.clone()),
                last_modified: Some(s.last_modified),
            }))
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.objects.lock().expect("memory store lock").remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectEntry>> {
        Ok(self
            .objects
            .lock()
            .expect("memory store lock")
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, s)| ObjectEntry {
                key: k.clone(),
                size: s.bytes.len() as u64,
                last_modified: Some(s.last_modified),
            })
            .collect())
    }

    async fn presign_put(&self, key: &str, ttl: Duration) -> StoreResult<String> {
        Ok(format!(
            "memory://{}/{}?stub-presign=put&ttl={}",
            self.bucket,
            key,
            ttl.as_secs()
        ))
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> StoreResult<String> {
        Ok(format!(
            "memory://{}/{}?stub-presign=get&ttl={}",
            self.bucket,
            key,
            ttl.as_secs()
        ))
    }
}
