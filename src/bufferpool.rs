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
    sync::{Arc, RwLock},
};

use crate::{
    error::{DbError, Result},
    format::{Lsn, PageId, INVALID_LSN},
    mmap::PageFileMmap,
    page::SlottedPage,
    wal::Wal,
};

/// The read seam (6b). Any page-read consumer (heap reads, the SQL read
/// executor) is generic over this so it works both on the writer's
/// [`BufferPool`] and on a lock-free-of-frames [`SharedPageReader`] handed to
/// concurrent readers.
pub trait PageReader {
    fn read_page(&self, page_id: PageId) -> Result<SlottedPage>;
    fn page_size(&self) -> usize;
}

/// Shared, read-only view over the page file for concurrent readers (6b).
/// Holds a clone of the buffer pool's `Arc<RwLock<PageFileMmap>>` and reads
/// pages directly under the mmap read-lock — no frame/eviction bookkeeping
/// (the OS page cache is the cache; MVCC visibility filters the bytes). The
/// `RwLock` is what prevents a reader observing a torn page or a
/// remapped-away mmap while the single writer mutates it.
#[derive(Clone)]
pub struct SharedPageReader {
    mmap: Arc<RwLock<PageFileMmap>>,
    page_size: usize,
}

impl SharedPageReader {
    pub fn new(mmap: Arc<RwLock<PageFileMmap>>, page_size: usize) -> Self {
        Self { mmap, page_size }
    }
}

impl PageReader for SharedPageReader {
    fn read_page(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id)
    }
    fn page_size(&self) -> usize {
        self.page_size
    }
}

fn lock_poisoned() -> DbError {
    DbError::Recovery("buffer pool mmap lock poisoned".into())
}

/// Read one page out of the shared mmap under its read-lock, returning an
/// owned `SlottedPage` (a copy) so no lock is held past this call.
fn read_page_locked(
    mmap: &Arc<RwLock<PageFileMmap>>,
    page_size: usize,
    page_id: PageId,
) -> Result<SlottedPage> {
    let guard = mmap.read().map_err(|_| lock_poisoned())?;
    let start = page_id as usize * page_size;
    let end = start + page_size;
    if end > guard.len() {
        return Err(DbError::PageNotFound { page_id });
    }
    let raw = guard[start..end].to_vec();
    drop(guard);
    if raw.iter().all(|&b| b == 0) {
        return Ok(SlottedPage::from_bytes_unchecked(raw));
    }
    SlottedPage::from_bytes(raw)
}

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
    mmap: Arc<RwLock<PageFileMmap>>,
    file_page_count: u32,
    /// The durable WAL frontier as last observed from the `Wal` (D5). A dirty
    /// page may be written back and evicted only when `page.LSN <=
    /// durable_wal_lsn` — otherwise stealing it would put un-recoverable state
    /// on disk. Kept as a lower-bound hint: stale-low is always safe (it only
    /// makes `find_victim` skip a page that is in fact evictable), never
    /// unsafe. Refreshed on every write-path fetch and on `sync_wal`.
    durable_wal_lsn: Lsn,
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

        let mmap = Arc::new(RwLock::new(PageFileMmap::new(&file)?));
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
            durable_wal_lsn: INVALID_LSN,
        })
    }

    /// Advance the pool's view of the durable WAL frontier (D5). Called after
    /// any WAL fsync so `find_victim` can safely write back and evict dirty
    /// pages up to `lsn`. Monotonic: never moves the frontier backward.
    pub fn set_durable_wal_lsn(&mut self, lsn: Lsn) {
        if lsn > self.durable_wal_lsn {
            self.durable_wal_lsn = lsn;
        }
    }

    /// Allocate a new page in the file, return its PageId. Takes the mmap
    /// write-lock for the remap so no concurrent reader is holding a slice
    /// into the mapping being replaced.
    pub fn alloc_page(&mut self) -> Result<PageId> {
        let new_id = self.file_page_count;
        self.file_page_count += 1;
        let new_len = self.file_page_count as u64 * self.page_size as u64;
        self.file.set_len(new_len)?;
        {
            let mut guard = self.mmap.write().map_err(|_| lock_poisoned())?;
            *guard = PageFileMmap::new(&self.file)?;
        }
        tracing::debug!(page_id = new_id, "new page allocated");
        Ok(new_id)
    }

    /// A shared, read-only view over the page file for concurrent readers
    /// (6b). See [`SharedPageReader`].
    pub fn shared_reader(&self) -> SharedPageReader {
        SharedPageReader::new(Arc::clone(&self.mmap), self.page_size)
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

    /// Like [`Self::fetch_page`], but usable on the write path, where making
    /// room may require *stealing* a dirty page. It first refreshes the
    /// durable-WAL frontier from `wal` so `find_victim` can write back and
    /// evict any now-durable dirty page; and if the pool is still full of
    /// dirty pages whose WAL is **not** yet durable (the group-commit
    /// deferred-sync case, M9), it forces a single WAL fsync and retries once
    /// — the ARIES "force the log before stealing the page" step (D5). This is
    /// what makes deferred-sync mode safe for working sets larger than the
    /// pool, and it also lets the ordinary (per-statement-fsync) path evict
    /// dirty pages at scale instead of failing with `BufferPoolFull`.
    pub fn fetch_page_for_write(&mut self, page_id: PageId, wal: &mut Wal) -> Result<SlottedPage> {
        self.set_durable_wal_lsn(wal.durable_lsn);
        match self.fetch_page(page_id) {
            Err(DbError::BufferPoolFull) => {
                wal.sync()?;
                self.set_durable_wal_lsn(wal.durable_lsn);
                self.fetch_page(page_id)
            }
            other => other,
        }
    }

    /// Write a modified SlottedPage back into the mmap window (in-memory only).
    /// D5 is NOT checked here — the invariant governs flush/evict, not in-memory writes.
    pub fn write_page(&mut self, page: &SlottedPage) -> Result<()> {
        let page_id = page.page_id();
        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        {
            let mut guard = self.mmap.write().map_err(|_| lock_poisoned())?;
            guard[start..end].copy_from_slice(page.as_bytes());
        }

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
        {
            let guard = self.mmap.read().map_err(|_| lock_poisoned())?;
            guard.flush_range(start, end - start)?;
        }

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

    /// Read one page directly from the shared mmap (no frame bookkeeping).
    /// This is the concurrent-reader entry point and the impl behind
    /// [`PageReader::read_page`] for the writer's own pool.
    pub fn read_page(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id)
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn read_page_from_mmap(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id)
    }

    fn find_victim(&mut self) -> Result<usize> {
        let cap = self.capacity;
        for _ in 0..cap * 2 {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % cap;
            let (pinned, referenced, dirty, page_id) = {
                let f = &self.frames[idx];
                (f.pin_count > 0, f.clock_ref, f.dirty, f.page_id)
            };
            if pinned {
                continue;
            }
            if referenced {
                self.frames[idx].clock_ref = false;
                continue;
            }
            if dirty {
                let Some(pid) = page_id else {
                    return Ok(idx);
                };
                let page_lsn = self.read_page_from_mmap(pid).map(|p| p.lsn()).unwrap_or(0);
                // D5: a dirty page may be stolen only once its WAL is durable.
                // If the durable frontier is unknown (nothing fsynced yet) or
                // behind this page, skip it — the write path
                // (`fetch_page_for_write`) forces a WAL sync and retries.
                if self.durable_wal_lsn == INVALID_LSN || page_lsn > self.durable_wal_lsn {
                    continue;
                }
                // Durable: write it back before reusing the frame (ARIES steal).
                self.flush_page(pid, self.durable_wal_lsn)?;
            }
            return Ok(idx);
        }
        Err(DbError::BufferPoolFull)
    }
}

