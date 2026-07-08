//! Concurrent read handle (6b).
//!
//! A `Send + Sync + Clone` view for reads that run **off** the single writer
//! thread. It holds only shared state — the page-file mmap behind an
//! `Arc<RwLock<..>>` ([`SharedPageReader`]) and the transaction snapshot state
//! ([`SharedTxn`]) — so many readers execute in parallel with each other and
//! with the writer, coordinating solely through MVCC snapshots rather than the
//! writer's request channel.
//!
//! A read here allocates no xid and writes no WAL: [`txn::read_snapshot`] gives
//! a self-contained READ COMMITTED snapshot plus a sentinel `self_xid`, and the
//! bytes come straight from the shared mmap under its read-lock. This is the
//! payoff of the earlier interior-mutability groundwork: under MVCC a reader
//! genuinely needs nothing exclusive.
//!
//! v1 covers point reads (`get`). Concurrent SQL `SELECT` is the same pattern
//! layered on a shared catalog + a read-only executor path (tracked in
//! `docs/backlog/group_commit_and_read_concurrency.md`).

use crate::bufferpool::{PageReader, SharedPageReader};
use crate::error::{DbError, Result};
use crate::heap::RowId;
use crate::mvcc::is_visible;
use crate::txn::{self, SharedTxn};

/// A cloneable, thread-safe handle for concurrent reads (6b). Obtain one from
/// [`crate::Engine::read_handle`].
#[derive(Clone)]
pub struct ReadHandle {
    reader: SharedPageReader,
    txn: SharedTxn,
}

impl ReadHandle {
    pub(crate) fn new(reader: SharedPageReader, txn: SharedTxn) -> Self {
        Self { reader, txn }
    }

    /// Read one untyped row by [`RowId`] under a fresh READ COMMITTED snapshot,
    /// with no writer-thread involvement. Returns
    /// [`DbError::NoVisibleVersion`] if the tuple version at `row_id` is not
    /// visible to that snapshot (superseded, deleted, or never committed) —
    /// exactly the same visibility rule the writer-side `Engine::get` applies.
    pub fn get(&self, row_id: RowId) -> Result<Vec<u8>> {
        let (snapshot, self_xid) = txn::read_snapshot(&self.txn);
        let page = self.reader.read_page(row_id.page_id)?;
        let th = page.tuple_header(row_id.slot)?;
        if is_visible(th.xmin, th.xmax, &snapshot, self_xid) {
            Ok(page.get(row_id.slot)?.to_vec())
        } else {
            Err(DbError::NoVisibleVersion {
                page_id: row_id.page_id,
                slot: row_id.slot,
            })
        }
    }
}

/// Compile-time proof that a `ReadHandle` can be shared across threads — the
/// whole point of 6b. (`Engine` itself stays deliberately non-`Sync`.)
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ReadHandle>();
};
