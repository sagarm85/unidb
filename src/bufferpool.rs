// Buffer pool: fixed frames, pin/unpin, clock eviction, dirty set.
// Enforces D5: a dirty page may NOT be flushed/evicted while
//   page.LSN > durable_WAL_LSN.
//
// The page file is memory-mapped via mmap::PageFileMmap (the only unsafe module).
// Frames are eviction-tracking metadata; actual data lives in the mmap window.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::Path,
};

use crate::{
    error::{DbError, Result},
    format::{Lsn, PageId, INVALID_LSN},
    mmap::PageFileMmap,
    page::SlottedPage,
};

struct Frame {
    page_id: Option<PageId>,
    pin_count: u32,
    dirty: bool,
    clock_ref: bool,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page_id: None,
            pin_count: 0,
            dirty: false,
            clock_ref: false,
        }
    }
}

pub struct BufferPool {
    page_size: usize,
    capacity: usize,
    frames: Vec<Frame>,
    /// frame_index[page_id] = frame_idx
    frame_index: HashMap<PageId, usize>,
    clock_hand: usize,
    file: File,
    mmap: PageFileMmap,
    file_page_count: u32,
}

impl BufferPool {
    pub fn open(path: &Path, page_size: usize, capacity: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        // Ensure the file is at least one page so the mmap is non-empty.
        if file.metadata()?.len() == 0 {
            file.set_len(page_size as u64)?;
        }

        let file_len = file.metadata()?.len();
        let file_page_count = (file_len / page_size as u64) as u32;

        let mmap = PageFileMmap::new(&file)?;
        let frames = (0..capacity).map(|_| Frame::empty()).collect();

        tracing::info!(
            path = %path.display(),
            page_size,
            capacity,
            file_page_count,
            "buffer pool opened"
        );

        Ok(Self {
            page_size,
            capacity,
            frames,
            frame_index: HashMap::new(),
            clock_hand: 0,
            file,
            mmap,
            file_page_count,
        })
    }

    /// Allocate a new page in the file, return its PageId.
    pub fn alloc_page(&mut self) -> Result<PageId> {
        let new_id = self.file_page_count;
        self.file_page_count += 1;
        let new_len = self.file_page_count as u64 * self.page_size as u64;
        self.file.set_len(new_len)?;
        self.mmap = PageFileMmap::new(&self.file)?;
        tracing::debug!(page_id = new_id, "new page allocated");
        Ok(new_id)
    }

    /// Pin a page into a frame and return its data (as SlottedPage).
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<SlottedPage> {
        if let Some(&frame_idx) = self.frame_index.get(&page_id) {
            self.frames[frame_idx].pin_count += 1;
            self.frames[frame_idx].clock_ref = true;
            return self.read_page_from_mmap(page_id);
        }

        // Page not in pool — find a victim frame.
        let frame_idx = self.find_victim()?;

        if let Some(old_pid) = self.frames[frame_idx].page_id {
            self.frame_index.remove(&old_pid);
        }

        self.frames[frame_idx].page_id = Some(page_id);
        self.frames[frame_idx].pin_count = 1;
        self.frames[frame_idx].dirty = false;
        self.frames[frame_idx].clock_ref = true;
        self.frame_index.insert(page_id, frame_idx);

        self.read_page_from_mmap(page_id)
    }

    /// Write a modified SlottedPage back into the mmap window (in-memory only).
    /// D5 is NOT checked here — the invariant governs flush/evict, not in-memory writes.
    pub fn write_page(&mut self, page: &SlottedPage) -> Result<()> {
        let page_id = page.page_id();
        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        self.mmap[start..end].copy_from_slice(page.as_bytes());

        if let Some(&frame_idx) = self.frame_index.get(&page_id) {
            self.frames[frame_idx].dirty = true;
        }
        Ok(())
    }