impl PageReader for BufferPool {
    fn read_page(&self, page_id: PageId) -> Result<SlottedPage> {
        BufferPool::read_page(self, page_id)
    }
    fn page_size(&self) -> usize {
        self.page_size
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
        assert!(
            result.is_err(),
            "D5: flush must be rejected when page LSN > durable WAL LSN"
        );
    }

    /// 6a: a pool full of dirty pages whose WAL is not yet durable (the
    /// group-commit deferred-sync case) must not dead-end at `BufferPoolFull`.
    /// The write-path fetch forces a WAL fsync and then steals a now-durable
    /// page — without ever violating D5 (a page is written back only once its
    /// LSN is durable).
    #[test]
    fn fetch_for_write_forces_wal_sync_to_evict_nondurable_dirty_pages() {
        use crate::wal::Wal;
        let dir = tempdir().unwrap();
        let mut pool = open_pool(dir.path(), 2); // tiny: 2 frames
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
        wal.set_deferred_sync(true); // append without fsync — durable stays behind

        // Fill both frames with unpinned, dirty, NOT-yet-durable pages.
        for i in 0..2u64 {
            let pid = pool.alloc_page().unwrap();
            let _ = pool.fetch_page(pid).unwrap(); // bring into a frame (pins it)
            let lsn = wal.begin_user_txn(i + 1).unwrap(); // deferred: no fsync
            let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
            page.set_lsn(lsn);
            pool.write_page(&page).unwrap(); // marks the frame dirty
            pool.unpin(pid);
        }
        assert_eq!(wal.durable_lsn, INVALID_LSN, "nothing fsynced yet");

        // A plain fetch can find no victim — every frame is dirty and ahead of
        // the (still-INVALID) durable frontier.
        let pid2 = pool.alloc_page().unwrap();
        assert!(
            matches!(pool.fetch_page(pid2), Err(DbError::BufferPoolFull)),
            "plain fetch must fail when all frames are dirty + non-durable"
        );

        // The write-path fetch forces a WAL sync (making the two pages durable)
        // and then succeeds by stealing one of them — reaching past this
        // `unwrap` is itself the proof that eviction no longer dead-ends. (The
        // freshly-alloc'd page reads as zeros until the caller initializes it,
        // so its embedded page_id is not yet meaningful — hence no id check.)
        let _ = pool.fetch_page_for_write(pid2, &mut wal).unwrap();
        assert!(
            wal.durable_lsn > INVALID_LSN,
            "fetch_for_write must have forced an fsync to make room"
        );
        // The pool is usable again: only one of the two originals was stolen,
        // so a subsequent write-path fetch still succeeds.
        let pid3 = pool.alloc_page().unwrap();
        pool.fetch_page_for_write(pid3, &mut wal).unwrap();
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
