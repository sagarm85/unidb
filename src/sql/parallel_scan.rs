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
//! **Default on** since worker governance (backlog item 15): a **global worker
//! budget** (`GLOBAL_MAX`/`AVAILABLE` + [`acquire`]/[`WorkerLease`]) bounds total
//! live workers across all concurrent queries — extra scans degrade to serial
//! instead of oversubscribing — and workers honor the query deadline/cancellation
//! (`snapshot_deadline`). `UNIDB_PARALLEL_SCAN=0` / `Engine::set_parallel_scan(false)`
//! remain the field-revert net.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use crate::bufferpool::SharedPageReader;
use crate::error::{DbError, Result};
use crate::format::{PageId, Xid};
use crate::heap::{count_page_visible, get_visible, scan_page_into, RowId};
use crate::mvcc::Snapshot;
use crate::sql::logical::Literal;

// ── config (runtime toggle + lazy env defaults) ──────────────────────────────

// Default-ON since governance (item 15): the global worker budget bounds
// oversubscription and workers honor timeout/cancellation, so it's safe to run
// by default. `UNIDB_PARALLEL_SCAN=0` / `Engine::set_parallel_scan(false)` remain
// the field-revert net.
static ENABLED: AtomicBool = AtomicBool::new(true);
static MIN_PAGES: AtomicUsize = AtomicUsize::new(64);
static MAX_WORKERS: AtomicUsize = AtomicUsize::new(0); // per-query cap; 0 → cores
/// The **global** worker budget (the whole point of governance): the total number
/// of parallel-scan worker threads that may be live across *all* queries at once.
/// Bounds oversubscription — a busy server with many concurrent scans can never
/// spawn more than this, extra queries degrade to serial instead. Set once at
/// init; `AVAILABLE` is the live remaining count (acquire/release).
static GLOBAL_MAX: AtomicUsize = AtomicUsize::new(0);
static AVAILABLE: AtomicUsize = AtomicUsize::new(0);
static INIT: Once = Once::new();

fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn init_from_env() {
    INIT.call_once(|| {
        if std::env::var("UNIDB_PARALLEL_SCAN").as_deref() == Ok("0") {
            ENABLED.store(false, Ordering::Relaxed);
        } else if std::env::var("UNIDB_PARALLEL_SCAN").as_deref() == Ok("1") {
            ENABLED.store(true, Ordering::Relaxed);
        }
        MIN_PAGES.store(env_usize("UNIDB_PARALLEL_MIN_PAGES", 64), Ordering::Relaxed);
        MAX_WORKERS.store(
            env_usize("UNIDB_PARALLEL_MAX_WORKERS", 0),
            Ordering::Relaxed,
        );
        let global = env_usize("UNIDB_PARALLEL_MAX_TOTAL_WORKERS", cores()).max(1);
        GLOBAL_MAX.store(global, Ordering::Relaxed);
        AVAILABLE.store(global, Ordering::Relaxed);
    });
}

/// Set the global worker budget at runtime (0 → `available_parallelism`).
pub fn set_max_total_workers(n: usize) {
    init_from_env();
    let n = if n == 0 { cores() } else { n }.max(1);
    // Reset the live pool to the new ceiling (safe between scans; a scan in flight
    // holds its own permits and releases them via its lease regardless).
    let prev = GLOBAL_MAX.swap(n, Ordering::Relaxed);
    let inflight = prev.saturating_sub(AVAILABLE.load(Ordering::Relaxed));
    AVAILABLE.store(n.saturating_sub(inflight), Ordering::Relaxed);
}

/// An admission lease for `degree` parallel-scan workers, taken from the global
/// budget and **released on drop** (so permits return even on an early `?`
/// error). Held for the duration of a `parallel_*` call.
pub struct WorkerLease {
    granted: usize,
}

impl WorkerLease {
    pub fn degree(&self) -> usize {
        self.granted
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        AVAILABLE.fetch_add(self.granted, Ordering::Relaxed);
    }
}

/// Take up to `want` permits from the global pool (non-blocking — grabs whatever
/// is free right now, `0..=want`).
fn take_from_pool(want: usize) -> usize {
    loop {
        let cur = AVAILABLE.load(Ordering::Relaxed);
        let grant = want.min(cur);
        if grant == 0 {
            return 0;
        }
        if AVAILABLE
            .compare_exchange_weak(cur, cur - grant, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return grant;
        }
    }
}