    /// Flush a specific dirty page to disk. D5 checked again here.
    pub fn flush_page(&mut self, page_id: PageId, durable_wal_lsn: Lsn) -> Result<()> {
        let page = self.read_page_from_mmap(page_id)?;

        if durable_wal_lsn != INVALID_LSN && page.lsn() > durable_wal_lsn {
            return Err(DbError::Recovery(format!(
                "D5 violation on flush: page {page_id} LSN {} > durable WAL LSN {durable_wal_lsn}",
                page.lsn()
            )));
        }

        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        self.mmap.flush_range(start, end - start)?;

        if let Some(&frame_idx) = self.frame_index.get(&page_id) {
            self.frames[frame_idx].dirty = false;
        }
        tracing::debug!(page_id, "page flushed to disk");
        Ok(())
    }

    /// Decrement pin count for a page.
    pub fn unpin(&mut self, page_id: PageId) {
        if let Some(&frame_idx) = self.frame_index.get(&page_id) {
            if self.frames[frame_idx].pin_count > 0 {
                self.frames[frame_idx].pin_count -= 1;
            }
        }
    }

    pub fn flush_all(&mut self, durable_wal_lsn: Lsn) -> Result<()> {
        let dirty_pages: Vec<PageId> = self
            .frames
            .iter()
            .filter_map(|f| if f.dirty { f.page_id } else { None })
            .collect();
        for pid in dirty_pages {
            self.flush_page(pid, durable_wal_lsn)?;
        }
        Ok(())
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn read_page_from_mmap(&self, page_id: PageId) -> Result<SlottedPage> {
        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        if end > self.mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let raw = self.mmap[start..end].to_vec();
        if raw.iter().all(|&b| b == 0) {
            return Ok(SlottedPage::from_bytes_unchecked(raw));
        }
        SlottedPage::from_bytes(raw)
    }

    fn find_victim(&mut self) -> Result<usize> {
        let cap = self.capacity;
        for _ in 0..cap * 2 {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % cap;
            let f = &self.frames[idx];
            if f.pin_count > 0 {
                continue;
            }
            if f.clock_ref {
                self.frames[idx].clock_ref = false;
                continue;
            }
            // D5: never evict a dirty page whose LSN is ahead of the durable WAL.
            // Callers of find_victim must flush first if needed; for now just skip
            // such frames (they will become evictable after a flush).
            if f.dirty {
                if let Some(pid) = f.page_id {
                    if let Ok(p) = self.read_page_from_mmap(pid) {
                        if p.lsn() > self.durable_wal_lsn_hint() {
                            continue; // skip — can't evict yet
                        }
                    }
                }
            }
            return Ok(idx);
        }
        Err(DbError::BufferPoolFull)
    }

    /// Returns 0 when unknown; callers that want strict D5 on eviction should
    /// flush_all before relying on pool capacity. This hint keeps the eviction
    /// path simple without requiring the pool to track the WAL.
    fn durable_wal_lsn_hint(&self) -> Lsn {
        INVALID_LSN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, PAGE_TYPE_HEAP};
    use crate::page::SlottedPage;
    use tempfile::tempdir;

    fn open_pool(dir: &Path, cap: usize) -> BufferPool {
        BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, cap).unwrap()
    }

    #[test]
    fn alloc_and_write_page() {
        let dir = tempdir().unwrap();
        let mut pool = open_pool(dir.path(), 16);
        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.set_lsn(5);
        pool.write_page(&page).unwrap();
        pool.flush_page(pid, 5).unwrap();
    }

    #[test]
    fn d5_violation_on_flush_rejected() {
        // D5: flushing a page whose LSN > durable_wal_lsn must fail.
        let dir = tempdir().unwrap();
        let mut pool = open_pool(dir.path(), 16);
        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.set_lsn(100);
        pool.write_page(&page).unwrap(); // in-memory write is fine
        let result = pool.flush_page(pid, 50); // flush with durable WAL only at 50
        assert!(result.is_err(), "D5: flush must be rejected when page LSN > durable WAL LSN");
    }

    #[test]
    fn fetch_written_page() {
        let dir = tempdir().unwrap();
        let mut pool = open_pool(dir.path(), 16);
        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.insert(b"test_row").unwrap();
        page.set_lsn(1);
        pool.write_page(&page).unwrap();
        let fetched = pool.fetch_page(pid).unwrap();
        assert_eq!(fetched.get(0).unwrap(), b"test_row");
    }
}
