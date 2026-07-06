// Single-table heap: insert / read / update / delete.
// Each operation is a WAL mini-transaction (D2).
// In-place update is fine in M0; MVCC versioning deferred to M1 (D4).
// FSM: linear scan for a page with sufficient free space (acceptable for M0).

use crate::{
    bufferpool::BufferPool,
    error::Result,
    format::{PageId, PAGE_TYPE_HEAP},
    page::SlottedPage,
    wal::Wal,
};

/// Stable row identifier: (page_id, slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId {
    pub page_id: PageId,
    pub slot: u16,
}

pub struct Heap {
    page_size: usize,
    /// Ordered list of page IDs belonging to this heap.
    pages: Vec<PageId>,
}

impl Heap {
    pub fn new(page_size: usize) -> Self {
        Self {
            page_size,
            pages: Vec::new(),
        }
    }

    /// Insert `data` and return the assigned RowId.
    /// WAL order: BEGIN → INSERT record → page modified in memory → COMMIT (fsync).
    /// D5 is satisfied: we write to memory before commit; flush only after commit.
    pub fn insert(&mut self, data: &[u8], pool: &mut BufferPool, wal: &mut Wal) -> Result<RowId> {
        let needed = crate::page::SLOT_SIZE + crate::page::TUPLE_HEADER_SIZE + data.len();
        let page_id = self.find_or_alloc_page(needed, pool, wal)?;

        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(page_id)?;
        let slot = page.insert(data)?;
        let ins_lsn = wal.log_insert(txn_id, begin_lsn, page_id, slot, data)?;
        page.set_lsn(ins_lsn);
        pool.write_page(&page)?; // in-memory; D5 not triggered here
        pool.unpin(page_id);
        wal.commit_mini_txn(txn_id, ins_lsn)?; // fsync → durable_lsn >= ins_lsn

        Ok(RowId { page_id, slot })
    }

    /// Read the payload stored at `row_id`.
    pub fn get(&self, row_id: RowId, pool: &mut BufferPool) -> Result<Vec<u8>> {
        let page = pool.fetch_page(row_id.page_id)?;
        let data = page.get(row_id.slot)?.to_vec();
        pool.unpin(row_id.page_id);
        Ok(data)
    }

    /// Update the payload at `row_id`. New data must fit in the existing slot.
    pub fn update(
        &mut self,
        row_id: RowId,
        new_data: &[u8],
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(row_id.page_id)?;
        let old_data = page.get(row_id.slot)?.to_vec();
        let upd_lsn =
            wal.log_update(txn_id, begin_lsn, row_id.page_id, row_id.slot, new_data, &old_data)?;
        page.update(row_id.slot, new_data)?;
        page.set_lsn(upd_lsn);
        pool.write_page(&page)?;
        pool.unpin(row_id.page_id);
        wal.commit_mini_txn(txn_id, upd_lsn)?;
        Ok(())
    }

    /// Delete the row at `row_id`.
    pub fn delete(&mut self, row_id: RowId, pool: &mut BufferPool, wal: &mut Wal) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(row_id.page_id)?;
        let old_data = page.get(row_id.slot)?.to_vec();
        let del_lsn =
            wal.log_delete(txn_id, begin_lsn, row_id.page_id, row_id.slot, &old_data)?;
        page.delete(row_id.slot)?;
        page.set_lsn(del_lsn);
        pool.write_page(&page)?;
        pool.unpin(row_id.page_id);
        wal.commit_mini_txn(txn_id, del_lsn)?;
        Ok(())
    }

    // ── FSM ──────────────────���─────────────────────��────────────────────────

    fn find_or_alloc_page(
        &mut self,
        needed: usize,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<PageId> {
        for &pid in &self.pages {
            let page = pool.fetch_page(pid)?;
            let free = page.free_space();
            pool.unpin(pid);
            if free >= needed {
                return Ok(pid);
            }
        }
        self.alloc_heap_page(pool, wal)
    }

    fn alloc_heap_page(&mut self, pool: &mut BufferPool, wal: &mut Wal) -> Result<PageId> {
        let pid = pool.alloc_page()?;
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let alloc_lsn = wal.log_insert(txn_id, begin_lsn, pid, u16::MAX, &[])?;
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, self.page_size);
        page.set_lsn(alloc_lsn);
        wal.commit_mini_txn(txn_id, alloc_lsn)?;
        pool.write_page(&page)?;
        pool.unpin(pid);
        self.pages.push(pid);
        tracing::debug!(page_id = pid, "heap page allocated");
        Ok(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufferpool::BufferPool;
    use crate::error::DbError;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::wal::Wal;
    use tempfile::tempdir;

    fn setup(dir: &std::path::Path) -> (Heap, BufferPool, Wal) {
        let pool =
            BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&dir.join("db.wal"), INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        (heap, pool, wal)
    }

    #[test]
    fn insert_and_get() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal) = setup(dir.path());
        let rid = heap.insert(b"hello", &mut pool, &mut wal).unwrap();
        let data = heap.get(rid, &mut pool).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn update_in_place() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal) = setup(dir.path());
        let rid = heap.insert(b"old_value", &mut pool, &mut wal).unwrap();
        heap.update(rid, b"new", &mut pool, &mut wal).unwrap();
        let data = heap.get(rid, &mut pool).unwrap();
        assert_eq!(data, b"new");
    }

    #[test]
    fn delete_then_get_fails() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal) = setup(dir.path());
        let rid = heap.insert(b"to_delete", &mut pool, &mut wal).unwrap();
        heap.delete(rid, &mut pool, &mut wal).unwrap();
        assert!(matches!(heap.get(rid, &mut pool), Err(DbError::TupleDeleted { .. })));
    }

    #[test]
    fn multiple_rows() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal) = setup(dir.path());
        let r1 = heap.insert(b"row1", &mut pool, &mut wal).unwrap();
        let r2 = heap.insert(b"row2", &mut pool, &mut wal).unwrap();
        let r3 = heap.insert(b"row3", &mut pool, &mut wal).unwrap();
        assert_eq!(heap.get(r1, &mut pool).unwrap(), b"row1");
        assert_eq!(heap.get(r2, &mut pool).unwrap(), b"row2");
        assert_eq!(heap.get(r3, &mut pool).unwrap(), b"row3");
    }
}