/// Admission control (replaces a bare `degree_for` at call sites): gate on
/// config plus the page/candidate threshold, then reserve workers from the
/// **global** budget. Returns a lease with `>= 2` workers (released on drop),
/// or `None` to run serial — disabled, below threshold, or the global pool is
/// busy right now — so a flood of concurrent scans stays bounded instead of
/// oversubscribing.
pub fn acquire(n_units: usize) -> Option<WorkerLease> {
    let want = degree_for(n_units)?;
    let granted = take_from_pool(want);
    if granted >= 2 {
        Some(WorkerLease { granted })
    } else {
        if granted > 0 {
            AVAILABLE.fetch_add(granted, Ordering::Relaxed);
        }
        None
    }
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
    let ncores = cores();
    let cap = if cap == 0 { ncores } else { cap.min(ncores) };
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
    let deadline = crate::query_limits::snapshot_deadline();

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, total, err, stop, deadline, pages) =
                (&cursor, &total, &err, &stop, &deadline, pages);
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() {
                        break;
                    }
                    if i % 4 == 0 {
                        if let Err(e) = deadline.check() {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
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

/// Parallel index-candidate resolution: partition a B-tree/index candidate
/// `RowId` list across `degree` workers, each resolving its slice
/// (`get_visible` → `per_candidate` = the B2 deform + predicate re-check +
/// project) and keeping the survivors; gather = **concat** (order-agnostic —
/// the caller has no `ORDER BY`). This is the filtered-`SELECT` hot path: a range
/// predicate is served by the index, so the query is a *candidate* list to
/// resolve (random `heap.get` + `body` decode per row), not a page scan — and
/// that per-candidate work parallelizes just like the page scan does.
pub fn parallel_resolve_candidates<F>(
    candidates: &[RowId],
    reader: &SharedPageReader,
    snapshot: &Snapshot,
    self_xid: Xid,
    degree: usize,
    per_candidate: &F,
) -> Result<Vec<Vec<Literal>>>
where
    F: Fn(RowId, &[u8]) -> Result<Option<Vec<Literal>>> + Sync,
{
    let cursor = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let err: Mutex<Option<DbError>> = Mutex::new(None);
    let parts: Mutex<Vec<Vec<Vec<Literal>>>> = Mutex::new(Vec::new());
    let deadline = crate::query_limits::snapshot_deadline();

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, stop, err, parts, deadline, candidates) =
                (&cursor, &stop, &err, &parts, &deadline, candidates);
            s.spawn(move || {
                let mut rows: Vec<Vec<Literal>> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= candidates.len() {
                        break;
                    }
                    if i % 64 == 0 {
                        if let Err(e) = deadline.check() {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    let rid = candidates[i];
                    let bytes = match get_visible(&reader, rid, snapshot, self_xid) {
                        Ok(Some(b)) => b,
                        Ok(None) => continue, // superseded / vacuumed / uncommitted hint
                        Err(e) => {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    };
                    match per_candidate(rid, &bytes) {
                        Ok(Some(row)) => rows.push(row),
                        Ok(None) => {}
                        Err(e) => {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                parts.lock().unwrap_or_else(|p| p.into_inner()).push(rows);
            });
        }
    });

    if let Some(e) = err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    let mut all_rows = Vec::new();
    for rows in parts.into_inner().unwrap_or_else(|p| p.into_inner()) {
        all_rows.extend(rows);
    }
    Ok(all_rows)
}

/// Parallel filtered `COUNT(*)` — **partial aggregate** (Milestone P follow-up):
/// each worker scans its page slice, evaluates `matches` on every visible tuple,
/// and counts the survivors; the gather **sums** the partials. This is the lever
/// that lifts a `COUNT(*) WHERE <predicate>` from the base-scan-only speedup
/// (Filter + Aggregate were a serial Amdahl tail) toward the near-linear speedup
/// the unfiltered count already gets — the whole scan → filter → count runs in
/// the workers. `matches` must be `Sync` (a subquery-free predicate over the
/// pure `eval_qexpr`); it returns `Err` to abort.
pub fn parallel_count_matching<F>(
    pages: &[PageId],
    reader: &SharedPageReader,
    snapshot: &Snapshot,
    self_xid: Xid,
    degree: usize,
    matches: &F,
) -> Result<usize>
where
    F: Fn(&[u8]) -> Result<bool> + Sync,
{
    let cursor = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let err: Mutex<Option<DbError>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let deadline = crate::query_limits::snapshot_deadline();

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, total, err, stop, deadline, pages) =
                (&cursor, &total, &err, &stop, &deadline, pages);
            s.spawn(move || {
                let mut local = 0usize;
                let mut page_buf: Vec<(RowId, Vec<u8>)> = Vec::new();
                'outer: while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() {
                        break;
                    }
                    if i % 4 == 0 {
                        if let Err(e) = deadline.check() {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    page_buf.clear();
                    if let Err(e) =
                        scan_page_into(&reader, pages[i], snapshot, self_xid, &mut page_buf)
                    {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    for (_, bytes) in page_buf.drain(..) {
                        match matches(&bytes) {
                            Ok(true) => local += 1,
                            Ok(false) => {}
                            Err(e) => {
                                *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                stop.store(true, Ordering::Relaxed);
                                break 'outer;
                            }
                        }
                    }
                }
                total.fetch_add(local, Ordering::Relaxed);
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
    let deadline = crate::query_limits::snapshot_deadline();

    std::thread::scope(|s| {
        for _ in 0..degree {
            let reader = reader.clone();
            let (cursor, stop, err, parts, deadline, pages) =
                (&cursor, &stop, &err, &parts, &deadline, pages);
            s.spawn(move || {
                let mut rows: Vec<Vec<Literal>> = Vec::new();
                let mut ids: Vec<RowId> = Vec::new();
                let mut page_buf: Vec<(RowId, Vec<u8>)> = Vec::new();
                'outer: while !stop.load(Ordering::Relaxed) {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() {
                        break;
                    }
                    if i % 4 == 0 {
                        if let Err(e) = deadline.check() {
                            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            stop.store(true, Ordering::Relaxed);
                            break 'outer;
                        }
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
