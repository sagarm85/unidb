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

use std::sync::{Arc, RwLock};

use crate::bufferpool::{PageReader, SharedPageReader};
use crate::catalog::Catalog;
use crate::error::{DbError, Result};
use crate::heap::RowId;
use crate::mvcc::is_visible;
use crate::sql::executor::{exec_select_readonly, plan_is_concurrent_read, ExecResult};
use crate::sql::logical::{apply_rls, LogicalPlan};
use crate::sql::parser::parse_sql;
use crate::txn::{self, SharedTxn};

/// Whether every statement in `sql` can run on the concurrent read path (6b):
/// a non-empty batch of plain `SELECT`s with no `NEAR`. Lets a caller (the
/// server's `POST /sql`) route reads to [`ReadHandle::execute_sql`] and
/// everything else (writes, DDL, NEAR) to the single writer thread. Parse
/// failures return `false` so the writer path can surface the real error.
pub fn is_concurrent_read_sql(sql: &str) -> bool {
    match parse_sql(sql) {
        Ok(plans) => !plans.is_empty() && plans.iter().all(plan_is_concurrent_read),
        Err(_) => false,
    }
}

/// A cloneable, thread-safe handle for concurrent reads (6b). Obtain one from
/// [`crate::Engine::read_handle`].
#[derive(Clone)]
pub struct ReadHandle {
    reader: SharedPageReader,
    txn: SharedTxn,
    catalog: Arc<RwLock<Catalog>>,
}

impl ReadHandle {
    pub(crate) fn new(
        reader: SharedPageReader,
        txn: SharedTxn,
        catalog: Arc<RwLock<Catalog>>,
    ) -> Self {
        Self {
            reader,
            txn,
            catalog,
        }
    }

    /// Execute one or more **read-only** SQL statements on the concurrent read
    /// path (6b): each must be a plain `SELECT` (no writes, DDL, or `NEAR` —
    /// NEAR needs the HNSW index fast path, which stays on the writer thread).
    /// Reuses the writer-side decode/predicate/projection logic via
    /// [`exec_select_readonly`], sourcing pages from the shared mmap and the
    /// snapshot from shared txn state — no xid, no WAL, no writer round-trip.
    /// Returns [`DbError::SqlPlan`] if a statement is not concurrent-readable,
    /// so the server can route such statements to the writer path instead.
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<ExecResult>> {
        let plans = parse_sql(sql)?;
        let mut out = Vec::with_capacity(plans.len());
        for plan in plans {
            let plan = apply_rls(plan, &self.catalog_read());
            if !plan_is_concurrent_read(&plan) {
                return Err(DbError::SqlPlan(
                    "read handle executes only plain read-only SELECT (no writes, DDL, or NEAR)"
                        .into(),
                ));
            }
            let LogicalPlan::Select {
                table,
                projection,
                predicate,
            } = &plan
            else {
                unreachable!("plan_is_concurrent_read guarantees a Select");
            };
            let (snapshot, self_xid) = txn::read_snapshot(&self.txn);
            out.push(exec_select_readonly(
                table,
                projection,
                predicate,
                &self.catalog_read(),
                &snapshot,
                self_xid,
                &self.reader,
            )?);
        }
        Ok(out)
    }

    fn catalog_read(&self) -> std::sync::RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
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
