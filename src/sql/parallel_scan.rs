//! Parallel scan workers (Milestone P).
//!
//! The one place unidb was clearly behind Postgres is raw scan throughput at
//! scale — Postgres runs a *parallel* sequential scan. This module partitions a
//! table's pages across workers (NOT tokio — §4) that each read the shared mmap
//! (`SharedPageReader`) under its read-lock and filter by the same MVCC snapshot
//! the serial path uses.
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
//!
//! **Item 45 lever 2 — pre-spawned pool**: workers are spawned once at `INIT`
//! time and park on a condvar between queries. A query posts a job and waits on
//! `done_cond` until all `degree` workers finish, eliminating the per-query
//! OS thread-creation cost (~50 µs × N threads).
//!
//! ## Pool safety note
//!
//! `run_in_pool` dispatches closures that may capture non-`'static` references
//! (e.g. `&[ColumnDef]` from the executor). `transmute_job_lifetime` extends
//! the closure's lifetime to `'static` for storage in the pool's Arc slot. This
//! is sound because:
//! 1. `run_in_pool` blocks (on `done_cond`) until **every** worker invocation
//!    has returned — the actual borrows are valid throughout.
//! 2. `guard.job = None` is set before `run_in_pool` returns, so the transmuted
//!    Arc cannot outlive the closure's real lifetime.
//! 3. An entry mutex (`PoolInner::entry`) serialises concurrent `run_in_pool`
//!    callers so at most one active job exists in the pool at any time.
//!
//! Same technique as `std::thread::scope` and `rayon::scope` (both use
//! `unsafe` internally for this lifetime extension). The crate uses
//! `#![deny(unsafe_code)]` (not `forbid`), so the `#[allow(unsafe_code)]` on
//! the single function `transmute_job_lifetime` is permitted.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};

use crate::bufferpool::SharedPageReader;
use crate::error::{DbError, Result};
use crate::format::{PageId, Xid};
use crate::heap::{count_page_visible, get_visible, scan_page_into, RowId};
use crate::mvcc::Snapshot;
use crate::sql::logical::Literal;

// ── config (runtime toggle + lazy env defaults) ──────────────────────────────

static ENABLED: AtomicBool = AtomicBool::new(true);
static MIN_PAGES: AtomicUsize = AtomicUsize::new(64);

/// Item 45: minimum candidate count before candidate-resolution parallelises.
pub const PARALLEL_CANDIDATE_MIN: usize = 64;
static MAX_WORKERS: AtomicUsize = AtomicUsize::new(0); // per-query cap; 0 → cores
/// The **global** worker budget: total parallel-scan workers live across all
/// queries at once. `AVAILABLE` is the live remaining count (acquire/release).
static GLOBAL_MAX: AtomicUsize = AtomicUsize::new(0);
static AVAILABLE: AtomicUsize = AtomicUsize::new(0);
static INIT: Once = Once::new();

// ── worker-governance observability (item 21) ────────────────────────────────

static PARALLEL_SCANS: AtomicU64 = AtomicU64::new(0);
static WORKERS_GRANTED: AtomicU64 = AtomicU64::new(0);
static SERIAL_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Worker-governance snapshot (item 21).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct WorkerStats {
    pub global_max: usize,
    pub available: usize,
    pub parallel_scans: u64,
    pub workers_granted: u64,
    pub serial_fallbacks: u64,
}

pub fn worker_stats() -> WorkerStats {
    init_from_env();
    WorkerStats {
        global_max: GLOBAL_MAX.load(Ordering::Relaxed),
        available: AVAILABLE.load(Ordering::Relaxed),
        parallel_scans: PARALLEL_SCANS.load(Ordering::Relaxed),
        workers_granted: WORKERS_GRANTED.load(Ordering::Relaxed),
        serial_fallbacks: SERIAL_FALLBACKS.load(Ordering::Relaxed),
    }
}

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

// ── pre-spawned worker pool (item 45 lever 2) ────────────────────────────────

struct PoolState {
    generation: u64,
    degree: usize,
    job: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    finished: usize,
    shutdown: bool,
}

