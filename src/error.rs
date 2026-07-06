use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad magic number: expected {expected:#010x}, got {got:#010x}")]
    BadMagic { expected: u32, got: u32 },

    #[error("unsupported format version: {0}")]
    BadVersion(u16),

    #[error("invalid page size {0}: must be power-of-two in [4096, 65536]")]
    BadPageSize(u32),

    #[error("page checksum mismatch on page {page_id}")]
    ChecksumMismatch { page_id: u32 },

    #[error("WAL record at LSN {lsn} is corrupt")]
    WalCorrupt { lsn: u64 },

    #[error("buffer pool: no free frames (all pinned)")]
    BufferPoolFull,

    #[error("page {page_id} not found in buffer pool")]
    PageNotFound { page_id: u32 },

    #[error("heap is full: no space for tuple of {size} bytes")]
    HeapFull { size: usize },

    #[error("slot {slot} out of range on page {page_id}")]
    SlotOutOfRange { page_id: u32, slot: u16 },

    #[error("tuple at ({page_id}, {slot}) has been deleted")]
    TupleDeleted { page_id: u32, slot: u16 },

    #[error("recovery error: {0}")]
    Recovery(String),

    #[error("control file corrupt: {0}")]
    ControlFileCorrupt(String),
}

pub type Result<T> = std::result::Result<T, DbError>;
