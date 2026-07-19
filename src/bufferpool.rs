// Buffer pool: fixed frames, pin/unpin, clock eviction, dirty set.
// Enforces D5: a dirty page may NOT be flushed/evicted while
//   page.LSN > durable_WAL_LSN.
//
// The page file is memory-mapped via mmap::PageFileMmap (the only unsafe module).
// Frames are eviction-tracking metadata; actual data lives in the mmap window.
//
// P5.a — CONCURRENT. The pool is internally synchronized so it can be shared as
// `&BufferPool` (and `Arc<BufferPool>`) across many writer threads:
//   * The mmap lives behind `Arc<RwLock<PageFileMmap>>` (unchanged from 6b) — the
//     read-lock serializes readers against a remap/torn write; the write-lock is
//     taken only to mutate page bytes or grow the mapping.
//   * The frame table + allocation metadata + D5/FPI/poison bookkeeping move
//     under one `Mutex<PoolState>`. Every method that used to be `&mut self` is
//     now `&self` (callers passing `&pool` auto-reborrow to `&`), so P5.a
//     lands without touching heap/index/graph call sites.
//   * A **page-latch table** (`latches`) hands out shared/exclusive latches per
//     page via a small `#![forbid(unsafe_code)]`-clean custom latch
//     ([`PageLatchInner`], a `Mutex`+`Condvar` reader/writer latch whose guards
//     own an `Arc` to the latch — no self-referential `'static` transmute). A
//     heap read-modify-write (`fetch_page_for_write` … `write_page`) holds that
//     page's **exclusive latch** for the whole span, so two writers can never
//     lose an update on the same page; latch-coupling walks take shared latches.
//
// D5 under concurrency: `durable_wal_lsn` is a monotonic frontier guarded by the
// state mutex; `find_victim` writes back + steals a dirty page only when its LSN
// is at or behind that frontier, all while holding the state mutex, so no thread
// can flush a page ahead of the durable WAL.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex, MutexGuard, RwLock,
    },
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
/// remapped-away mmap while a writer mutates it.
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
        // SharedPageReader accesses pages directly (bypasses the pool frame table),
        // so always verify CRC on load — this is equivalent to a pool miss.
        read_page_locked(&self.mmap, self.page_size, page_id, true)
    }
    fn page_size(&self) -> usize {
        self.page_size
    }
}

/// How many bytes the page file grows by per extension (P1.c). One whole-file
/// remap per chunk instead of per page. 4 MiB balances remap frequency against
/// slack: at the 8 KiB default page size this is 512 pages per grow.
const GROW_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Number of frames pre-allocated at pool open (item 37 — lazy frame growth).
/// Keeps `Engine::open()` cheap for small/embedded workloads while allowing a
/// multi-million-frame `capacity` ceiling without paying for it up front.
/// Actual frames are pushed on demand in `find_victim` up to `capacity`.
const INITIAL_SLAB_FRAMES: usize = 256;

fn lock_poisoned() -> DbError {
    DbError::Recovery("buffer pool mmap lock poisoned".into())
}

// ── page latches (P5.a) ──────────────────────────────────────────────────────

/// The shared state of one page latch: a reader count plus a "writer held"
/// flag. A small hand-rolled reader/writer latch (no `unsafe`, no external
/// crate) so its guards can own an `Arc` to the latch and be `'static` — the
/// standard-library `RwLockReadGuard` borrows its lock, which cannot be handed
/// out from an `Arc<RwLock>` without a self-referential transmute.
struct LatchState {
    readers: u32,
    writer: bool,
}

/// One page's physical latch (P5.a). Independent of the logical row locks in
/// `lockmgr.rs`: this orders *physical* access to one page's bytes. Held only
/// for the duration of a page access, never across an fsync or a user
/// transaction, so it can never participate in a logical-lock deadlock cycle.
struct PageLatchInner {
    mtx: Mutex<LatchState>,
    cv: Condvar,
}

