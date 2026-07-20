// Safe wrapper around memmap2::MmapMut.
// This is the ONLY module in the crate permitted to use `unsafe` (CLAUDE.md §4).
// Every `unsafe` block here documents the invariant it relies on.

#![allow(unsafe_code)] // sole permitted unsafe module (CLAUDE.md §4)

use std::fs::File;
use std::ops::{Deref, DerefMut};

use memmap2::{Advice, MmapMut};

use crate::error::Result;

pub struct PageFileMmap {
    inner: MmapMut,
}

impl PageFileMmap {
    /// Map the file for read-write access.
    ///
    /// # Safety invariant upheld by caller
    /// The `BufferPool` holds the only `File` handle to this path for the
    /// lifetime of the map; no other process writes to this file concurrently.
    pub fn new(file: &File) -> Result<Self> {
        // SAFETY: We own exclusive access to the file (enforced by the DB open
        // lock; single-writer single-file design, D6). The file is at least
        // one page long (guaranteed by BufferPool::open before mapping).
        let inner = unsafe { MmapMut::map_mut(file)? };
        Ok(Self { inner })
    }

    /// `msync` the given byte range to the backing file. An `Err` here is a
    /// durability failure: the caller (`BufferPool::flush_page`) treats it as
    /// fatal for the session (P1.b) — it does not mark the page clean and
    /// poisons the pool, because a failed `msync` may leave the OS having
    /// dropped the dirty page while clearing its dirty bit, so a retry could
    /// falsely succeed.
    pub fn flush_range(&self, offset: usize, len: usize) -> Result<()> {
        self.inner.flush_range(offset, len)?;
        Ok(())
    }

    /// Hint the OS to prefetch `len` bytes starting at `offset` (item 70 —
    /// sequential scan read-ahead).
    ///
    /// The call is **best-effort**: `MADV_WILLNEED` is a hint; the kernel may
    /// ignore it or honour it asynchronously. Any error is silently discarded —
    /// the hint never affects correctness.
    ///
    /// Only active on Unix (Linux + macOS) via `memmap2::Advice::WillNeed`.
    /// On other platforms this is a no-op.
    #[cfg(unix)]
    pub fn prefetch_range(&self, offset: usize, len: usize) {
        if len == 0 || offset >= self.inner.len() {
            return;
        }
        // Clamp so we never hint past the mapped region.
        let clamped_len = len.min(self.inner.len() - offset);
        // SAFETY: `advise_range` with `Advice::WillNeed` is a pure hint — it
        // does not modify the memory contents and cannot cause unsound reads or
        // writes.  `offset` and `clamped_len` are bounds-checked above to stay
        // within the mapped region.  The hint is safe to call from any thread
        // because the mmap address and length are stable for the lifetime of
        // `PageFileMmap` (remaps replace `self.inner` atomically under the
        // pool's mmap write-lock, not mid-call here).
        let _ = self
            .inner
            .advise_range(Advice::WillNeed, offset, clamped_len);
    }

    /// No-op on non-Unix platforms.
    #[cfg(not(unix))]
    pub fn prefetch_range(&self, _offset: usize, _len: usize) {}

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Deref for PageFileMmap {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.inner
    }
}

impl DerefMut for PageFileMmap {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.inner
    }
}