struct PoolInner {
    /// Serialises `run_in_pool` callers so the single (generation, finished,
    /// job) slot is never raced by two concurrent callers. In practice the
    /// AVAILABLE governance already limits concurrent parallel scans, so this
    /// mutex rarely causes contention.
    entry: Mutex<()>,
    state: Mutex<PoolState>,
    /// Workers park here between generations.
    work_cond: Condvar,
    /// Caller parks here until `finished == degree`.
    done_cond: Condvar,
    /// Workers actually spawned (≤ GLOBAL_MAX at init time).
    size: usize,
}

static POOL: OnceLock<Arc<PoolInner>> = OnceLock::new();

fn worker_main(inner: Arc<PoolInner>, index: usize) {
    let mut my_gen: u64 = 0;
    loop {
        let job = {
            let mut guard = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if guard.shutdown {
                    return;
                }
                if guard.generation != my_gen {
                    let gen = guard.generation;
                    if index < guard.degree {
                        // This worker is in the active set for this generation.
                        my_gen = gen;
                        break guard.job.as_ref().unwrap().clone();
                    } else {
                        // Not in this batch: advance generation and wait.
                        my_gen = gen;
                    }
                }
                guard = inner
                    .work_cond
                    .wait(guard)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        job();
        {
            let mut guard = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.finished += 1;
            if guard.finished == guard.degree {
                inner.done_cond.notify_one();
            }
        }
    }
}

/// Extend a closure's lifetime to `'static` for pool dispatch.
///
/// # Safety
/// Only called from `run_in_pool`, which:
/// - blocks on `done_cond` until all worker invocations return,
/// - sets `guard.job = None` before returning (drops the transmuted Arc).
///
/// The closure (and any non-`'static` data it captures) is therefore valid for
/// the entire window in which the transmuted reference can be accessed.
/// Same soundness basis as `std::thread::scope` and `rayon::scope`.
#[allow(unsafe_code)]
fn transmute_job_lifetime<F: Fn() + Send + Sync>(f: F) -> Arc<dyn Fn() + Send + Sync + 'static> {
    let erased: Arc<dyn Fn() + Send + Sync + '_> = Arc::new(f);
    // Arc<dyn Fn() + '_> and Arc<dyn Fn() + 'static> have identical
    // representations (data ptr + vtable ptr). The transmute is a no-op in
    // machine code; it only removes the compile-time lifetime restriction.
    // SAFETY: see doc comment above.
    unsafe {
        std::mem::transmute::<Arc<dyn Fn() + Send + Sync + '_>, Arc<dyn Fn() + Send + Sync + 'static>>(
            erased,
        )
    }
}

/// Post `f` to the pre-spawned pool and block until all `degree` workers finish.
fn run_in_pool<F: Fn() + Send + Sync>(degree: usize, f: F) {
    let inner = POOL
        .get()
        .expect("parallel pool not yet initialised — call init_from_env first");
    // Serialise concurrent callers (see PoolInner::entry doc).
    let _entry = inner.entry.lock().unwrap_or_else(|p| p.into_inner());

    let real_degree = degree.min(inner.size);
    if real_degree < 2 {
        // Safety net: pool smaller than requested. acquire() guarantees ≥ 2.
        f();
        return;
    }

    let job = transmute_job_lifetime(f);
    let mut guard = inner.state.lock().unwrap_or_else(|p| p.into_inner());
    guard.job = Some(job);
    guard.degree = real_degree;
    guard.finished = 0;
    guard.generation = guard.generation.wrapping_add(1);
    inner.work_cond.notify_all();
    while guard.finished < guard.degree {
        guard = inner
            .done_cond
            .wait(guard)
            .unwrap_or_else(|p| p.into_inner());
    }
    // Clear the Arc before returning: the transmuted 'static ref must not
    // outlive the closure's actual lifetime.
    guard.job = None;
    // _entry drops here, releasing the entry serialisation lock.
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

        // Item 45 lever 2: spawn the pre-warmed worker pool.
        // Workers park on work_cond between queries; spawning them here pays
        // the OS thread-creation cost once instead of per query.
        let mut spawned = 0usize;
        let proto = Arc::new(PoolInner {
            entry: Mutex::new(()),
            state: Mutex::new(PoolState {
                generation: 0,
                degree: 0,
                job: None,
                finished: 0,
                shutdown: false,
            }),
            work_cond: Condvar::new(),
            done_cond: Condvar::new(),
            size: global, // corrected below if any spawn fails
        });
        for idx in 0..global {
            let w = Arc::clone(&proto);
            if std::thread::Builder::new()
                .name(format!("unidb-scan-{idx}"))
                .spawn(move || worker_main(w, idx))
                .is_ok()
            {
                spawned += 1;
            }
        }
        // If spawn count differs from global (rare OOM), rebuild with correct
        // size so run_in_pool never waits for workers that don't exist.
        let pool = if spawned == global {
            proto
        } else {
            Arc::new(PoolInner {
                entry: Mutex::new(()),
                state: Mutex::new(PoolState {
                    generation: 0,
                    degree: 0,
                    job: None,
                    finished: 0,
                    shutdown: false,
                }),
                work_cond: Condvar::new(),
                done_cond: Condvar::new(),
                size: spawned,
            })
        };
        let _ = POOL.set(pool);
    });
}