impl PageLatchInner {
    fn new() -> Self {
        Self {
            mtx: Mutex::new(LatchState {
                readers: 0,
                writer: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn acquire_shared(&self) {
        let mut g = self.mtx.lock().unwrap_or_else(|e| e.into_inner());
        while g.writer {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.readers += 1;
    }

    fn release_shared(&self) {
        let mut g = self.mtx.lock().unwrap_or_else(|e| e.into_inner());
        g.readers = g.readers.saturating_sub(1);
        if g.readers == 0 {
            self.cv.notify_all();
        }
    }

    fn acquire_exclusive(&self) {
        let mut g = self.mtx.lock().unwrap_or_else(|e| e.into_inner());
        while g.writer || g.readers > 0 {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.writer = true;
    }

    fn release_exclusive(&self) {
        let mut g = self.mtx.lock().unwrap_or_else(|e| e.into_inner());
        g.writer = false;
        self.cv.notify_all();
    }
}

/// A held shared page latch (P5.a). While alive, no writer holds the page's
/// exclusive latch, so the page bytes are stable for reading / for coupling to
/// a child during a structure traversal. Releases on drop.
#[must_use = "hold the latch for the whole physical page access"]
pub struct SharedLatch(Arc<PageLatchInner>);

impl Drop for SharedLatch {
    fn drop(&mut self) {
        self.0.release_shared();
    }
}

/// A held exclusive page latch (P5.a). While alive, this thread is the sole
/// mutator of the page's bytes — the invariant that makes concurrent
/// `fetch_page_for_write` … `write_page` spans on the *same* page safe (no lost
/// update). Different pages have independent latches, so writers to distinct
/// pages proceed fully in parallel. Releases on drop.
#[must_use = "hold the latch for the whole physical page read-modify-write"]
pub struct ExclusiveLatch(Arc<PageLatchInner>);

impl Drop for ExclusiveLatch {
    fn drop(&mut self) {
        self.0.release_exclusive();
    }
}

/// The logical high-water mark (next-unused page id) given a file physically
/// sized to `mapped_pages` (P1.c). Trailing pages left all-zero by a previous
/// session's chunked pre-growth were never handed out — a real heap/catalog
/// page always carries a non-zero header — so skip them so `alloc_page` reuses
/// that slack rather than leaking a chunk per reopen. The scan is bounded by
/// one chunk (growth never runs more than a chunk ahead of allocation).
fn logical_page_count(
    mmap: &Arc<RwLock<PageFileMmap>>,
    page_size: usize,
    mapped_pages: u32,
) -> Result<u32> {
    let guard = mmap.read().map_err(|_| lock_poisoned())?;
    let mut count = mapped_pages;
    while count > 0 {
        let start = (count - 1) as usize * page_size;
        let end = start + page_size;
        if end > guard.len() || !guard[start..end].iter().all(|&b| b == 0) {
            break;
        }
        count -= 1;
    }
    Ok(count)
}

/// Read one page out of the shared mmap under its read-lock, returning an
/// owned `SlottedPage` (a copy) so no lock is held past this call.
///
/// `verify` controls whether the CRC is checked:
///   - `true`  (pool miss / first load): verify CRC — catches on-disk corruption.
///   - `false` (pool hit): skip CRC — the page was verified when it entered the
///     pool; re-verifying on every subsequent fetch is pure overhead (item 86).
fn read_page_locked(
    mmap: &Arc<RwLock<PageFileMmap>>,
    page_size: usize,
    page_id: PageId,
    verify: bool,
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
    if verify {
        SlottedPage::from_bytes(raw)
    } else {
        Ok(SlottedPage::from_bytes_unchecked(raw))
    }
}

struct Frame {
    page_id: Option<PageId>,
    pin_count: u32,
    dirty: bool,
    clock_ref: bool,
    /// The LSN of the most-recent `write_page` on this frame (item 77: D5
    /// fast-path).  Kept in sync by `write_page` so `find_victim` can check
    /// `frame.lsn > durable_wal_lsn` without re-reading 8 KiB from mmap.
    /// Zero for a newly allocated, unwritten frame (no WAL record yet).
    lsn: Lsn,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page_id: None,
            pin_count: 0,
            dirty: false,
            clock_ref: false,
            lsn: 0,
        }
    }
}

/// The mutable pool state guarded by one mutex (P5.a). Everything that used to
/// be a `&mut self` field lives here; the public methods lock this briefly.
struct PoolState {
    frames: Vec<Frame>,
    /// frame_index[page_id] = frame_idx
    frame_index: HashMap<PageId, usize>,
    clock_hand: usize,
    file: File,
    /// Next page id to hand out (the logical high-water mark). Distinct from
    /// `mapped_pages` (P1.c): the file is pre-grown in large chunks, so the
    /// mapping usually covers more pages than have been allocated.
    file_page_count: u32,
    /// Number of pages the backing file is sized to *and* the mmap covers
    /// (P1.c). Always `>= file_page_count`.
    mapped_pages: u32,
    /// The durable WAL frontier as last observed from the `Wal` (D5). A dirty
    /// page may be written back and evicted only when `page.LSN <=
    /// durable_wal_lsn`. Stale-low is always safe (only skips an evictable
    /// page), never unsafe. Refreshed on every write-path fetch and `sync_wal`.
    durable_wal_lsn: Lsn,
    /// Pages that already have a full-page image (`WAL_FPI`) logged since the
    /// last checkpoint (P1.a — torn-page protection). Tracked by `PageId` (not
    /// per-frame) so it survives eviction: exactly one FPI per page per interval.
    fpi_logged: std::collections::HashSet<PageId>,
    /// Set once a data-file flush (`msync`) has failed (P1.b, fsyncgate) — fatal
    /// for the session; every later flush returns `DurabilityFailure`.
    flush_poisoned: bool,
    /// Test/fault-injection hook (P1.b): the next `flush_page` fails and poisons.
    flush_fault_armed: bool,
}

pub struct BufferPool {
    page_size: usize,
    capacity: usize,
    mmap: Arc<RwLock<PageFileMmap>>,
    /// How many pages to grow the file by when the mapping must be extended
    /// (P1.c). One remap per `grow_chunk_pages` allocations.
    grow_chunk_pages: u32,
    state: Mutex<PoolState>,
    /// Per-page physical latches (P5.a). Created on demand; a page's latch
    /// orders concurrent physical access to that page only, so distinct pages
    /// never contend here.
    latches: Mutex<HashMap<PageId, Arc<PageLatchInner>>>,
    /// Cache-efficiency counters (item 21). Lock-free `Relaxed` atomics — a
    /// `fetch_add` outside the state mutex, so the read/write hot paths that
    /// hit an already-resident frame never pay for observability. `hits` when
    /// `fetch_page` finds the page resident, `misses` when it must fault it in,
    /// `evictions` each time a resident page is displaced from a frame.
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

/// Cache-efficiency snapshot (item 21) — the buffer-pool half of `stats()`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct BufferPoolStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// `hits / (hits + misses)`, `0.0` before the first access.
    pub hit_ratio: f64,
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
        let mapped_pages = (file_len / page_size as u64) as u32;

        let mmap = Arc::new(RwLock::new(PageFileMmap::new(&file)?));

        // P1.c: skip trailing all-zero slack pages pre-grown by a previous
        // session so `alloc_page` reuses that slack instead of leaking a chunk.
        let file_page_count = logical_page_count(&mmap, page_size, mapped_pages)?;

        // Item 37: allocate only a small initial slab; grow on demand in
        // find_victim up to `capacity` as a ceiling. Keeps open() cheap for
        // small/embedded callers even when capacity is in the millions.
        let initial_frames = INITIAL_SLAB_FRAMES.min(capacity);
        let frames: Vec<Frame> = (0..initial_frames).map(|_| Frame::empty()).collect();

        // Grow the file by ~4 MiB at a time (at least one page).
        let grow_chunk_pages = (GROW_CHUNK_BYTES / page_size).max(1) as u32;

        tracing::info!(
            path = %path.display(),
            page_size,
            capacity,
            initial_frames,
            file_page_count,
            mapped_pages,
            grow_chunk_pages,
            "buffer pool opened"
        );

        Ok(Self {
            page_size,
            capacity,
            mmap,
            grow_chunk_pages,
            state: Mutex::new(PoolState {
                frames,
                frame_index: HashMap::new(),
                clock_hand: 0,
                file,
                file_page_count,
                mapped_pages,
                durable_wal_lsn: INVALID_LSN,
                fpi_logged: std::collections::HashSet::new(),
                flush_poisoned: false,
                flush_fault_armed: false,
            }),
            latches: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        })
    }

    /// A cold-path readout of the cache-efficiency counters (item 21).
    pub fn pool_stats(&self) -> BufferPoolStats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        BufferPoolStats {
            hits,
            misses,
            evictions: self.evictions.load(Ordering::Relaxed),
            hit_ratio: if total == 0 {
                0.0
            } else {
                hits as f64 / total as f64
            },
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, PoolState> {
        // Recover from poisoning rather than aborting: a poisoned pool mutex
        // means a prior panic while locked; proceed with state as-is.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get (or create) the shared latch entry for `page_id`.
    fn latch_for(&self, page_id: PageId) -> Arc<PageLatchInner> {
        let mut table = self.latches.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            table
                .entry(page_id)
                .or_insert_with(|| Arc::new(PageLatchInner::new())),
        )
    }

    /// Acquire a **shared** physical latch on `page_id` (P5.a) — for a read or a
    /// latch-coupling descent. Blocks only while another thread holds the
    /// exclusive latch on the *same* page.
    pub fn latch_shared(&self, page_id: PageId) -> SharedLatch {
        let latch = self.latch_for(page_id);
        latch.acquire_shared();
        SharedLatch(latch)
    }

    /// Acquire an **exclusive** physical latch on `page_id` (P5.a) — held across
    /// a whole `fetch_page_for_write` … `write_page` read-modify-write so two
    /// writers can never lose an update on the same page. Blocks while any other
    /// thread holds a shared or exclusive latch on the same page.
    pub fn latch_exclusive(&self, page_id: PageId) -> ExclusiveLatch {
        let latch = self.latch_for(page_id);
        latch.acquire_exclusive();
        ExclusiveLatch(latch)
    }

    /// Arm a one-shot data-file flush fault (P1.b fault injection).
    pub fn arm_flush_fault(&self) {
        self.lock_state().flush_fault_armed = true;
    }

    /// Whether the pool has latched into the poisoned state (a flush failed).
    pub fn is_flush_poisoned(&self) -> bool {
        self.lock_state().flush_poisoned
    }

    /// Log a full-page image (`WAL_FPI`) for `page_id` if one has not yet been
    /// logged since the last checkpoint (P1.a — torn-page protection). Call this
    /// on the write path right after fetching a page for write and before
    /// emitting the first incremental change record for that page in the current
    /// mini-txn. Returns `Some(fpi_lsn)` when an image was written, or `None`
    /// when the page was already covered this interval.
    ///
    /// Concurrency (P5.a): the caller holds the page's exclusive latch, so the
    /// "read the current image" step observes stable bytes and no other writer
    /// can be modifying this page. The FPI set lives under the state mutex, so
    /// the interval-claim is atomic — exactly one FPI per page per interval.
    pub fn maybe_log_fpi(
        &self,
        page_id: PageId,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: Lsn,
    ) -> Result<Option<Lsn>> {
        if self.lock_state().fpi_logged.contains(&page_id) {
            return Ok(None);
        }
        let image = self.read_page(page_id)?;
        let lsn = wal.log_fpi(txn_id, prev_lsn, page_id, image.as_bytes())?;
        self.lock_state().fpi_logged.insert(page_id);
        Ok(Some(lsn))
    }

    /// Record that `page_id` has an equivalent full-page image already in the
    /// WAL for this checkpoint interval (e.g. vacuum's compacted-page image).
    pub fn mark_fpi_logged(&self, page_id: PageId) {
        self.lock_state().fpi_logged.insert(page_id);
    }

    /// Reset full-page-image tracking (P1.a). Called by `checkpoint::run` once
    /// all dirty pages have been flushed.
    pub fn clear_fpi_tracking(&self) {
        self.lock_state().fpi_logged.clear();
    }

    /// Overwrite a page with a raw image during recovery redo of a `WAL_FPI`
    /// (P1.a). Deliberately does **not** read or CRC-validate the existing
    /// on-disk page. `image` must be exactly `page_size` bytes.
    pub fn restore_page_image(&self, page_id: PageId, image: &[u8]) -> Result<()> {
        if image.len() != self.page_size {
            return Err(DbError::Recovery(format!(
                "WAL_FPI image for page {page_id} is {} bytes, expected {}",
                image.len(),
                self.page_size
            )));
        }
        // Recovery is single-threaded, but take the exclusive latch anyway so
        // "page bytes mutated only under the exclusive latch" holds uniformly.
        let _lx = self.latch_exclusive(page_id);
        self.ensure_mapped(page_id + 1)?;
        {
            let mut st = self.lock_state();
            if page_id + 1 > st.file_page_count {
                st.file_page_count = page_id + 1;
            }
        }
        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        {
            let mut guard = self.mmap.write().map_err(|_| lock_poisoned())?;
            guard[start..end].copy_from_slice(image);
        }
        let mut st = self.lock_state();
        if let Some(&frame_idx) = st.frame_index.get(&page_id) {
            st.frames[frame_idx].dirty = true;
        }
        Ok(())
    }

    /// Ensure the page file is sized (and mapped) to include `page_id`, only
    /// ever growing. Recovery uses this when replaying heap redo into a data
    /// file smaller than the log implies — e.g. a freshly-created replica
    /// materializing from a shipped WAL (P6.c), or any recover-into-wiped-file
    /// path. Normal crash recovery leaves the file already sized, so this is a
    /// cheap no-op there.
    pub fn ensure_page_allocated(&self, page_id: PageId) -> Result<()> {
        self.ensure_mapped(page_id + 1)?;
        let mut st = self.lock_state();
        if page_id + 1 > st.file_page_count {
            st.file_page_count = page_id + 1;
        }
        Ok(())
    }

    /// Advance the pool's view of the durable WAL frontier (D5). Monotonic.
    pub fn set_durable_wal_lsn(&self, lsn: Lsn) {
        let mut st = self.lock_state();
        if lsn > st.durable_wal_lsn {
            st.durable_wal_lsn = lsn;
        }
    }

    /// Allocate a new page in the file, return its PageId (P1.c). Grows the file
    /// in chunks and remaps only on a chunk boundary. The high-water-mark bump
    /// is atomic under the state mutex, so two concurrent allocators get
    /// distinct ids.
    pub fn alloc_page(&self) -> Result<PageId> {
        let new_id = {
            let mut st = self.lock_state();
            let id = st.file_page_count;
            st.file_page_count += 1;
            id
        };
        self.ensure_mapped(new_id + 1)?;
        tracing::debug!(page_id = new_id, "new page allocated");
        Ok(new_id)
    }

    /// Ensure the file is sized (and mapped) to at least `min_pages` pages,
    /// growing by whole `grow_chunk_pages` chunks and re-creating the mmap only
    /// when the boundary is crossed (P1.c). Takes the state lock (for the file
    /// handle + `mapped_pages`) and the mmap write-lock (for the remap).
    fn ensure_mapped(&self, min_pages: u32) -> Result<()> {
        let mut st = self.lock_state();
        if min_pages <= st.mapped_pages {
            return Ok(());
        }
        let chunk = self.grow_chunk_pages;
        let target = min_pages
            .div_ceil(chunk)
            .saturating_mul(chunk)
            .max(min_pages);
        let new_len = target as u64 * self.page_size as u64;
        st.file.set_len(new_len)?;
        {
            let mut guard = self.mmap.write().map_err(|_| lock_poisoned())?;
            *guard = PageFileMmap::new(&st.file)?;
        }
        st.mapped_pages = target;
        tracing::debug!(mapped_pages = target, "page file mapping grown");
        Ok(())
    }

    /// Number of pages logically allocated (the next page id to be handed out).
    pub fn page_count(&self) -> u32 {
        self.lock_state().file_page_count
    }

    /// The configured page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// A shared, read-only view over the page file for concurrent readers (6b).
    pub fn shared_reader(&self) -> SharedPageReader {
        SharedPageReader::new(Arc::clone(&self.mmap), self.page_size)
    }

    /// Pin a page into a frame and return its data (as SlottedPage).
    pub fn fetch_page(&self, page_id: PageId) -> Result<SlottedPage> {
        {
            let mut st = self.lock_state();
            if let Some(&frame_idx) = st.frame_index.get(&page_id) {
                st.frames[frame_idx].pin_count += 1;
                st.frames[frame_idx].clock_ref = true;
                drop(st);
                self.hits.fetch_add(1, Ordering::Relaxed); // item 21: cache hit
                // Pool hit: page was CRC-verified when it entered the pool;
                // skip re-verification (item 86).
                return self.read_page_from_mmap_unchecked(page_id);
            }

            // Page not in pool — a cache miss; find a victim frame (may flush a
            // durable dirty page back, all under the same state lock).
            self.misses.fetch_add(1, Ordering::Relaxed); // item 21: cache miss
            let frame_idx = self.find_victim(&mut st)?;

            if let Some(old_pid) = st.frames[frame_idx].page_id {
                // A resident page is being displaced to make room (item 21).
                self.evictions.fetch_add(1, Ordering::Relaxed);
                st.frame_index.remove(&old_pid);
            }
            st.frames[frame_idx].page_id = Some(page_id);
            st.frames[frame_idx].pin_count = 1;
            st.frames[frame_idx].dirty = false;
            st.frames[frame_idx].clock_ref = true;
            st.frame_index.insert(page_id, frame_idx);
        }
        // Pool miss: verify CRC on first load (item 86).
        self.read_page_from_mmap(page_id)
    }

    /// Like [`Self::fetch_page`], but usable on the write path, where making
    /// room may require *stealing* a dirty page. It first refreshes the durable
    /// frontier from `wal`, and if the pool is still full of not-yet-durable
    /// dirty pages (group-commit deferred-sync, M9) it forces one WAL fsync and
    /// retries once — the ARIES "force the log before stealing the page" step
    /// (D5). Preserved verbatim under concurrency.
    pub fn fetch_page_for_write(&self, page_id: PageId, wal: &Wal) -> Result<SlottedPage> {
        self.set_durable_wal_lsn(wal.durable_lsn());
        match self.fetch_page(page_id) {
            Err(DbError::BufferPoolFull) => {
                wal.sync()?;
                self.set_durable_wal_lsn(wal.durable_lsn());
                self.fetch_page(page_id)
            }
            other => other,
        }
    }

    /// Write a modified SlottedPage back into the mmap window (in-memory only).
    /// D5 is NOT checked here — the invariant governs flush/evict, not in-memory
    /// writes. The caller must hold the page's exclusive latch (P5.a).
    pub fn write_page(&self, page: &SlottedPage) -> Result<()> {
        let page_id = page.page_id();
        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        {
            let mut guard = self.mmap.write().map_err(|_| lock_poisoned())?;
            guard[start..end].copy_from_slice(page.as_bytes());
        }
        let mut st = self.lock_state();
        if let Some(&frame_idx) = st.frame_index.get(&page_id) {
            st.frames[frame_idx].dirty = true;
            // Item 77: cache the page LSN in the frame so find_victim can
            // check D5 without re-reading 8 KiB from mmap per dirty frame.
            st.frames[frame_idx].lsn = page.lsn();
        }
        Ok(())
    }

    /// Flush a specific dirty page to disk. D5 checked again here. Public entry —
    /// takes the state lock and delegates to the lock-free `flush_locked`.
    pub fn flush_page(&self, page_id: PageId, durable_wal_lsn: Lsn) -> Result<()> {
        let mut st = self.lock_state();
        self.flush_locked(&mut st, page_id, durable_wal_lsn)
    }

    /// D5-checked flush of one page, assuming the state lock is already held.
    fn flush_locked(
        &self,
        st: &mut PoolState,
        page_id: PageId,
        durable_wal_lsn: Lsn,
    ) -> Result<()> {
        // P1.b: once a data-file flush has failed, never report success again.
        if st.flush_poisoned {
            return Err(DbError::DurabilityFailure(format!(
                "buffer pool is poisoned by an earlier flush failure; page {page_id} cannot be reported durable"
            )));
        }

        // Pool-resident page — CRC was valid when set_lsn() wrote it; skip re-check
        // (item 86). D5 still enforced below via LSN comparison.
        let page = self.read_page_from_mmap_unchecked(page_id)?;

        // D5 (the invariant that must never break): a dirty page may not reach
        // disk while its LSN is ahead of the durable WAL frontier.
        if durable_wal_lsn != INVALID_LSN && page.lsn() > durable_wal_lsn {
            return Err(DbError::Recovery(format!(
                "D5 violation on flush: page {page_id} LSN {} > durable WAL LSN {durable_wal_lsn}",
                page.lsn()
            )));
        }

        // Fault injection (P1.b): fail before touching the mmap, poison the pool,
        // and — critically — do NOT mark the frame clean.
        if st.flush_fault_armed {
            st.flush_fault_armed = false;
            st.flush_poisoned = true;
            tracing::error!(
                page_id,
                "data-file flush fault injected — poisoning pool (P1.b)"
            );
            return Err(DbError::DurabilityFailure(format!(
                "injected data-file flush failure on page {page_id}"
            )));
        }

        let start = page_id as usize * self.page_size;
        let end = start + self.page_size;
        {
            let guard = self.mmap.read().map_err(|_| lock_poisoned())?;
            if let Err(e) = guard.flush_range(start, end - start) {
                drop(guard);
                st.flush_poisoned = true;
                return Err(DbError::DurabilityFailure(format!(
                    "data-file msync failed on page {page_id}: {e}"
                )));
            }
        }

        if let Some(&frame_idx) = st.frame_index.get(&page_id) {
            st.frames[frame_idx].dirty = false;
        }
        tracing::debug!(page_id, "page flushed to disk");
        Ok(())
    }

    /// Decrement pin count for a page.
    pub fn unpin(&self, page_id: PageId) {
        let mut st = self.lock_state();
        if let Some(&frame_idx) = st.frame_index.get(&page_id) {
            if st.frames[frame_idx].pin_count > 0 {
                st.frames[frame_idx].pin_count -= 1;
            }
        }
    }

    pub fn flush_all(&self, durable_wal_lsn: Lsn) -> Result<()> {
        let mut st = self.lock_state();
        // P1.b: a poisoned pool must never claim a successful flush.
        if st.flush_poisoned {
            return Err(DbError::DurabilityFailure(
                "buffer pool is poisoned by an earlier flush failure; flush_all cannot succeed"
                    .into(),
            ));
        }
        let dirty_pages: Vec<PageId> = st
            .frames
            .iter()
            .filter_map(|f| if f.dirty { f.page_id } else { None })
            .collect();
        for pid in dirty_pages {
            self.flush_locked(&mut st, pid, durable_wal_lsn)?;
        }
        Ok(())
    }

    /// Read one page directly from the shared mmap (no frame bookkeeping).
    /// Always verifies CRC — equivalent to a pool miss.
    pub fn read_page(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id, true)
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// Pool miss / flush path: read from mmap with CRC verification (item 86).
    fn read_page_from_mmap(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id, true)
    }

    /// Pool hit path: read from mmap WITHOUT CRC re-verification (item 86).
    /// Safe because the page was verified when it first entered the pool.
    fn read_page_from_mmap_unchecked(&self, page_id: PageId) -> Result<SlottedPage> {
        read_page_locked(&self.mmap, self.page_size, page_id, false)
    }

    /// Find a victim frame, flushing a durable dirty page back first if needed.
    /// Assumes the caller holds the state lock (passed in as `st`) — so D5 is
    /// enforced atomically with the eviction decision.
    ///
    /// Item 37 (lazy growth): sweeps only the currently-allocated frames
    /// (`st.frames.len()`), then grows the table by one frame if the ceiling
    /// (`self.capacity`) has not been reached rather than returning
    /// `BufferPoolFull` immediately. Growth is O(1) amortized (Vec doubling).
    fn find_victim(&self, st: &mut PoolState) -> Result<usize> {
        let current = st.frames.len();
        // Item 78: fast-path grow — if the pool is below capacity, push a new
        // frame immediately without running the O(current) clock sweep.
        //
        // Rationale: the clock sweep is for *eviction* (i.e., reclaiming a
        // slot when the pool is at capacity).  When current < capacity, we can
        // always grow, so sweeping is wasted work.  For a 100k-row batch
        // UPDATE that creates 383 fill pages, this changes Phase B from
        // O(pages × 383) loop iterations to O(1) per alloc — a measurable
        // speedup when many pages are already resident.  Eviction behaviour
        // (at capacity) is completely unchanged: the sweep only runs when
        // `current == capacity`.
        if current < self.capacity {
            let idx = current;
            st.frames.push(Frame::empty());
            return Ok(idx);
        }
        // Pool is full — run two full sweeps of the frame table to find a
        // victim via the standard second-chance clock algorithm.
        // `current` is captured before any potential growth so the loop stays bounded.
        for _ in 0..current.saturating_mul(2) {
            let idx = st.clock_hand % current;
            st.clock_hand = (idx + 1) % current;
            let (pinned, referenced, dirty, page_id) = {
                let f = &st.frames[idx];
                (f.pin_count > 0, f.clock_ref, f.dirty, f.page_id)
            };
            if pinned {
                continue;
            }
            if referenced {
                st.frames[idx].clock_ref = false;
                continue;
            }
            if dirty {
                let Some(pid) = page_id else {
                    return Ok(idx);
                };
                // Item 77: use the cached LSN from the frame rather than
                // re-reading the full 8 KiB page just to extract its LSN.
                // `frame.lsn` is set by every `write_page` call, so it is
                // always current.  This eliminates O(dirty_frames) mmap reads
                // per page fetch in the deferred-sync write workload.
                let page_lsn = st.frames[idx].lsn;
                // D5: a dirty page may be stolen only once its WAL is durable.
                if st.durable_wal_lsn == INVALID_LSN || page_lsn > st.durable_wal_lsn {
                    continue;
                }
                // D5 tripwire (P1.b): only reach here for a page at/behind the
                // durable frontier — assert it so a future filter change can't
                // silently steal a page ahead of the WAL.
                debug_assert!(
                    st.durable_wal_lsn != INVALID_LSN && page_lsn <= st.durable_wal_lsn,
                    "D5 violation at steal point: page {pid} LSN {page_lsn} > durable WAL {}",
                    st.durable_wal_lsn
                );
                let durable = st.durable_wal_lsn;
                self.flush_locked(st, pid, durable)?;
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

/// Compile-time proof that the concurrent buffer pool is shareable across
/// threads (P5.a) — the foundation of concurrent writers.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BufferPool>();
};

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
        let pool = open_pool(dir.path(), 16);
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
        let pool = open_pool(dir.path(), 16);
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
    #[test]
    fn fetch_for_write_forces_wal_sync_to_evict_nondurable_dirty_pages() {
        use crate::wal::Wal;
        let dir = tempdir().unwrap();
        let pool = open_pool(dir.path(), 2); // tiny: 2 frames
        let wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
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
        assert_eq!(wal.durable_lsn(), INVALID_LSN, "nothing fsynced yet");

        // A plain fetch can find no victim — every frame is dirty and ahead of
        // the (still-INVALID) durable frontier.
        let pid2 = pool.alloc_page().unwrap();
        assert!(
            matches!(pool.fetch_page(pid2), Err(DbError::BufferPoolFull)),
            "plain fetch must fail when all frames are dirty + non-durable"
        );

        // The write-path fetch forces a WAL sync and then succeeds.
        let _ = pool.fetch_page_for_write(pid2, &wal).unwrap();
        assert!(
            wal.durable_lsn() > INVALID_LSN,
            "fetch_for_write must have forced an fsync to make room"
        );
        let pid3 = pool.alloc_page().unwrap();
        pool.fetch_page_for_write(pid3, &wal).unwrap();
    }

    /// P1.a: `maybe_log_fpi` logs exactly one full-page image per page per
    /// checkpoint interval, and `clear_fpi_tracking` re-arms it.
    #[test]
    fn maybe_log_fpi_logs_once_per_interval_then_rearms_on_clear() {
        use crate::wal::Wal;
        let dir = tempdir().unwrap();
        let pool = open_pool(dir.path(), 16);
        let wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.insert(b"row").unwrap();
        page.set_lsn(1);
        pool.write_page(&page).unwrap();

        let (txn, begin) = wal.begin_mini_txn().unwrap();
        let first = pool.maybe_log_fpi(pid, &wal, txn, begin).unwrap();
        assert!(first.is_some(), "first touch must log an FPI");
        let second = pool.maybe_log_fpi(pid, &wal, txn, begin).unwrap();
        assert!(second.is_none(), "second touch must not re-log");

        pool.clear_fpi_tracking();
        let third = pool.maybe_log_fpi(pid, &wal, txn, begin).unwrap();
        assert!(
            third.is_some(),
            "post-checkpoint touch must log a fresh FPI"
        );
    }

    /// P1.a: `restore_page_image` overwrites a page from a raw image and extends
    /// the file when the image targets a page past the current end.
    #[test]
    fn restore_page_image_overwrites_torn_and_extends() {
        let dir = tempdir().unwrap();
        let pool = open_pool(dir.path(), 16);
        let ps = DEFAULT_PAGE_SIZE as usize;

        let far_pid: PageId = 5;
        let mut page = SlottedPage::new(far_pid, PAGE_TYPE_HEAP, ps);
        page.insert(b"restored").unwrap();
        page.set_lsn(9);
        let image = page.as_bytes().to_vec();

        pool.restore_page_image(far_pid, &image).unwrap();
        let read = pool.read_page(far_pid).unwrap();
        assert_eq!(read.get(0).unwrap(), b"restored");
        assert_eq!(read.lsn(), 9);

        assert!(pool.restore_page_image(far_pid, &image[..ps - 1]).is_err());
    }

    /// P1.b: an injected data-file flush failure poisons the pool — the flush
    /// returns a `DurabilityFailure`, the frame stays **dirty**, and every later
    /// flush keeps failing.
    #[test]
    fn flush_failure_poisons_and_keeps_page_dirty() {
        let dir = tempdir().unwrap();
        let pool = open_pool(dir.path(), 16);
        let pid = pool.alloc_page().unwrap();
        let _ = pool.fetch_page(pid).unwrap(); // bring pid into a frame
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.set_lsn(5);
        pool.write_page(&page).unwrap(); // marks the frame dirty

        pool.arm_flush_fault();
        let res = pool.flush_page(pid, 5);
        assert!(
            matches!(res, Err(DbError::DurabilityFailure(_))),
            "a failed data-file flush must surface a fatal DurabilityFailure, got {res:?}"
        );
        assert!(pool.is_flush_poisoned(), "pool must latch poisoned");
        // The frame must still be dirty — a poisoned flush cannot claim durable.
        {
            let st = pool.lock_state();
            assert!(
                st.frames[st.frame_index[&pid]].dirty,
                "frame must remain dirty after a failed flush"
            );
        }
        assert!(matches!(
            pool.flush_page(pid, 5),
            Err(DbError::DurabilityFailure(_))
        ));
        assert!(matches!(
            pool.flush_all(5),
            Err(DbError::DurabilityFailure(_))
        ));
    }

    /// P1.c: `alloc_page` grows the file in chunks, and a reopen reclaims
    /// trailing all-zero slack pages so ids stay contiguous.
    #[test]
    fn alloc_page_grows_in_chunks_and_reopen_reclaims_slack() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.db");
        let ps = DEFAULT_PAGE_SIZE as usize;
        {
            let pool = BufferPool::open(&path, ps, 16).unwrap();
            let mut ids = Vec::new();
            for _ in 0..3 {
                let pid = pool.alloc_page().unwrap();
                let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, ps);
                page.set_lsn(1);
                pool.write_page(&page).unwrap();
                ids.push(pid);
            }
            assert_eq!(ids, vec![0, 1, 2], "ids are contiguous from 0");
            assert_eq!(pool.page_count(), 3);
            let physical = std::fs::metadata(&path).unwrap().len() / ps as u64;
            assert!(
                physical >= 512,
                "file should be pre-grown by a chunk, got {physical} pages"
            );
        }
        let pool = BufferPool::open(&path, ps, 16).unwrap();
        assert_eq!(
            pool.page_count(),
            3,
            "slack pages must not inflate the count"
        );
        assert_eq!(
            pool.alloc_page().unwrap(),
            3,
            "next id continues contiguously"
        );
    }

    #[test]
    fn fetch_written_page() {
        let dir = tempdir().unwrap();
        let pool = open_pool(dir.path(), 16);
        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize);
        page.insert(b"test_row").unwrap();
        page.set_lsn(1);
        pool.write_page(&page).unwrap();
        let fetched = pool.fetch_page(pid).unwrap();
        assert_eq!(fetched.get(0).unwrap(), b"test_row");
    }

    /// P5.a: two threads writing distinct pages concurrently both succeed, and
    /// many threads writing the *same* page under its exclusive latch serialize
    /// without losing an update.
    #[test]
    fn concurrent_distinct_page_writes_and_same_page_latch() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        let pool = Arc::new(open_pool(dir.path(), 64));
        let ps = DEFAULT_PAGE_SIZE as usize;

        // Pre-allocate 8 pages.
        let mut pids = Vec::new();
        for _ in 0..8 {
            let pid = pool.alloc_page().unwrap();
            let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, ps);
            page.set_lsn(1);
            pool.write_page(&page).unwrap();
            pids.push(pid);
        }

        // 8 threads each own a distinct page and append a row under its latch.
        let mut handles = Vec::new();
        for &pid in &pids {
            let pool = Arc::clone(&pool);
            handles.push(std::thread::spawn(move || {
                let _lx = pool.latch_exclusive(pid);
                let mut page = pool.fetch_page(pid).unwrap();
                page.insert(format!("row-{pid}").as_bytes()).unwrap();
                page.set_lsn(2);
                pool.write_page(&page).unwrap();
                pool.unpin(pid);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        for &pid in &pids {
            let page = pool.read_page(pid).unwrap();
            assert_eq!(page.get(0).unwrap(), format!("row-{pid}").as_bytes());
        }

        // Same-page contention: N threads each append one row to ONE shared
        // page, each holding the exclusive latch for its whole read-modify-write.
        // All N rows must survive (no lost update).
        let shared = pids[0];
        {
            let mut page = SlottedPage::new(shared, PAGE_TYPE_HEAP, ps);
            page.set_lsn(3);
            pool.write_page(&page).unwrap();
        }
        let n = 50u16;
        let mut handles = Vec::new();
        for i in 0..n {
            let pool = Arc::clone(&pool);
            handles.push(std::thread::spawn(move || {
                let _lx = pool.latch_exclusive(shared);
                let mut page = pool.fetch_page(shared).unwrap();
                page.insert(format!("v{i}").as_bytes()).unwrap();
                page.set_lsn(4);
                pool.write_page(&page).unwrap();
                pool.unpin(shared);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let page = pool.read_page(shared).unwrap();
        assert_eq!(
            page.slot_count_pub(),
            n,
            "all {n} concurrent same-page inserts must survive under the latch"
        );
    }
}
