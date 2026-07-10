//! Parallel scan workers (Milestone P).
//!
//! The one place unidb was clearly behind Postgres is raw scan throughput at
//! scale — Postgres runs a *parallel* sequential scan. This module partitions a
//! table's pages across `std::thread::scope` workers (NOT tokio — §4) that each
//! read the shared mmap (`SharedPageReader`) under its read-lock and filter by
//! the same MVCC snapshot the serial path uses.
//!
//! Correctness rests on unidb's **mmap-as-storage** model: the buffer pool
//! writes page mutations directly into the mmap under its write-lock, and a
//! reader takes an *owned copy* under the read-lock — so a worker always sees
//! current committed data (exactly what the shipped `ReadHandle` (6b) relies
//! on). There is no pool-vs-disk staleness to reconcile. Read-only: no WAL, no
//! recovery, no on-disk format change.
//!
//! **Default off** behind a runtime toggle + env (`UNIDB_PARALLEL_SCAN`), so it
//! ships dark and is flipped after a soak — mirroring `UNIDB_CONCURRENT_SQL_WRITES`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use crate::bufferpool::SharedPageReader;
use crate::error::{DbError, Result};
use crate::format::{PageId, Xid};
use crate::heap::{count_page_visible, scan_page_into, RowId};
use crate::mvcc::Snapshot;
use crate::sql::logical::Literal;

// ── config (runtime toggle + lazy env defaults) ──────────────────────────────

static ENABLED: AtomicBool = AtomicBool::new(false);
static MIN_PAGES: AtomicUsize = AtomicUsize::new(64);
static MAX_WORKERS: AtomicUsize = AtomicUsize::new(0); // 0 → available_parallelism
static INIT: Once = Once::new();

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn init_from_env() {
    INIT.call_once(|| {
        if std::env::var("UNIDB_PARALLEL_SCAN").as_deref() == Ok("1") {
            ENABLED.store(true, Ordering::Relaxed);
        }
        MIN_PAGES.store(env_usize("UNIDB_PARALLEL_MIN_PAGES", 64), Ordering::Relaxed);
        MAX_WORKERS.store(
            env_usize("UNIDB_PARALLEL_MAX_WORKERS", 0),
            Ordering::Relaxed,
        );
    });
}

/// Enable/disable parallel scan at runtime (the field-revert safety net).
pub fn set_enabled(on: bool) {
    init_from_env();
    ENABLED.store(on, Ordering::Relaxed);
}

/// Minimum page count before a scan is worth parallelizing (below it, the serial
/// fast path is byte-for-byte unchanged).
pub fn set_min_pages(n: usize) {
    init_from_env();
    MIN_PAGES.store(n, Ordering::Relaxed);
}

/// Cap on worker threads (0 = use `available_parallelism`).
pub fn set_max_workers(n: usize) {
    init_from_env();
    MAX_WORKERS.store(n, Ordering::Relaxed);
}

/// The worker count to use for a scan over `n_pages`, or `None` to stay serial
/// (disabled, below the page threshold, or only one worker would result). Never
/// more workers than pages.
pub fn degree_for(n_pages: usize) -> Option<usize> {
    init_from_env();
    if !ENABLED.load(Ordering::Relaxed) || n_pages < MIN_PAGES.load(Ordering::Relaxed) {
        return None;
    }
    let cap = MAX_WORKERS.load(Ordering::Relaxed);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cap = if cap == 0 { cores } else { cap.min(cores) };
    let degree = cap.max(1).min(n_pages);
    (degree > 1).then_some(degree)
}

// ── the workers ──────────────────────────────────────────────────────────────

/// Parallel `COUNT(*)` (P-a): partition `pages` across `degree` workers, each
/// counting visible slots via headers (no decode), and sum the partials.
pub fn parallel_count(
    pages: &[PageId],
    reader: &SharedPageReader,
    snapshot: &Snapshot,
    self_xid: Xid,
    degree: usize,
) -> Result<usize> {
    let cursor = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let err: Mutex<Option<DbError>> = Mutex::new(None);
    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, total, err, stop, pages) = (&cursor, &total, &err, &stop, pages);
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() {
                        break;
                    }
                    match count_page_visible(&reader, pages[i], snapshot, self_xid) {
                        Ok(c) => {
                            total.fetch_add(c, Ordering::Relaxed);
                        }
                        Err(e) => {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            });
        }
    });

    if let Some(e) = err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    Ok(total.load(Ordering::Relaxed))
}

/// Parallel scan + filter + project (P-b): each worker scans its page slice and
/// runs `per_row` (the caller's B2 deform + predicate + project) on every
/// visible tuple, keeping the survivors. Results are **concatenated**
/// (order-agnostic — the caller must have no `ORDER BY`). Returns the projected
/// rows and their `RowId`s (the read set, for SSI note-reads).
///
/// `per_row` returns `Ok(Some(row))` to keep a projected row, `Ok(None)` to drop
/// it (predicate rejected), or `Err` to abort the whole scan. It must be `Sync`
/// (shared across workers) — it captures only immutable schema/predicate state.
pub fn parallel_filter_project<F>(
    pages: &[PageId],
    reader: &SharedPageReader,
    snapshot: &Snapshot,
    self_xid: Xid,
    degree: usize,
    per_row: &F,
) -> Result<(Vec<Vec<Literal>>, Vec<RowId>)>
where
    F: Fn(RowId, &[u8]) -> Result<Option<Vec<Literal>>> + Sync,
{
    let cursor = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let err: Mutex<Option<DbError>> = Mutex::new(None);
    #[allow(clippy::type_complexity)]
    let parts: Mutex<Vec<(Vec<Vec<Literal>>, Vec<RowId>)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, stop, err, parts, pages) = (&cursor, &stop, &err, &parts, pages);
            s.spawn(move || {
                let mut rows: Vec<Vec<Literal>> = Vec::new();
                let mut ids: Vec<RowId> = Vec::new();
                let mut page_buf: Vec<(RowId, Vec<u8>)> = Vec::new();
                'outer: while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() {
                        break;
                    }
                    page_buf.clear();
                    if let Err(e) =
                        scan_page_into(&reader, pages[i], snapshot, self_xid, &mut page_buf)
                    {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    for (rid, bytes) in page_buf.drain(..) {
                        match per_row(rid, &bytes) {
                            Ok(Some(row)) => {
                                rows.push(row);
                                ids.push(rid);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                stop.store(true, Ordering::Relaxed);
                                break 'outer;
                            }
                        }
                    }
                }
                parts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((rows, ids));
            });
        }
    });

    if let Some(e) = err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    let mut all_rows = Vec::new();
    let mut all_ids = Vec::new();
    for (rows, ids) in parts.into_inner().unwrap_or_else(|p| p.into_inner()) {
        all_rows.extend(rows);
        all_ids.extend(ids);
    }
    Ok((all_rows, all_ids))
}