/// Set the global worker budget at runtime (0 → `available_parallelism`).
pub fn set_max_total_workers(n: usize) {
    init_from_env();
    let n = if n == 0 { cores() } else { n }.max(1);
    let prev = GLOBAL_MAX.swap(n, Ordering::Relaxed);
    let inflight = prev.saturating_sub(AVAILABLE.load(Ordering::Relaxed));
    AVAILABLE.store(n.saturating_sub(inflight), Ordering::Relaxed);
}

/// An admission lease for `degree` parallel-scan workers, taken from the global
/// budget and **released on drop**.
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

/// Admission control: gate on config plus the page/candidate threshold, then
/// reserve workers from the **global** budget. Returns a lease with `>= 2`
/// workers (released on drop), or `None` to run serial.
pub fn acquire(n_units: usize) -> Option<WorkerLease> {
    let want = degree_for(n_units)?;
    let granted = take_from_pool(want);
    if granted >= 2 {
        PARALLEL_SCANS.fetch_add(1, Ordering::Relaxed);
        WORKERS_GRANTED.fetch_add(granted as u64, Ordering::Relaxed);
        Some(WorkerLease { granted })
    } else {
        if granted > 0 {
            AVAILABLE.fetch_add(granted, Ordering::Relaxed);
        }
        SERIAL_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        None
    }
}

