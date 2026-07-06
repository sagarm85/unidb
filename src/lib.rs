// unsafe_code is denied crate-wide; mmap.rs is the sole exception (CLAUDE.md §4).
#![deny(unsafe_code)]

pub mod bufferpool;
pub mod checkpoint;
pub mod control;
pub mod error;
pub mod format;
pub mod heap;
pub mod mmap;
pub mod page;
pub mod recovery;
pub mod wal;

use std::path::{Path, PathBuf};

use crate::{
    bufferpool::BufferPool,
    control::ControlData,
    error::Result,
    format::DEFAULT_PAGE_SIZE,
    heap::Heap,
    wal::Wal,
};

pub use crate::heap::RowId;
pub use crate::error::DbError;

const POOL_CAPACITY: usize = 256;

pub struct Engine {
    control: ControlData,
    pool: BufferPool,
    wal: Wal,
    heap: Heap,
    control_path: PathBuf,
    _wal_path: PathBuf,
}

impl Engine {
    /// Open (or create) a database at `dir`. Pass `page_size = 0` to use the default.
    pub fn open(dir: &Path, page_size: u32) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ctrl_p = dir.join("control");
        let data_p = dir.join("data.db");
        let wal_p = dir.join("db.wal");

        let ps = if page_size == 0 { DEFAULT_PAGE_SIZE } else { page_size };
        let control = control::open_or_create(&ctrl_p, ps)?;
        let page_size_usize = control.page_size as usize;

        // Run recovery before opening normal operation.
        if wal_p.exists() && ctrl_p.exists() {
            recovery::recover(
                &ctrl_p,
                &data_p,
                &wal_p,
                page_size_usize,
                POOL_CAPACITY,
            )?;
        }

        let pool = BufferPool::open(&data_p, page_size_usize, POOL_CAPACITY)?;
        let wal_tail = control.wal_tail_lsn;
        let wal = Wal::open(&wal_p, wal_tail)?;
        let heap = Heap::new(page_size_usize);

        tracing::info!(dir = %dir.display(), page_size = control.page_size, "engine opened");
        Ok(Self {
            control,
            pool,
            wal,
            heap,
            control_path: ctrl_p,
            _wal_path: wal_p,
        })
    }

    pub fn insert(&mut self, data: &[u8]) -> Result<RowId> {
        self.heap.insert(data, &mut self.pool, &mut self.wal)
    }

    pub fn get(&mut self, row_id: RowId) -> Result<Vec<u8>> {
        self.heap.get(row_id, &mut self.pool)
    }

    pub fn update(&mut self, row_id: RowId, new_data: &[u8]) -> Result<()> {
        self.heap.update(row_id, new_data, &mut self.pool, &mut self.wal)
    }

    pub fn delete(&mut self, row_id: RowId) -> Result<()> {
        self.heap.delete(row_id, &mut self.pool, &mut self.wal)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        checkpoint::run(
            &mut self.pool,
            &mut self.wal,
            &self.control_path,
            &mut self.control,
        )
    }

    /// Flush all dirty pages without a full checkpoint (used in tests).
    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all(self.wal.durable_lsn)
    }
}

/// Initialize a `tracing_subscriber` with `RUST_LOG` env filter.
/// Call once at the start of your binary or test suite.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_insert_get_roundtrip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let rid = engine.insert(b"hello world").unwrap();
        let data = engine.get(rid).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn update_and_verify() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let rid = engine.insert(b"initial_value").unwrap();
        engine.update(rid, b"updated").unwrap();
        assert_eq!(engine.get(rid).unwrap(), b"updated");
    }

    #[test]
    fn delete_makes_row_gone() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let rid = engine.insert(b"transient").unwrap();
        engine.delete(rid).unwrap();
        assert!(engine.get(rid).is_err());
    }

    #[test]
    fn reopen_after_flush_recovers_data() {
        let dir = tempdir().unwrap();
        let rid = {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let rid = engine.insert(b"durable").unwrap();
            engine.flush().unwrap();
            rid
        };
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        assert_eq!(engine2.get(rid).unwrap(), b"durable");
    }
}
