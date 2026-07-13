//! Shared test harness: a memory-backed storage service over a fresh engine.
// Each integration-test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::sync::Arc;

use unidb::Engine;
use unidb_storage::{MemoryObjectStore, StorageConfig, StorageService};

/// A live service plus the handles tests poke at. `_dir` keeps the temp engine
/// directory alive for the duration of the test.
pub struct Harness {
    pub _dir: tempfile::TempDir,
    pub engine: Arc<Engine>,
    pub store: Arc<MemoryObjectStore>,
    pub svc: StorageService,
}

/// Build a memory-backed service with the given inline threshold (bytes below it
/// go inline as LOBs; at/above it go to the object store).
pub async fn harness(inline_threshold: usize) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let store = Arc::new(MemoryObjectStore::new("unidb"));
    let mut cfg = StorageConfig::memory();
    cfg.inline_threshold = inline_threshold;
    let svc = StorageService::new(engine.clone(), store.clone(), cfg)
        .await
        .unwrap();
    Harness {
        _dir: dir,
        engine,
        store,
        svc,
    }
}

/// Count rows returned by a read-only SQL statement (test convenience).
pub fn count_rows(engine: &Engine, sql: &str) -> usize {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    for r in res {
        if let unidb::SqlResult::Rows { rows, .. } = r {
            return rows.len();
        }
    }
    0
}