pub fn set_enabled(on: bool) {
    init_from_env();
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn set_min_pages(n: usize) {
    init_from_env();
    MIN_PAGES.store(n, Ordering::Relaxed);
}

pub fn set_max_workers(n: usize) {
    init_from_env();
    MAX_WORKERS.store(n, Ordering::Relaxed);
}

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
    let cursor = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let err: Arc<Mutex<Option<DbError>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = crate::query_limits::snapshot_deadline();
    let pages: Arc<[PageId]> = Arc::from(pages);
    let reader = reader.clone();
    let snapshot = snapshot.clone();

    run_in_pool(degree, {
        // Clone Arc handles for the closure; originals stay alive for gather.
        let cursor = Arc::clone(&cursor);
        let total = Arc::clone(&total);
        let err = Arc::clone(&err);
        let stop = Arc::clone(&stop);
        let pages = Arc::clone(&pages);
        move || {
            while !stop.load(Ordering::Relaxed) {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= pages.len() {
                    break;
                }
                if i.is_multiple_of(4) {
                    if let Err(e) = deadline.check() {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                match count_page_visible(&reader, pages[i], &snapshot, self_xid) {
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
        }
    });

    // run_in_pool returned: all workers finished. Gather results from originals.
    let err_val = err.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(e) = err_val {
        return Err(e);
    }
    Ok(total.load(Ordering::Relaxed))
}

/// Parallel index-candidate resolution: partition a B-tree/index candidate
/// `RowId` list across `degree` workers, each resolving its slice
/// (`get_visible` → `per_candidate` = the B2 deform + predicate re-check +
/// project) and keeping the survivors; gather = **concat** (order-agnostic —
/// the caller has no `ORDER BY`). This is the filtered-`SELECT` hot path.
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
    debug_assert!(
        candidates.len() >= PARALLEL_CANDIDATE_MIN,
        "parallel_resolve_candidates called with too-small candidate list ({} < {})",
        candidates.len(),
        PARALLEL_CANDIDATE_MIN
    );

    let cursor = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let err: Arc<Mutex<Option<DbError>>> = Arc::new(Mutex::new(None));
    let parts: Arc<Mutex<Vec<Vec<Vec<Literal>>>>> = Arc::new(Mutex::new(Vec::new()));
    let deadline = crate::query_limits::snapshot_deadline();
    let candidates: Arc<[RowId]> = Arc::from(candidates);
    let reader = reader.clone();
    let snapshot = snapshot.clone();

    run_in_pool(degree, {
        let cursor = Arc::clone(&cursor);
        let stop = Arc::clone(&stop);
        let err = Arc::clone(&err);
        let parts = Arc::clone(&parts);
        let candidates = Arc::clone(&candidates);
        move || {
            let mut rows: Vec<Vec<Literal>> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= candidates.len() {
                    break;
                }
                if i.is_multiple_of(64) {
                    if let Err(e) = deadline.check() {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                let rid = candidates[i];
                let bytes = match get_visible(&reader, rid, &snapshot, self_xid) {
                    Ok(Some(b)) => b,
                    Ok(None) => continue,
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
        }
    });

    let err_val = err.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(e) = err_val {
        return Err(e);
    }
    let mut all_rows = Vec::new();
    for rows in std::mem::take(&mut *parts.lock().unwrap_or_else(|p| p.into_inner())) {
        all_rows.extend(rows);
    }
    Ok(all_rows)
}

/// Parallel filtered `COUNT(*)` — **partial aggregate**: each worker scans its
/// page slice, evaluates `matches` on every visible tuple, and counts the
/// survivors; the gather **sums** the partials.
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
    let cursor = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let err: Arc<Mutex<Option<DbError>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = crate::query_limits::snapshot_deadline();
    let pages: Arc<[PageId]> = Arc::from(pages);
    let reader = reader.clone();
    let snapshot = snapshot.clone();

    run_in_pool(degree, {
        let cursor = Arc::clone(&cursor);
        let total = Arc::clone(&total);
        let err = Arc::clone(&err);
        let stop = Arc::clone(&stop);
        let pages = Arc::clone(&pages);
        move || {
            let mut local = 0usize;
            let mut page_buf: Vec<(RowId, Vec<u8>)> = Vec::new();
            'outer: while !stop.load(Ordering::Relaxed) {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= pages.len() {
                    break;
                }
                if i.is_multiple_of(4) {
                    if let Err(e) = deadline.check() {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                page_buf.clear();
                if let Err(e) =
                    scan_page_into(&reader, pages[i], &snapshot, self_xid, &mut page_buf)
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
        }
    });

    let err_val = err.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(e) = err_val {
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
/// it, or `Err` to abort the whole scan. It must be `Sync`.
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
    let cursor = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let err: Arc<Mutex<Option<DbError>>> = Arc::new(Mutex::new(None));
    #[allow(clippy::type_complexity)]
    let parts: Arc<Mutex<Vec<(Vec<Vec<Literal>>, Vec<RowId>)>>> = Arc::new(Mutex::new(Vec::new()));
    let deadline = crate::query_limits::snapshot_deadline();
    let pages: Arc<[PageId]> = Arc::from(pages);
    let reader = reader.clone();
    let snapshot = snapshot.clone();

    run_in_pool(degree, {
        let cursor = Arc::clone(&cursor);
        let stop = Arc::clone(&stop);
        let err = Arc::clone(&err);
        let parts = Arc::clone(&parts);
        let pages = Arc::clone(&pages);
        move || {
            let mut rows: Vec<Vec<Literal>> = Vec::new();
            let mut ids: Vec<RowId> = Vec::new();
            let mut page_buf: Vec<(RowId, Vec<u8>)> = Vec::new();
            'outer: while !stop.load(Ordering::Relaxed) {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= pages.len() {
                    break;
                }
                if i.is_multiple_of(4) {
                    if let Err(e) = deadline.check() {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                }
                page_buf.clear();
                if let Err(e) =
                    scan_page_into(&reader, pages[i], &snapshot, self_xid, &mut page_buf)
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
        }
    });

    let err_val = err.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(e) = err_val {
        return Err(e);
    }
    let mut all_rows = Vec::new();
    let mut all_ids = Vec::new();
    for (rows, ids) in std::mem::take(&mut *parts.lock().unwrap_or_else(|p| p.into_inner())) {
        all_rows.extend(rows);
        all_ids.extend(ids);
    }
    Ok((all_rows, all_ids))
}
