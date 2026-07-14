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

    /// A durability primitive (WAL `fsync` or data-file `msync`) failed
    /// (P1.b — fsyncgate). This is **fatal for the session**: on many
    /// platforms a failed `fsync`/`msync` may leave the OS having dropped the
    /// dirty page while clearing its dirty bit, so a later retry can return
    /// success without the data ever reaching disk. Rather than risk silently
    /// reporting durable when it is not, the failing component (`Wal` /
    /// `BufferPool`) latches into a poisoned state and returns this error for
    /// every subsequent durability request. The correct remedy is a
    /// process-level restart + recovery, not retrying in the same session.
    #[error("durability failure (fatal for this session): {0}")]
    DurabilityFailure(String),

    #[error("control file corrupt: {0}")]
    ControlFileCorrupt(String),

    #[error("write conflict: record already locked by transaction {holder_xid}")]
    WriteConflict { holder_xid: u64 },

    #[error("could not serialize access due to concurrent update (xid {xid})")]
    SerializationFailure { xid: u64 },

    #[error("deadlock detected: transaction {xid} chosen as victim to break a wait-for cycle")]
    Deadlock { xid: u64 },

    #[error("query exceeded its time limit of {limit_ms} ms")]
    QueryTimeout { limit_ms: u64 },

    #[error("query was cancelled")]
    QueryCancelled,

    #[error("transaction {xid} is not active")]
    TxnNotActive { xid: u64 },

    #[error("transaction {xid} already committed or aborted")]
    TxnAlreadyFinished { xid: u64 },

    #[error("no visible version of row ({page_id}, {slot}) under current snapshot")]
    NoVisibleVersion { page_id: u32, slot: u16 },

    #[error("SQL parse error: {0}")]
    SqlParse(String),

    #[error("SQL planning error: {0}")]
    SqlPlan(String),

    #[error("unsupported SQL feature: {0}")]
    SqlUnsupported(String),

    #[error("table '{0}' not found")]
    TableNotFound(String),

    #[error("table '{0}' already exists")]
    TableAlreadyExists(String),

    #[error("column '{column}' not found on table '{table}'")]
    ColumnNotFound { table: String, column: String },

    #[error("catalog is corrupt: {0}")]
    CatalogCorrupt(String),

    #[error("NOT NULL constraint violated: column '{column}' on table '{table}' cannot be NULL")]
    NotNullViolation { table: String, column: String },

    #[error("UNIQUE constraint violated on table '{table}' column(s) [{columns}]")]
    UniqueViolation { table: String, columns: String },

    #[error("CHECK constraint violated on table '{table}'")]
    CheckViolation { table: String },

    #[error("{}", fk_violation_msg(table, ref_table, column, value))]
    ForeignKeyViolation {
        table: String,
        ref_table: String,
        /// Present for row-level violations (item 36); `None` for the legacy
        /// table-existence-only check (keeps existing callers unbroken).
        column: Option<String>,
        /// String-encoded FK value that had no matching parent row.
        value: Option<String>,
    },

    /// Only ever produced by the optional M5 server layer
    /// (`src/server/engine_handle.rs`): the dedicated writer thread that
    /// owns the `Engine` has stopped responding (its channel is closed —
    /// most likely it panicked). There is no in-process recovery from this;
    /// per M5's design notes, the expected remedy is a process-level
    /// restart, not retrying against the same `EngineHandle`.
    #[error("engine is unavailable: the writer thread has stopped responding")]
    EngineUnavailable,

    /// Replication / slot management error (P6.b): e.g. a slot name already
    /// exists, is unknown, or a slots-file (de)serialization failed.
    #[error("replication error: {0}")]
    Replication(String),

    /// Authorization error (P6.e): a bad users/roles/GRANT statement (unknown
    /// grantee, duplicate user, etc.).
    #[error("authorization error: {0}")]
    Authz(String),

    /// Permission denied (P6.e): the current user lacks the required privilege.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

fn fk_violation_msg(
    table: &str,
    ref_table: &str,
    column: &Option<String>,
    value: &Option<String>,
) -> String {
    match (column, value) {
        (Some(col), Some(val)) => format!(
            "FOREIGN KEY constraint violated on table '{table}': \
             column '{col}' value {val} has no matching row in '{ref_table}'"
        ),
        _ => format!(
            "FOREIGN KEY constraint violated on table '{table}': \
             referenced table '{ref_table}' does not exist"
        ),
    }
}
