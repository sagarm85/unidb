// Safe wrapper around memmap2::MmapMut.
// This is the ONLY module in the crate permitted to use `unsafe` (CLAUDE.md §4).
// Every `unsafe` block here documents the invariant it relies on.

#![allow(unsafe_code)] // sole permitted unsafe module (CLAUDE.md §4)

use std::fs::File;
use std::ops::{Deref, DerefMut};

use memmap2::MmapMut;

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

    pub fn flush_range(&self, offset: usize, len: usize) -> Result<()> {
        self.inner.flush_range(offset, len)?;
        Ok(())
    }

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
