//! unidb: a single embedded storage/transaction engine that unifies
//! relational CRUD, vector search (HNSW), graph edges, and a WAL-derived
//! event queue over **one** page store, one WAL, one buffer pool, and one
//! transaction manager. A single transaction can touch all four data
//! models atomically, because there is only one node and one log — see
//! `CLAUDE.md` for the full design rationale and locked decisions.
//!
//! [`Engine`] is the sole entry point. Every operation takes an explicit
//! `Xid` obtained from [`Engine::begin`] (or [`Engine::begin_with_isolation`])
//! and must be finished with [`Engine::commit`] or [`Engine::abort`] — there
//! is no implicit transaction anywhere in this crate. The public API groups
//! into:
//! - **Lifecycle**: [`Engine::open`], [`Engine::checkpoint`], [`Engine::flush`].
//! - **Transactions**: [`Engine::begin`], [`Engine::begin_with_isolation`],
//!   [`Engine::commit`], [`Engine::abort`].
//! - **Raw CRUD**: [`Engine::insert`], [`Engine::get`], [`Engine::update`],
//!   [`Engine::delete`] — untyped byte-slice rows, the lowest-level API.
//! - **SQL**: [`Engine::execute_sql`] (a practical subset — see `CLAUDE.md`
//!   §1's non-goals; not full ANSI SQL). Accepts a full `;`-separated
//!   multi-statement string executed atomically under one `xid`.
//! - **Graph**: [`Engine::execute_cypher`] (a Cypher subset), plus the
//!   lower-level [`Engine::create_edge`]/[`Engine::delete_edge`]/
//!   [`Engine::edges_from`].
//! - **Event queue**: [`Engine::enable_events`], [`Engine::poll_events`]/
//!   [`Engine::ack_events`] (Kafka-style manual-commit consumer offsets),
//!   [`Engine::vacuum_events`].
//! - **Secondary indexing**: [`Engine::set_column_index`],
//!   [`Engine::index_status`].
//! - **Row-level security**: [`Engine::set_rls_policy`] (Rust-API only, no
//!   SQL `CREATE POLICY` surface — see the module doc on `catalog.rs`).
//!
//! An optional REST/JWT/SSE/metrics server built on top of this crate lives
//! behind the `server` Cargo feature (`src/server/`, `src/bin/
//! unidb-server.rs`) — the engine itself never depends on an async runtime;
//! see `CLAUDE.md`'s "tokio (M5 server only — the engine stays sync)" note.

// unsafe_code is denied crate-wide; mmap.rs is the sole exception (CLAUDE.md §4).
#![deny(unsafe_code)]

pub mod audit;
pub mod authz;
/// Autovacuum (A3) — the background launcher thread that auto-triggers the
/// existing M10 `Engine::vacuum`. See the module doc for why concurrent
/// background vacuum needs no new locking.
pub mod autovacuum;
pub mod backup;
pub mod btree_index;
pub mod bufferpool;
pub mod catalog;
pub mod checkpoint;
pub mod concurrency_hooks;
pub mod control;
pub mod csr_index;
/// P3.c — durable on-disk IVF-Flat vector index (production). Wired into
/// `CREATE INDEX ... USING HNSW|IVF` and `NEAR`. See the module doc and
/// `docs/design/p3c_vector_spike.md`.
pub mod disk_vector;
pub mod error;
pub mod format;
pub mod fulltext;
pub mod graph;
pub mod heap;
/// P3.d — chunked, streamed, out-of-line large-object storage.
pub mod large_object;
pub mod lockmgr;
pub mod mmap;
pub mod mvcc;
pub mod page;
pub mod query_limits;
pub mod queue;
pub mod read_handle;
pub mod recovery;
pub mod replication;
#[cfg(feature = "server")]
pub mod server;
pub mod sql;
pub mod txn;
pub mod vector;
pub mod wal;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    bufferpool::BufferPool,
    catalog::{Catalog, CatalogCtx, IndexKind, IndexStatus, TableDef},
    control::ControlData,
    disk_vector::DiskIvfIndex,
    error::Result,
    format::{Lsn, PageId, Xid, DEFAULT_PAGE_SIZE},
    graph::{
        edges::{self, Edge},
        executor as graph_executor,
        index::resolve_candidates_batched,
        parser::parse_cypher,
    },
    heap::Heap,
    large_object::LobStore,
    lockmgr::LockManager,
    queue::{CONSUMERS_TABLE, EVENTS_TABLE},
    sql::{
        executor::{self, ExecCtx, ExecResult},
        logical::{apply_rls, bind_params, Expr, Literal, LogicalPlan},
        parser::parse_sql,
        query::{FromNode, QuerySpec},
    },
    txn::{IsolationLevel, TransactionManager, UndoAction},
    wal::Wal,
};

pub use crate::error::DbError;
pub use crate::heap::RowId;
pub use crate::read_handle::ReadHandle;
pub use crate::sql::executor::ExecResult as SqlResult;
pub use crate::txn::IsolationLevel as Isolation;

/// Default buffer-pool capacity in frames (P1.c). Raised from 256 (2 MiB at
/// the 8 KiB default page size) to 4096 (32 MiB) — far fewer evictions at
/// 100k+ rows per table. Override with the `UNIDB_BUFFER_POOL_PAGES` env var
/// or [`Engine::open_with_pool_capacity`].
const DEFAULT_POOL_CAPACITY: usize = 4096;

/// The configured buffer-pool capacity: `UNIDB_BUFFER_POOL_PAGES` if it parses
/// to a sane value (>= 16 frames), else [`DEFAULT_POOL_CAPACITY`].
fn configured_pool_capacity() -> usize {
    std::env::var("UNIDB_BUFFER_POOL_PAGES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 16)
        .unwrap_or(DEFAULT_POOL_CAPACITY)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

/// Auto-checkpoint policy (P1.e). A checkpoint bounds WAL growth (and the P1.a
/// full-page-image volume); before P1.e it was manual-only, so the WAL grew
/// unbounded. The engine runs the existing checkpoint path inline on the writer
/// thread when **either** trigger fires and the engine is quiescent (no open
/// transaction) — running it mid-transaction would let the checkpoint's WAL
/// truncation discard an in-flight transaction's undo records.
#[derive(Debug, Clone, Copy)]
pub struct AutoCheckpointConfig {
    /// Master switch. Defaults on (env `UNIDB_AUTO_CHECKPOINT=0` disables).
    pub enabled: bool,
    /// Checkpoint at least this often once quiescent (env
    /// `UNIDB_CHECKPOINT_TIMEOUT_SECS`, default 60 s).
    pub timeout: Duration,
    /// Checkpoint once the WAL since the last checkpoint reaches this many bytes
    /// (env `UNIDB_MAX_WAL_SIZE_BYTES`, default 64 MiB).
    pub max_wal_size: u64,
}

impl Default for AutoCheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("UNIDB_AUTO_CHECKPOINT").as_deref() != Ok("0"),
            timeout: Duration::from_secs(env_u64("UNIDB_CHECKPOINT_TIMEOUT_SECS", 60)),
            max_wal_size: env_u64("UNIDB_MAX_WAL_SIZE_BYTES", 64 * 1024 * 1024),
        }
    }
}

/// Autovacuum policy (A2), mirroring [`AutoCheckpointConfig`]'s shape and its
/// auto-trigger precedent (P1.e). A background thread (A3) wakes every
/// `naptime`, and when the Postgres-style trigger fires
///
/// ```text
/// dead_tuple_estimate > threshold + scale_factor * live_tuple_estimate
/// ```
///
/// it calls the existing, already-safe [`Engine::vacuum`] (M10) — autovacuum
/// only auto-*triggers* reclamation; it does not re-implement it, nor touch the
/// vacuum horizon (which stays correct under concurrency, P5.c, and pinned by
/// replication slots, P6.b). Default on with conservative Postgres-like
/// thresholds so an idle or light workload never triggers it.
#[derive(Debug, Clone, Copy)]
pub struct AutoVacuumConfig {
    /// Master switch. Defaults on (env `UNIDB_AUTOVACUUM_ENABLED=0` disables).
    pub enabled: bool,
    /// Minimum dead-tuple count before a vacuum is even considered (env
    /// `UNIDB_AUTOVACUUM_THRESHOLD`, default 50 — Postgres's default).
    pub threshold: u64,
    /// Fraction of the live-tuple estimate added to `threshold` so larger tables
    /// tolerate more churn before vacuuming (env `UNIDB_AUTOVACUUM_SCALE_FACTOR`,
    /// default 0.2 — Postgres's default).
    pub scale_factor: f64,
    /// How long the background launcher sleeps between policy checks (env
    /// `UNIDB_AUTOVACUUM_NAPTIME_SECS`, default 60 s — Postgres's default).
    pub naptime: Duration,
}

impl Default for AutoVacuumConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("UNIDB_AUTOVACUUM_ENABLED").as_deref() != Ok("0"),
            threshold: env_u64("UNIDB_AUTOVACUUM_THRESHOLD", 50),
            scale_factor: env_f64("UNIDB_AUTOVACUUM_SCALE_FACTOR", 0.2),
            naptime: Duration::from_secs(env_u64("UNIDB_AUTOVACUUM_NAPTIME_SECS", 60).max(1)),
        }
    }
}

impl AutoVacuumConfig {
    /// The Postgres-style trigger: `dead > threshold + scale_factor * live`.
    /// Pure function of the config + the two estimates, so it is trivially
    /// testable and the launcher (A3) and any caller evaluate it identically.
    pub fn should_vacuum(&self, dead: u64, live: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let trigger = self.threshold as f64 + self.scale_factor * live as f64;
        dead as f64 > trigger
    }
}

/// A parsed-but-not-yet-bound statement (P2.e), produced by
/// [`Engine::prepare`] and run with [`Engine::execute_prepared`]. Holds the
/// logical plans so a query is parsed once and executed many times with
/// different bind parameters.
#[derive(Debug, Clone)]
pub struct Prepared {
    plans: Vec<LogicalPlan>,
}

/// The outcome of an [`Engine::vacuum`] pass (M10). Surfaces the numbers the
/// milestone cares about — including whether a long-lived transaction/reader
/// held the horizon back and blocked reclamation, so that footgun is visible
/// rather than silently swallowed (same as Postgres surfacing `oldest_xmin`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VacuumReport {
    /// The visibility horizon used (`OldestXmin`) — see
    /// [`crate::txn::TransactionManager::vacuum_horizon`].
    pub horizon: Xid,
    /// LIVE tuple slots examined across every table's heap.
    pub rows_scanned: usize,
    /// Dead versions physically removed (line pointers marked DEAD then freed).
    pub versions_reclaimed: usize,
    /// Slots promoted DEAD→UNUSED (now reusable). Equals `versions_reclaimed`
    /// in v1 (whole-page compaction promotes every reclaimed slot).
    pub slots_freed: usize,
    /// Tuple-body bytes reclaimed by intra-page compaction.
    pub bytes_reclaimed: usize,
    /// `true` if the horizon was held below `next_xid` by a live transaction
    /// or concurrent reader — reclamation may have been more conservative than
    /// a quiescent database would allow.
    pub horizon_blocked: bool,
}

pub struct Engine {
    /// Recovery/control metadata. Interior-mutable behind a `Mutex` (P5.e) so
    /// the shared `&self` engine can rewrite `catalog_root`/checkpoint state
    /// from any worker thread. **Never hold this lock across an fsync** (WAL or
    /// data-file) — lock, read/mutate the small struct, unlock. `page_size` is
    /// cached out-of-band below because it never changes after open and is read
    /// on nearly every method.
    control: Mutex<ControlData>,
    /// Immutable page size (bytes), cached from the control file at open so the
    /// hot paths don't lock `control` just to read it.
    page_size: usize,
    pool: BufferPool,
    wal: Wal,
    heap: Heap,
    txn_mgr: TransactionManager,
    lock_mgr: LockManager,
    // Behind `Arc<RwLock>` (6b) so the concurrent read path (`ReadHandle`)
    // can see the live catalog — including `TableDef.pages`, which grows on
    // INSERT. The writer takes the write-lock only briefly (per statement,
    // never across an fsync); readers take the read-lock.
    catalog: Arc<RwLock<Catalog>>,
    control_path: PathBuf,
    _wal_path: PathBuf,
    /// Meta page id of the durable edge-adjacency index (P3.b) — a `DiskBTree`
    /// over `__edges__.from_id`. Cached here so `create_edge`/`delete_edge`/
    /// `edges_from`/Cypher reconstruct the tree without a catalog lookup on
    /// every call. Crash-recovered, never rebuilt on open.
    edge_index_meta: PageId,
    /// Meta page id of the `__lobs__` large-object chunk index (P3.d) — a durable
    /// `DiskBTree` on `lob_id`, cached like `edge_index_meta`.
    lob_index_meta: PageId,
    /// Next large-object id to hand out (P3.d), derived at open from the highest
    /// committed `lob_id` in `__lobs__` (mirrors `next_event_seq`).
    /// Atomic (P5.e) so `put_large_object` can hand out ids from `&self`.
    next_lob_id: AtomicI64,
    next_event_seq: AtomicU64,
    /// Auto-checkpoint policy + state (P1.e). Behind `Mutex` (P5.e) for `&self`.
    auto_checkpoint: Mutex<AutoCheckpointConfig>,
    last_checkpoint: Mutex<Instant>,
    checkpoints_triggered: AtomicU64,
    /// Approximate count of **dead tuple versions** created since the last
    /// vacuum (A1). Incremented once per `xmax` stamp — every UPDATE (the old
    /// version dies) and every DELETE — and reset to 0 by `vacuum_inner`. This
    /// is the autovacuum trigger's numerator (A2). Approximate by design, like
    /// Postgres's `n_dead_tup`: it counts at the raw-CRUD and SQL-statement
    /// chokepoints (not the `heap.rs` mutation itself, which recovery redo also
    /// drives — recovery must not count), so an aborted UPDATE/DELETE over-counts
    /// until the next vacuum refreshes the estimate.
    dead_tuples: AtomicU64,
    /// Approximate count of **live tuples** (Postgres `reltuples`) — the
    /// trigger's `live` term (A2). Incremented on INSERT, decremented on DELETE
    /// (UPDATE leaves it unchanged: one visible version replaces another), and
    /// re-set exactly to the scanned live-slot count at the end of every vacuum
    /// (`vacuum_inner`), so vacuum corrects any accumulated drift — again
    /// mirroring how Postgres refreshes its estimate on (auto)vacuum/analyze.
    live_tuples: AtomicU64,
    /// Autovacuum policy (A2). Behind `Mutex` (like `auto_checkpoint`) for
    /// `&self` mutation. The background launcher (A3) reads this each naptime.
    autovacuum: Mutex<AutoVacuumConfig>,
    /// How many autovacuum passes the background launcher has run this session
    /// (A4 observability). Distinct from manual `Engine::vacuum` calls.
    autovacuums_triggered: AtomicU64,
    /// Wall-clock (seconds since the Unix epoch) of the last autovacuum pass,
    /// 0 if none yet (A4 observability). Coarse timestamp for `/metrics`.
    last_autovacuum_epoch_secs: AtomicU64,
    /// The background autovacuum launcher (A3), present once `spawn_autovacuum`
    /// has been called on an `Arc<Engine>`. `None` for a bare `Engine::open`
    /// handle (which cannot host a `Weak`-holding worker) or when the policy is
    /// disabled. Dropping the engine drops this, whose `Drop` stops the thread —
    /// the clean-shutdown hook.
    autovacuum_handle: Mutex<Option<crate::autovacuum::AutoVacuumHandle>>,
    /// Serializes the non-CRUD write paths that do a *non-atomic*
    /// read-catalog-then-mutate-a-shared-secondary-index sequence — graph edges
    /// (the `__edges__.from_id` `DiskBTree` + page list), large objects (the
    /// `__lobs__` tree), the event queue's system tables, and catalog DDL
    /// (P5.e-3). Two of these running at once could lose a page-list update or
    /// corrupt a shared index tree, which the per-page heap latches alone don't
    /// prevent (they guard one page, not a multi-page tree or a catalog RMW).
    ///
    /// The hot paths do **not** take this lock: raw CRUD (`insert`/`get`/
    /// `update`/`delete`) touches only the latched heap + row locks and scales
    /// across cores, and SQL already serializes writers on the catalog
    /// `RwLock`. So this coarse lock only serializes the secondary,
    /// low-frequency write paths — correctness first; finer-grained index
    /// concurrency (latch-coupled B-tree writes) is future work.
    write_serial: Mutex<()>,
    /// Replication slots (P6.b). Each slot pins a `restart_lsn` the WAL must be
    /// retained from; the checkpoint truncation floor is
    /// `min(checkpoint_lsn, min slot restart_lsn)` so a consumer's segments are
    /// never deleted before it has streamed them. Persisted in `slots.json`.
    replication: Arc<crate::replication::SlotRegistry>,
    /// Users / roles / privileges (P6.e). The embedded API runs as an implicit
    /// superuser (identity `None`); named users go through `execute_sql_as` with
    /// per-table privilege checks. Persisted in `roles.json`.
    authz: Arc<crate::authz::RoleStore>,
    /// Security audit trail (P6.f) — auth DDL + named-user access decisions,
    /// appended to `audit.log`.
    audit: Arc<crate::audit::AuditLog>,
    /// Observability counters (P6.g): lifetime commits / aborts this session.
    commits: AtomicU64,
    aborts: AtomicU64,
    /// Slow-query log (P6.g): SQL statements whose wall-clock exceeded the
    /// threshold, kept as a bounded ring (most recent last). Threshold in
    /// **micros**; 0 disables (default), settable via `set_slow_query_threshold`.
    slow_query_threshold_us: AtomicU64,
    slow_queries: Mutex<std::collections::VecDeque<SlowQuery>>,
}

/// One slow-query-log entry (P6.g).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SlowQuery {
    pub sql: String,
    pub micros: u64,
}

/// A point-in-time snapshot of engine activity + counters (P6.g) — the
/// `pg_stat_*`-style view surfaced by `Engine::stats` and `GET /stats`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStats {
    pub commits: u64,
    pub aborts: u64,
    pub checkpoints: u64,
    /// Live transactions (writers) — the active-session count.
    pub active_transactions: usize,
    /// WAL bytes since the last checkpoint (auto-checkpoint pressure).
    pub wal_bytes: u64,
    /// Registered replication slots + the largest lag (tail LSN − min slot).
    pub replication_slots: usize,
    pub max_replication_lag: u64,
    /// Pages currently in the data file.
    pub data_pages: u32,
    /// The most recent slow queries (bounded).
    pub recent_slow_queries: Vec<SlowQuery>,
    /// Autovacuum passes run by the background launcher this session (A4).
    pub autovacuums: u64,
    /// Estimated dead tuple versions since the last vacuum (A1/A4) — the
    /// autovacuum trigger's numerator.
    pub dead_tuple_estimate: u64,
    /// Estimated live tuples (A1/A4) — the trigger's `live` term.
    pub live_tuple_estimate: u64,
    /// Unix-epoch seconds of the last autovacuum pass, 0 if none yet (A4).
    pub last_autovacuum_epoch_secs: u64,
}

/// `Engine` must be safely **shareable** across threads (P5.e: a pool of N
/// worker threads each hold `Arc<Engine>` and issue concurrent writes — see
/// `src/server/engine_handle.rs`). This reverses the M5 "single writer thread,
/// `Engine` is `!Sync`" simplification (human sign-off recorded in
/// `PROGRESS.md`). It is a compiler-enforced fact, not an assumption: every
/// mutated field is now interior-mutable behind a `Mutex`/`RwLock`/atomic, and
/// every storage component (`BufferPool`/`Wal`/`Heap`/`TransactionManager`/
/// `LockManager`) exposes a `&self` API (P5.a–P5.e-1), so `Send + Sync` hold.
/// This line turns "believed true" into "verified at every compile," so a
/// future field addition that broke `Sync` would fail to build immediately.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Engine>();
};

/// Read/write-lock a shared catalog, recovering from poisoning rather than
/// panicking (a poisoned catalog means a prior panic-while-locked; proceeding
/// with the state as-is is safer than aborting the process). Free functions
/// (not `&self` methods) so they borrow only the `catalog` field, leaving the
/// engine's other fields free to borrow disjointly in the same scope.
fn cat_read(c: &RwLock<Catalog>) -> RwLockReadGuard<'_, Catalog> {
    c.read().unwrap_or_else(|e| e.into_inner())
}

fn cat_write(c: &RwLock<Catalog>) -> RwLockWriteGuard<'_, Catalog> {
    c.write().unwrap_or_else(|e| e.into_inner())
}

/// Map an auth-DDL statement to an `(action, object)` pair for the audit log
/// (P6.f).
fn auth_stmt_audit(stmt: &crate::authz::AuthStmt) -> (&'static str, String) {
    use crate::authz::AuthStmt as A;
    match stmt {
        A::CreateUser { name, .. } => ("create_user", name.clone()),
        A::DropUser(name) => ("drop_user", name.clone()),
        A::CreateRole(name) => ("create_role", name.clone()),
        A::DropRole(name) => ("drop_role", name.clone()),
        A::GrantPrivs { table, grantee, .. } => ("grant", format!("{table} to {grantee}")),
        A::RevokePrivs { table, grantee, .. } => ("revoke", format!("{table} from {grantee}")),
        A::GrantRole { role, grantee } => ("grant_role", format!("{role} to {grantee}")),
        A::RevokeRole { role, grantee } => ("revoke_role", format!("{role} from {grantee}")),
    }
}

/// A short audit action verb for a data/DDL plan (P6.f).
fn plan_audit_action(plan: &LogicalPlan) -> &'static str {
    match plan {
        LogicalPlan::Select { .. } | LogicalPlan::Query(_) | LogicalPlan::Explain { .. } => {
            "select"
        }
        LogicalPlan::Insert { .. } => "insert",
        LogicalPlan::Update { .. } => "update",
        LogicalPlan::Delete { .. } => "delete",
        _ => "ddl",
    }
}

/// The base tables a Phase-4 query reads (P6.e privilege check): every
/// `FROM` table across the query and its CTE bodies, excluding CTE names (which
/// are derived relations, not base tables). Subquery-only references are not
/// walked in v1 (a documented approximation — such a query from a non-superuser
/// simply isn't over-granted; it may need broader grants).
fn query_base_tables(spec: &QuerySpec) -> Vec<String> {
    fn walk(node: &FromNode, ctes: &std::collections::HashSet<String>, out: &mut Vec<String>) {
        match node {
            FromNode::Table(t) => {
                if !ctes.contains(&t.table) {
                    out.push(t.table.clone());
                }
            }
            FromNode::Join { left, right, .. } => {
                walk(left, ctes, out);
                walk(right, ctes, out);
            }
        }
    }
    let cte_names: std::collections::HashSet<String> =
        spec.with.iter().map(|(n, _)| n.clone()).collect();
    let mut out = Vec::new();
    walk(&spec.from, &cte_names, &mut out);
    for (_, cte) in &spec.with {
        out.extend(query_base_tables(cte));
    }
    out
}

/// Lock the control-metadata `Mutex`, recovering from poisoning rather than
/// panicking (same rationale as [`cat_read`]). **Keep the guard's scope
/// minimal — never hold it across an fsync** (see the `control` field doc).
fn ctrl_lock(c: &Mutex<ControlData>) -> std::sync::MutexGuard<'_, ControlData> {
    c.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poison-tolerant lock of the non-CRUD write serializer (P5.e-3, see the
/// `write_serial` field).
fn serial_lock(m: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poison-tolerant lock of the auto-checkpoint policy `Mutex` (P5.e).
fn ctrl_lock_ac(
    m: &Mutex<AutoCheckpointConfig>,
) -> std::sync::MutexGuard<'_, AutoCheckpointConfig> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poison-tolerant lock of the last-checkpoint-instant `Mutex` (P5.e).
fn ctrl_lock_lc(m: &Mutex<Instant>) -> std::sync::MutexGuard<'_, Instant> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poison-tolerant lock of the autovacuum policy `Mutex` (A2).
fn ctrl_lock_av(m: &Mutex<AutoVacuumConfig>) -> std::sync::MutexGuard<'_, AutoVacuumConfig> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    /// Open (or create) a database at `dir`. Pass `page_size = 0` to use the
    /// default. The buffer-pool capacity comes from `UNIDB_BUFFER_POOL_PAGES`
    /// or the [`DEFAULT_POOL_CAPACITY`] default (P1.c).
    pub fn open(dir: &Path, page_size: u32) -> Result<Self> {
        Self::open_with_pool_capacity(dir, page_size, configured_pool_capacity())
    }

    /// Open (or create) a database and return it wrapped in an `Arc` with the
    /// background **autovacuum launcher started** (A3) — the "default-on"
    /// deployment/embedded-primary entry point. Equivalent to
    /// `let e = Arc::new(Engine::open(..)?); e.spawn_autovacuum(); e`.
    ///
    /// `Engine::open` itself returns a bare `Engine` with no background thread
    /// (a `Weak`-holding worker needs an `Arc`); use this when you want
    /// autovacuum without managing the `Arc`/`spawn_autovacuum` dance yourself.
    /// Honors the A2 policy: no thread is spawned if autovacuum is disabled.
    pub fn open_arc(dir: &Path, page_size: u32) -> Result<Arc<Self>> {
        let engine = Arc::new(Self::open(dir, page_size)?);
        engine.spawn_autovacuum();
        Ok(engine)
    }

    /// Open (or create) a database with an explicit buffer-pool capacity in
    /// frames (P1.c) — for tests/benchmarks that need a specific pool size
    /// without going through the `UNIDB_BUFFER_POOL_PAGES` env var.
    pub fn open_with_pool_capacity(
        dir: &Path,
        page_size: u32,
        pool_capacity: usize,
    ) -> Result<Self> {
        let pool_capacity = pool_capacity.max(16);
        std::fs::create_dir_all(dir)?;
        let ctrl_p = dir.join("control");
        let data_p = dir.join("data.db");
        let wal_p = dir.join("db.wal");

        let ps = if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size
        };
        // `control` is interior-mutable from the start (P5.e) so the same
        // `&Mutex<ControlData>` can be handed to the `CatalogCtx`/`ensure_*`
        // helpers below and then moved directly into the `Engine`.
        let control = Mutex::new(control::open_or_create(&ctrl_p, ps)?);
        let page_size_usize = ctrl_lock(&control).page_size as usize;

        // Run recovery before opening normal operation.
        if wal_p.exists() && ctrl_p.exists() {
            recovery::recover(&ctrl_p, &data_p, &wal_p, page_size_usize, pool_capacity)?;
        }

        let pool = BufferPool::open(&data_p, page_size_usize, pool_capacity)?;
        let wal_tail = ctrl_lock(&control).wal_tail_lsn;
        let wal = Wal::open(&wal_p, wal_tail)?;
        let heap = Heap::new(page_size_usize);

        // Resume the xid counter past the highest xid that ever began —
        // reusing an xid would corrupt MVCC visibility for existing tuples
        // (see MEMORY.md's design note). The WAL scan alone is not
        // sufficient: a checkpoint truncates every WAL_TXN_BEGIN record
        // before its LSN (ordinarily *all* of them, since a checkpoint only
        // runs after everything has committed), so after any checkpoint the
        // WAL has nothing left to scan. `control.next_xid` (persisted at
        // every checkpoint, M5 fix — see format.rs's v2->v3 note) is the
        // other half of this max: whichever source has seen a higher xid
        // wins, so this is correct whether or not a checkpoint ever ran.
        let existing_records = if wal_p.exists() {
            Wal::scan_file(&wal_p)?
        } else {
            Vec::new()
        };
        let next_xid = TransactionManager::recover_next_xid(&existing_records)
            .max(ctrl_lock(&control).next_xid);
        let txn_mgr = TransactionManager::with_next_xid(next_xid);
        let lock_mgr = LockManager::new();

        let mut catalog = Catalog::load(&ctrl_lock(&control), &pool)?;

        // `__edges__` always exists after open — before any user transaction
        // begins, so unlike ordinary `CREATE TABLE` there's no "ran inside a
        // transaction that later aborted" gap here (see MEMORY.md's M3.a
        // design note).
        {
            let mut cctx = CatalogCtx {
                pool: &pool,
                wal: &wal,
                control_path: &ctrl_p,
                control: &control,
                page_size: page_size_usize,
            };
            edges::ensure_edges_table(&mut catalog, &mut cctx)?;
            queue::ensure_queue_tables(&mut catalog, &mut cctx)?;
        }
        let edge_index_meta = ensure_edge_index(
            &mut catalog,
            &txn_mgr,
            &pool,
            &wal,
            &lock_mgr,
            &ctrl_p,
            &control,
            page_size_usize,
        )?;
        let lob_index_meta = large_object::ensure_lobs_table(
            &mut catalog,
            &txn_mgr,
            &pool,
            &wal,
            &lock_mgr,
            &ctrl_p,
            &control,
            page_size_usize,
        )?;
        let next_lob_id =
            derive_next_lob_id(&catalog, &txn_mgr, &pool, &wal, &lock_mgr, page_size_usize)?;
        let next_event_seq =
            derive_next_event_seq(&catalog, &txn_mgr, &pool, &wal, &lock_mgr, page_size_usize)?;
        // Replication slots (P6.b): persisted retention positions loaded from
        // `slots.json` — they hold the WAL truncation floor back at checkpoint.
        let replication = Arc::new(crate::replication::SlotRegistry::open(dir)?);
        // Users / roles / privileges (P6.e), loaded from `roles.json`.
        let authz = Arc::new(crate::authz::RoleStore::open(dir)?);
        // Security audit trail (P6.f).
        let audit = Arc::new(crate::audit::AuditLog::open(dir)?);

        // Phase 3: every secondary index is durable and crash-recovered — the
        // B-Tree/full-text/edge indexes as `DiskBTree`s (P3.a/P3.b), the vector
        // index as an on-disk IVF-Flat (P3.c). `Engine::open` does ZERO index
        // rebuilding: it reads each index straight from its stable meta page.
        // This is the O(1)-open moat; the async rebuild worker is retired.
        //
        // Commit-time fsync (C1): make **group-committed force-log-at-commit**
        // the default. Statement mini-txns issued inside an open user
        // transaction append their WAL records without a per-statement fsync;
        // `Engine::commit`'s `sync_up_to(commit_lsn)` is the single durable
        // point (one fsync per transaction — group-coalesced across concurrent
        // committers). This is ARIES' force-log-at-commit, fulfilling D1; D2
        // (mini-txn bracketing) and D5 (WAL-before-page) are unchanged — D5 now
        // holds under deferral via the buffer pool's eviction-forced sync (C2).
        // The open-time system-table setup above ran while the WAL was still in
        // per-statement mode, so those meta pages are already durable; the flip
        // affects only post-open user activity. Standalone operations that claim
        // durability without a following commit (checkpoint, vacuum) issue their
        // own sync — see the C1 durability-claim audit in PROGRESS.md. The
        // per-statement policy survives only as an internal flag
        // (`set_deferred_sync(false)`) so the crash harness can exercise both.
        wal.set_deferred_sync(true);
        tracing::info!(dir = %dir.display(), page_size = page_size_usize, next_xid, "engine opened");
        Ok(Self {
            control,
            page_size: page_size_usize,
            pool,
            wal,
            heap,
            txn_mgr,
            lock_mgr,
            catalog: Arc::new(RwLock::new(catalog)),
            control_path: ctrl_p,
            _wal_path: wal_p,
            edge_index_meta,
            lob_index_meta,
            next_lob_id: AtomicI64::new(next_lob_id),
            next_event_seq: AtomicU64::new(next_event_seq),
            auto_checkpoint: Mutex::new(AutoCheckpointConfig::default()),
            last_checkpoint: Mutex::new(Instant::now()),
            checkpoints_triggered: AtomicU64::new(0),
            dead_tuples: AtomicU64::new(0),
            live_tuples: AtomicU64::new(0),
            autovacuum: Mutex::new(AutoVacuumConfig::default()),
            autovacuums_triggered: AtomicU64::new(0),
            last_autovacuum_epoch_secs: AtomicU64::new(0),
            autovacuum_handle: Mutex::new(None),
            write_serial: Mutex::new(()),
            replication,
            authz,
            audit,
            commits: AtomicU64::new(0),
            aborts: AtomicU64::new(0),
            slow_query_threshold_us: AtomicU64::new(0),
            slow_queries: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// Like [`Engine::execute_sql`], but under per-query resource limits (P5.f):
    /// a wall-clock **timeout**, a cooperative **cancellation** token, and/or a
    /// **`work_mem`** row budget the `ORDER BY`/hash-join spill operators respect.
    /// The limits are installed on the current thread for the duration of the
    /// call (a query runs on one worker thread, P5.e-3) and cleared on return, so
    /// a long scan/sort/join aborts with [`DbError::QueryTimeout`] /
    /// [`DbError::QueryCancelled`] at its next check point instead of running
    /// unbounded. `QueryLimits::default()` imposes no limit.
    pub fn execute_sql_with_limits(
        &self,
        xid: Xid,
        sql: &str,
        limits: crate::query_limits::QueryLimits,
    ) -> Result<Vec<ExecResult>> {
        let _guard = crate::query_limits::install(limits);
        self.execute_sql(xid, sql)
    }

    /// The authorization store (P6.e) — users/roles/privileges.
    pub fn authz(&self) -> &crate::authz::RoleStore {
        &self.authz
    }

    /// Execute SQL **as** a named user (P6.e), enforcing per-table privileges.
    /// `user == None` is the implicit **superuser** (the embedded API), so
    /// `execute_sql` is exactly `execute_sql_as(None, ..)` and is unrestricted.
    ///
    /// Also the entry point for auth DDL (`CREATE USER`/`ROLE`, `GRANT`,
    /// `REVOKE`) — those are intercepted here (they aren't `sqlparser` grammar),
    /// require superuser, and mutate the role store rather than the catalog.
    pub fn execute_sql_as(
        &self,
        user: Option<&str>,
        xid: Xid,
        sql: &str,
    ) -> Result<Vec<ExecResult>> {
        // Auth DDL (whole-statement) is handled here, not by the SQL executor.
        if let Some(stmt) = crate::authz::parse_auth_stmt(sql)? {
            let (action, object) = auth_stmt_audit(&stmt);
            match self
                .require_superuser(user)
                .and_then(|()| self.authz.apply(&stmt))
            {
                Ok(()) => {
                    self.audit.record_admin(user, action, &object, true);
                    Ok(vec![ExecResult::Rows(Vec::new())])
                }
                Err(e) => {
                    self.audit.record_admin(user, action, &object, false);
                    Err(e)
                }
            }
        } else {
            // A named non-superuser must hold the matching privilege on every
            // table each statement touches (an effective superuser skips checks).
            if let Some(u) = user {
                if !self.is_effective_superuser(Some(u)) {
                    for plan in parse_sql(sql)? {
                        if let Err(e) = self.check_plan_privileges(u, &plan) {
                            self.audit
                                .record(Some(u), plan_audit_action(&plan), "", false);
                            return Err(e);
                        }
                        self.audit
                            .record(Some(u), plan_audit_action(&plan), "", true);
                    }
                }
            }
            self.execute_sql(xid, sql)
        }
    }

    /// Privilege pre-check for `sql` as `user`, without executing (P6.e). Used by
    /// the server's read/param fast paths, which don't route through
    /// [`Engine::execute_sql_as`]. A superuser / embedded (`None`) always passes.
    /// Auth DDL requires superuser here too.
    pub fn authorize_sql(&self, user: Option<&str>, sql: &str) -> Result<()> {
        if crate::authz::parse_auth_stmt(sql)?.is_some() {
            return self.require_superuser(user);
        }
        if self.is_effective_superuser(user) {
            return Ok(());
        }
        let u = user.expect("effective superuser covers None");
        for plan in parse_sql(sql)? {
            self.check_plan_privileges(u, &plan)?;
        }
        Ok(())
    }

    /// An **effective** superuser skips all privilege checks: the embedded API
    /// (`None`), a named `SUPERUSER`, or *any* identity while the role store has
    /// no registered users (open / bootstrap mode — see [`RoleStore::has_users`]).
    fn is_effective_superuser(&self, user: Option<&str>) -> bool {
        match user {
            None => true,
            Some(u) => self.authz.is_superuser(u) || !self.authz.has_users(),
        }
    }

    /// Superuser gate for auth/schema DDL (P6.e).
    fn require_superuser(&self, user: Option<&str>) -> Result<()> {
        if self.is_effective_superuser(user) {
            Ok(())
        } else {
            Err(DbError::PermissionDenied(format!(
                "user '{}' must be a superuser for this operation",
                user.unwrap_or("?")
            )))
        }
    }

    /// Enforce that non-superuser `user` may run `plan` (P6.e).
    fn check_plan_privileges(&self, user: &str, plan: &LogicalPlan) -> Result<()> {
        use crate::authz::Privilege as P;
        let reqs: Vec<(String, P)> = match plan {
            LogicalPlan::Select { table, .. } => vec![(table.clone(), P::Select)],
            LogicalPlan::Insert { table, .. } => vec![(table.clone(), P::Insert)],
            LogicalPlan::Update { table, .. } => vec![(table.clone(), P::Update)],
            LogicalPlan::Delete { table, .. } => vec![(table.clone(), P::Delete)],
            LogicalPlan::Query(spec) | LogicalPlan::Explain { spec, .. } => query_base_tables(spec)
                .into_iter()
                .map(|t| (t, P::Select))
                .collect(),
            // Schema DDL requires superuser in v1.
            LogicalPlan::CreateTable { .. }
            | LogicalPlan::CreateIndex { .. }
            | LogicalPlan::AlterTableAddColumn { .. }
            | LogicalPlan::AlterTableDropColumn { .. }
            | LogicalPlan::DropTable { .. }
            | LogicalPlan::Truncate { .. }
            | LogicalPlan::Analyze { .. } => {
                return Err(DbError::PermissionDenied(
                    "schema DDL requires a superuser".into(),
                ));
            }
        };
        for (table, priv_) in reqs {
            if !self.authz.has_privilege(user, &table, priv_) {
                return Err(DbError::PermissionDenied(format!(
                    "{priv_:?} on '{table}' for user '{user}'"
                )));
            }
        }
        Ok(())
    }

    /// Parse and execute one or more `;`-separated SQL statements under
    /// `xid`, applying each table's RLS policy (if any) as a planner
    /// rewrite before execution. Returns one result per statement. Wraps the
    /// executor with slow-query timing (P6.g).
    pub fn execute_sql(&self, xid: Xid, sql: &str) -> Result<Vec<ExecResult>> {
        let start = Instant::now();
        let result = self.execute_sql_inner(xid, sql);
        self.note_query_time(sql, start.elapsed());
        result
    }

    // ── Observability (P6.g) ───────────────────────────────────────────────────

    /// Set the slow-query threshold; a query slower than this is logged
    /// (`tracing::warn`) and added to the bounded slow-query ring surfaced by
    /// [`Engine::stats`]. Zero (the default) disables slow-query logging.
    pub fn set_slow_query_threshold(&self, threshold: Duration) {
        self.slow_query_threshold_us
            .store(threshold.as_micros() as u64, Ordering::Relaxed);
    }

    /// Record a statement's wall-clock, logging + retaining it if slow (P6.g).
    fn note_query_time(&self, sql: &str, elapsed: Duration) {
        let threshold = self.slow_query_threshold_us.load(Ordering::Relaxed);
        if threshold == 0 {
            return;
        }
        let micros = elapsed.as_micros() as u64;
        if micros < threshold {
            return;
        }
        tracing::warn!(micros, threshold_us = threshold, "slow query");
        let entry = SlowQuery {
            sql: sql.chars().take(500).collect(),
            micros,
        };
        let mut ring = self.slow_queries.lock().unwrap_or_else(|e| e.into_inner());
        ring.push_back(entry);
        while ring.len() > 32 {
            ring.pop_front();
        }
    }

    /// A `pg_stat_*`-style snapshot of engine activity + counters (P6.g).
    pub fn stats(&self) -> EngineStats {
        let tail = self.wal.current_lsn();
        let max_replication_lag = self
            .replication
            .min_restart_lsn()
            .map(|m| tail.saturating_sub(m))
            .unwrap_or(0);
        let recent_slow_queries = self
            .slow_queries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        EngineStats {
            commits: self.commits.load(Ordering::Relaxed),
            aborts: self.aborts.load(Ordering::Relaxed),
            checkpoints: self.checkpoints_triggered.load(Ordering::SeqCst),
            active_transactions: self.txn_mgr.active_count(),
            wal_bytes: self.wal.wal_bytes(),
            replication_slots: self.replication.list().len(),
            max_replication_lag,
            data_pages: self.pool.page_count(),
            recent_slow_queries,
            autovacuums: self.autovacuums_triggered.load(Ordering::Relaxed),
            dead_tuple_estimate: self.dead_tuples.load(Ordering::Relaxed),
            live_tuple_estimate: self.live_tuples.load(Ordering::Relaxed),
            last_autovacuum_epoch_secs: self.last_autovacuum_epoch_secs.load(Ordering::Relaxed),
        }
    }

    fn execute_sql_inner(&self, xid: Xid, sql: &str) -> Result<Vec<ExecResult>> {
        let page_size = self.page_size;
        let plans = parse_sql(sql)?;
        // Snapshot the catalog root so DDL (which the catalog persists
        // immediately, not on user-txn commit) from earlier statements of a
        // multi-statement request can be rolled back if a later one fails
        // (P2.c). Heap writes are undone by the caller's transaction abort; the
        // catalog, being non-MVCC, needs this explicit restore.
        let saved_catalog_root = ctrl_lock(&self.control).catalog_root;
        let mut results = Vec::with_capacity(plans.len());
        for plan in plans {
            let plan = apply_rls(plan, &cat_read(&self.catalog));
            let mut catalog = cat_write(&self.catalog);
            let mut ctx = ExecCtx {
                catalog: &mut catalog,
                txn_mgr: &self.txn_mgr,
                pool: &self.pool,
                wal: &self.wal,
                lock_mgr: &self.lock_mgr,
                control_path: &self.control_path,
                control: &self.control,
                page_size,
                xid,
                next_event_seq: &self.next_event_seq,
            };
            match executor::execute(plan, &mut ctx) {
                Ok(result) => {
                    self.note_dml_result(&result); // A1: dead/live-tuple accounting.
                    results.push(result);
                }
                Err(e) => {
                    drop(catalog);
                    self.restore_catalog_root(saved_catalog_root)?;
                    return Err(e);
                }
            }
        }
        Ok(results)
    }

    /// Roll the catalog back to a previously captured root page (P2.c). Used by
    /// `execute_sql` to undo DDL that earlier statements of a now-failed
    /// multi-statement request already persisted: the catalog is not
    /// user-transaction-scoped (a documented M1 limitation), so this manual
    /// restore is what makes a failed request leave the schema untouched. It
    /// rewrites the control file to the saved root and reloads the in-memory
    /// catalog from it. (Crash-safe, user-txn-scoped catalog redo/undo through
    /// recovery is a larger, Core-lane-coordinated follow-up — see PROGRESS.)
    fn restore_catalog_root(&self, root: crate::format::PageId) -> Result<()> {
        let reloaded = {
            let mut control = ctrl_lock(&self.control);
            if control.catalog_root == root {
                return Ok(());
            }
            control.catalog_root = root;
            control::write(&self.control_path, &control)?;
            Catalog::load(&control, &self.pool)?
        };
        *cat_write(&self.catalog) = reloaded;
        Ok(())
    }

    /// Parameterized SQL (P2.e): the same as [`Engine::execute_sql`], but `$n`
    /// placeholders are filled from `params` **as data, never re-parsed as
    /// SQL** — this is the injection-safe entry point. A value that would be
    /// malicious inside an interpolated string (e.g. `"'; DROP TABLE t; --"`)
    /// is bound as a plain `Literal::Text` and can only ever match/insert that
    /// literal string.
    pub fn execute_sql_params(
        &self,
        xid: Xid,
        sql: &str,
        params: &[Literal],
    ) -> Result<Vec<ExecResult>> {
        let plans = parse_sql(sql)?;
        self.run_bound_plans(xid, plans, params)
    }

    /// Parse a statement once into a reusable [`Prepared`] plan (P2.e). Parsing
    /// is separated from binding so the same plan can be executed many times
    /// with different `params` via [`Engine::execute_prepared`] — parse once,
    /// execute many.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        Ok(Prepared {
            plans: parse_sql(sql)?,
        })
    }

    /// Execute a previously [`prepare`](Engine::prepare)d plan with `params`
    /// bound by position (P2.e).
    pub fn execute_prepared(
        &self,
        xid: Xid,
        prepared: &Prepared,
        params: &[Literal],
    ) -> Result<Vec<ExecResult>> {
        self.run_bound_plans(xid, prepared.plans.clone(), params)
    }

    /// Shared execution loop for the parameterized entry points: bind `$n`
    /// placeholders, apply RLS, execute, and roll DDL back on failure (the same
    /// request-level catalog rollback [`Engine::execute_sql`] performs).
    fn run_bound_plans(
        &self,
        xid: Xid,
        plans: Vec<LogicalPlan>,
        params: &[Literal],
    ) -> Result<Vec<ExecResult>> {
        let page_size = self.page_size;
        let saved_catalog_root = ctrl_lock(&self.control).catalog_root;
        let mut results = Vec::with_capacity(plans.len());
        for mut plan in plans {
            // Bind before RLS/execute so a placeholder value can never be
            // interpreted as SQL structure.
            bind_params(&mut plan, params)?;
            let plan = apply_rls(plan, &cat_read(&self.catalog));
            let mut catalog = cat_write(&self.catalog);
            let mut ctx = ExecCtx {
                catalog: &mut catalog,
                txn_mgr: &self.txn_mgr,
                pool: &self.pool,
                wal: &self.wal,
                lock_mgr: &self.lock_mgr,
                control_path: &self.control_path,
                control: &self.control,
                page_size,
                xid,
                next_event_seq: &self.next_event_seq,
            };
            match executor::execute(plan, &mut ctx) {
                Ok(result) => {
                    self.note_dml_result(&result); // A1: dead/live-tuple accounting.
                    results.push(result);
                }
                Err(e) => {
                    drop(catalog);
                    self.restore_catalog_root(saved_catalog_root)?;
                    return Err(e);
                }
            }
        }
        Ok(results)
    }

    /// Parse and execute one Cypher query (M3.c): `MATCH (a)-[:TYPE]->(b)
    /// WHERE <predicate> RETURN <items>`. Mirrors `execute_sql`'s exact
    /// `ExecCtx` construction — single-statement only in v1, but returns
    /// `Vec<ExecResult>` for API symmetry and future multi-statement
    /// headroom.
    pub fn execute_cypher(&self, xid: Xid, query: &str) -> Result<Vec<ExecResult>> {
        let page_size = self.page_size;
        let parsed = parse_cypher(query)?;
        let mut catalog = cat_write(&self.catalog);
        let mut ctx = ExecCtx {
            catalog: &mut catalog,
            txn_mgr: &self.txn_mgr,
            pool: &self.pool,
            wal: &self.wal,
            lock_mgr: &self.lock_mgr,
            control_path: &self.control_path,
            control: &self.control,
            page_size,
            xid,
            next_event_seq: &self.next_event_seq,
        };
        let result = graph_executor::execute(parsed, &mut ctx, self.edge_index_meta)?;
        Ok(vec![result])
    }

    /// Attach a row-level-security policy to a table (M1: Rust API only,
    /// no `CREATE POLICY` SQL surface — see catalog.rs's module doc).
    pub fn set_rls_policy(&self, table: &str, policy: Expr) -> Result<()> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &self.pool,
            wal: &self.wal,
            control_path: &self.control_path,
            control: &self.control,
            page_size,
        };
        cat_write(&self.catalog).set_rls_policy(table, policy, &mut ctx)
    }

    /// Attach (or clear) a secondary index on one column (M2: Rust API
    /// only — `CREATE INDEX` SQL surface lands in M2.c). No backfill of
    /// already-committed rows happens here; those get indexed on the next
    /// `Engine::open`'s rebuild-on-open rescan. M2.c's `CREATE INDEX`
    /// backfills immediately instead, reusing this same catalog primitive.
    pub fn set_column_index(
        &self,
        table: &str,
        column: &str,
        kind: Option<IndexKind>,
    ) -> Result<()> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &self.pool,
            wal: &self.wal,
            control_path: &self.control_path,
            control: &self.control,
            page_size,
        };
        cat_write(&self.catalog).set_column_index(table, column, kind, &mut ctx)?;
        // C1 durability-claim audit: this is a **standalone** DDL-like operation
        // (no `xid`, no enclosing user commit to cover it), so it self-syncs its
        // catalog + index-backfill mini-txns before returning — preserving the
        // per-call durability contract it had before the commit-time-fsync flip.
        self.sync_wal()
    }

    /// Build status of a secondary index, or `None` if the column has no index.
    /// Since P3.c every index is durable and built synchronously as part of
    /// `CREATE INDEX`, so a present index is always [`IndexStatus::Ready`] — the
    /// async backfill window (and the `Building` state) no longer exist. Computed
    /// straight from the catalog; kept for the REST `GET /indexes/.../status`.
    pub fn index_status(&self, table: &str, column: &str) -> Option<IndexStatus> {
        let catalog = cat_read(&self.catalog);
        let table_def = catalog.lookup(table).ok()?;
        let col = table_def.columns.iter().find(|c| c.name == column)?;
        col.index.map(|_| IndexStatus::Ready)
    }

    // ── M4.a: event capture opt-in ──────────────────────────────────────────

    /// Opt a table into event capture (M4): from this point on, every
    /// INSERT/UPDATE/DELETE on `table` also durably writes a row to
    /// `__events__` under the same transaction (see
    /// `sql/executor.rs::send_event_capture`). Rejects `__events__`/
    /// `__consumers__` themselves as targets — defense in depth alongside
    /// the same guard in `send_event_capture`, following M2.a's
    /// "validate in more than one place" precedent for `VECTOR(n)`.
    pub fn enable_events(&self, table: &str) -> Result<()> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        if table == EVENTS_TABLE || table == CONSUMERS_TABLE {
            return Err(DbError::SqlPlan(format!(
                "cannot enable events on the system table '{table}' itself"
            )));
        }
        let page_size = self.page_size;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &self.pool,
            wal: &self.wal,
            control_path: &self.control_path,
            control: &self.control,
            page_size,
        };
        cat_write(&self.catalog).set_events_enabled(table, true, &mut ctx)?;
        // C1 durability-claim audit: standalone catalog mutation (no `xid`),
        // so it self-syncs before returning — same rationale as
        // `set_column_index` and the checkpoint/vacuum sites.
        self.sync_wal()
    }

    /// Fetch up to `limit` events with `seq` greater than `consumer`'s
    /// durable offset, ascending by `seq`. A pure read: an unregistered
    /// consumer is treated as offset 0 in-memory only — no
    /// `__consumers__` row is written here. Only `ack_events` durably
    /// advances a consumer's progress (M4.b), mirroring Kafka's manual-
    /// commit model: if offsets advanced on fetch, a crash between fetch
    /// and the caller actually processing the batch would silently skip
    /// events. No predicate pushdown exists — cost scales with
    /// `__events__`'s total row count, not with consumer lag or `limit`
    /// (see queue/mod.rs's module doc and `Engine::vacuum_events`, M4.c,
    /// which is the actual lever for this cost).
    pub fn poll_events(&self, xid: Xid, consumer: &str, limit: usize) -> Result<Vec<queue::Event>> {
        let page_size = self.page_size;
        let events_def = cat_read(&self.catalog).lookup(EVENTS_TABLE)?.clone();
        let consumers_def = cat_read(&self.catalog).lookup(CONSUMERS_TABLE)?.clone();
        let events_heap = Heap::open(page_size, events_def.fsm_meta, events_def.pages.clone());
        let consumers_heap = Heap::open(
            page_size,
            consumers_def.fsm_meta,
            consumers_def.pages.clone(),
        );
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;

        let offset =
            queue::find_consumer_offset(&consumers_heap, &snapshot, xid, &self.pool, consumer)?
                .map(|(_, offset)| offset)
                .unwrap_or(0);

        let mut events = Vec::new();
        for (_, bytes) in events_heap.scan(&snapshot, xid, &self.pool)? {
            let row = executor::decode_row(&bytes, &events_def.columns)?;
            let (
                Literal::Int(seq),
                Literal::Int(row_xid),
                Literal::Text(table_name),
                Literal::Text(op),
            ) = (&row[0], &row[1], &row[2], &row[3])
            else {
                continue;
            };
            if *seq <= offset {
                continue;
            }
            let payload = match &row[4] {
                Literal::Json(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            };
            events.push(queue::Event {
                seq: *seq,
                xid: *row_xid,
                table_name: table_name.clone(),
                op: op.clone(),
                payload,
            });
        }
        events.sort_by_key(|e| e.seq);
        events.truncate(limit);
        Ok(events)
    }

    /// Durably advance `consumer`'s offset to `up_to_seq` — the only
    /// operation in M4.b that writes to `__consumers__`. If the consumer
    /// has never acked before, this is where its row is created
    /// (auto-registration becomes durable on first ack, not on first
    /// poll).
    pub fn ack_events(&self, xid: Xid, consumer: &str, up_to_seq: i64) -> Result<()> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let consumers_def = cat_read(&self.catalog).lookup(CONSUMERS_TABLE)?.clone();
        let heap = Heap::open(
            page_size,
            consumers_def.fsm_meta,
            consumers_def.pages.clone(),
        );
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let existing = queue::find_consumer_offset(&heap, &snapshot, xid, &self.pool, consumer)?;

        let encoded = executor::encode_row(&queue::consumer_row(consumer, up_to_seq));
        match existing {
            Some((row_id, _)) => {
                let new_row_id =
                    heap.update(row_id, &encoded, xid, &self.pool, &self.wal, &self.lock_mgr)?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::XmaxStamp {
                        page_id: row_id.page_id,
                        slot: row_id.slot,
                    },
                )?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::Insert {
                        page_id: new_row_id.page_id,
                        slot: new_row_id.slot,
                    },
                )?;
            }
            None => {
                let row_id = heap.insert(&encoded, xid, &self.pool, &self.wal)?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::Insert {
                        page_id: row_id.page_id,
                        slot: row_id.slot,
                    },
                )?;
            }
        }

        if !heap.is_fsm_backed() && heap.page_ids() != consumers_def.pages.as_slice() {
            let mut cctx = CatalogCtx {
                pool: &self.pool,
                wal: &self.wal,
                control_path: &self.control_path,
                control: &self.control,
                page_size,
            };
            cat_write(&self.catalog).set_pages(
                CONSUMERS_TABLE,
                heap.page_ids().to_vec(),
                &mut cctx,
            )?;
        }
        Ok(())
    }

    /// Reclaim every `__events__` row every registered consumer has
    /// already acknowledged past — the actual resolution of the
    /// slow-consumer-vs-vacuum durability contract (see queue/mod.rs's
    /// module doc): a slow consumer's un-acked events simply accumulate in
    /// `__events__` rather than blocking WAL truncation, and this is the
    /// explicit, never-automatic lever for reclaiming them once every
    /// consumer has moved past. With zero registered consumers, this is a
    /// no-op that reclaims nothing — a not-yet-registered consumer might
    /// need full history. Deliberately **not** called from `Engine::
    /// checkpoint()` or any other automatic path, matching M1's
    /// zero-automatic-vacuum precedent.
    pub fn vacuum_events(&self, xid: Xid) -> Result<usize> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let consumers_def = cat_read(&self.catalog).lookup(CONSUMERS_TABLE)?.clone();
        let consumers_heap = Heap::open(
            page_size,
            consumers_def.fsm_meta,
            consumers_def.pages.clone(),
        );
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;

        let mut min_offset: Option<i64> = None;
        for (_, bytes) in consumers_heap.scan(&snapshot, xid, &self.pool)? {
            let row = executor::decode_row(&bytes, &consumers_def.columns)?;
            if let Literal::Int(offset) = row[1] {
                min_offset = Some(min_offset.map_or(offset, |m: i64| m.min(offset)));
            }
        }
        let Some(min_offset) = min_offset else {
            return Ok(0);
        };

        let events_def = cat_read(&self.catalog).lookup(EVENTS_TABLE)?.clone();
        let events_heap = Heap::open(page_size, events_def.fsm_meta, events_def.pages.clone());
        let to_reclaim: Vec<RowId> = events_heap
            .scan(&snapshot, xid, &self.pool)?
            .into_iter()
            .filter_map(|(row_id, bytes)| {
                let row = executor::decode_row(&bytes, &events_def.columns).ok()?;
                match row[0] {
                    Literal::Int(seq) if seq <= min_offset => Some(row_id),
                    _ => None,
                }
            })
            .collect();

        let mut reclaimed = 0usize;
        for row_id in to_reclaim {
            events_heap.delete(row_id, xid, &self.pool, &self.wal, &self.lock_mgr)?;
            self.txn_mgr.record_undo(
                xid,
                UndoAction::XmaxStamp {
                    page_id: row_id.page_id,
                    slot: row_id.slot,
                },
            )?;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    // ── M3.a: graph edges ───────────────────────────────────────────────────

    /// Insert one edge record into `__edges__`. Reconstructs its own `Heap`
    /// handle from the catalog's persisted page list — deliberately not
    /// `self.heap`, which has no table concept and backs only the raw
    /// `insert`/`get`/`update`/`delete` API above.
    pub fn create_edge(
        &self,
        xid: Xid,
        from_id: i64,
        to_id: i64,
        edge_type: &str,
        props: &str,
    ) -> Result<RowId> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog).lookup(edges::EDGES_TABLE)?.clone();
        let heap = Heap::open(page_size, table_def.fsm_meta, table_def.pages.clone());

        let encoded = executor::encode_row(&edges::edge_row(from_id, to_id, edge_type, props));
        let row_id = heap.insert(&encoded, xid, &self.pool, &self.wal)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;

        if !heap.is_fsm_backed() && heap.page_ids() != table_def.pages.as_slice() {
            let mut cctx = CatalogCtx {
                pool: &self.pool,
                wal: &self.wal,
                control_path: &self.control_path,
                control: &self.control,
                page_size,
            };
            cat_write(&self.catalog).set_pages(
                edges::EDGES_TABLE,
                heap.page_ids().to_vec(),
                &mut cctx,
            )?;
        }

        // P3.b: maintain the durable edge-adjacency index (a `DiskBTree` over
        // `__edges__.from_id`) synchronously and WAL-logged — the same durable
        // path a `BTree` column INSERT takes, so it is crash-recovered and
        // never rebuilt on open. (The M7 CSR index is retired — it was consulted
        // by no read path since the M7 traversal-uses-CSR revert, and adjacency
        // is now served durably here.)
        DiskBTree::new(self.edge_index_meta, page_size).insert(
            OrderedValue::Int(from_id),
            row_id,
            &self.pool,
            &self.wal,
        )?;
        Ok(row_id)
    }

    /// Delete one edge record. `from_id` is taken as an explicit parameter
    /// (the caller already has it from whatever scan/`edges_from` call
    /// located the row) to avoid a redundant `Heap::get` just to find it.
    pub fn delete_edge(&self, xid: Xid, row_id: RowId, from_id: i64) -> Result<()> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog).lookup(edges::EDGES_TABLE)?.clone();
        let heap = Heap::open(page_size, table_def.fsm_meta, table_def.pages.clone());

        heap.delete(row_id, xid, &self.pool, &self.wal, &self.lock_mgr)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        DiskBTree::new(self.edge_index_meta, page_size).remove(
            &OrderedValue::Int(from_id),
            row_id,
            &self.pool,
            &self.wal,
        )?;
        Ok(())
    }

    /// Traverse every edge out of `from_id`, MVCC-filtered against `xid`'s
    /// snapshot. `edge_index` is a candidate-fetcher, not a source of
    /// truth — every candidate `RowId` is re-resolved through the ordinary
    /// MVCC snapshot check (`resolve_candidates_batched`), so an edge whose
    /// creating transaction aborted never surfaces here even though the
    /// index may still reference it.
    pub fn edges_from(&self, xid: Xid, from_id: i64) -> Result<Vec<Edge>> {
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog).lookup(edges::EDGES_TABLE)?.clone();
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let candidates = DiskBTree::new(self.edge_index_meta, page_size)
            .search_eq(&OrderedValue::Int(from_id), &self.pool)?;
        let resolved = resolve_candidates_batched(
            &candidates,
            &snapshot,
            xid,
            &self.pool,
            &table_def.columns,
        )?;

        let mut out = Vec::with_capacity(resolved.len());
        for (row_id, row) in resolved {
            let to_id = match &row[1] {
                Literal::Int(n) => *n,
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.to_id decoded as non-Int: {other:?}"
                    )))
                }
            };
            let edge_type = match &row[2] {
                Literal::Text(s) => s.clone(),
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.edge_type decoded as non-Text: {other:?}"
                    )))
                }
            };
            let props = match &row[3] {
                Literal::Json(s) => s.clone(),
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.props decoded as non-Json: {other:?}"
                    )))
                }
            };
            out.push(Edge {
                row_id,
                to_id,
                edge_type,
                props,
            });
        }
        Ok(out)
    }

    /// Full-text search over a durable `FULLTEXT`-indexed column (P3.b): return
    /// every row of `table` whose `column` text contains **all** of `query`'s
    /// tokens (AND-only, matching the M2.c inverted-index semantics). Reads the
    /// durable on-disk B+tree — no rebuild, always crash-consistent. Every
    /// candidate is re-validated against `xid`'s MVCC snapshot, so an aborted or
    /// superseded row never surfaces even though the index may still reference
    /// it. Errors if the column has no built full-text index (Rust API only —
    /// there is still no `WHERE MATCH(...)` SQL surface).
    pub fn search_fulltext(
        &self,
        xid: Xid,
        table: &str,
        column: &str,
        query: &str,
    ) -> Result<Vec<Vec<Literal>>> {
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog).lookup(table)?.clone();
        let col = table_def
            .columns
            .iter()
            .find(|c| c.name == column && !c.dropped)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        let meta = match (col.index, col.index_root) {
            (Some(IndexKind::FullText), Some(m)) => m,
            _ => {
                return Err(DbError::SqlPlan(format!(
                    "column {column} has no full-text index"
                )))
            }
        };
        let tokens = fulltext::tokenize(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        // AND-only: intersect each token's posting list. Start from the shortest
        // list so the intersection shrinks fastest.
        let tree = DiskBTree::new(meta, page_size);
        let mut posting_lists: Vec<Vec<RowId>> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            posting_lists.push(tree.search_eq(&OrderedValue::Text(token.clone()), &self.pool)?);
        }
        posting_lists.sort_by_key(|l| l.len());
        let mut candidates: std::collections::HashSet<RowId> =
            posting_lists[0].iter().copied().collect();
        for list in &posting_lists[1..] {
            let set: std::collections::HashSet<RowId> = list.iter().copied().collect();
            candidates.retain(|r| set.contains(r));
            if candidates.is_empty() {
                break;
            }
        }

        let heap = Heap::open(page_size, table_def.fsm_meta, table_def.pages.clone());
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let mut out = Vec::new();
        for rid in candidates {
            match heap.get(rid, &snapshot, xid, &self.pool) {
                Ok(bytes) => out.push(executor::decode_row(&bytes, &table_def.columns)?),
                Err(DbError::NoVisibleVersion { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    // ── Large objects (P3.d) ────────────────────────────────────────────────

    /// Stream a large object into out-of-line chunked storage under `xid`,
    /// returning its `lob_id`. The chunks commit/abort **atomically with `xid`**
    /// (they are ordinary `__lobs__` rows written under the same transaction),
    /// so a caller can store a big value and its owning row in one transaction.
    /// Resident memory is one ~7 KiB chunk at a time — a multi-GB value never
    /// loads whole (the "without OOM" gate).
    pub fn put_large_object<R: std::io::Read>(&self, xid: Xid, reader: R) -> Result<i64> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let lob_id = self.next_lob_id.fetch_add(1, Ordering::SeqCst);
        let table_def = cat_read(&self.catalog)
            .lookup(large_object::LOBS_TABLE)?
            .clone();
        let heap = Heap::open(page_size, table_def.fsm_meta, table_def.pages.clone());
        let store = LobStore::new(self.lob_index_meta, page_size);
        store.write_stream(
            xid,
            lob_id,
            reader,
            &table_def,
            &heap,
            &self.pool,
            &self.wal,
            &self.txn_mgr,
        )?;
        self.persist_lobs_pages(&heap, &table_def.pages)?;
        Ok(lob_id)
    }

    /// Stream a large object out into `sink` one chunk at a time (never holding
    /// more than a chunk in memory), MVCC-filtered against `xid`. Returns bytes
    /// written; a `lob_id` with no visible chunks writes nothing.
    pub fn read_large_object<W: std::io::Write>(
        &self,
        xid: Xid,
        lob_id: i64,
        sink: W,
    ) -> Result<u64> {
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog)
            .lookup(large_object::LOBS_TABLE)?
            .clone();
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let store = LobStore::new(self.lob_index_meta, page_size);
        store.read_stream(lob_id, &table_def, &snapshot, xid, &self.pool, sink)
    }

    /// Delete every chunk of `lob_id` under `xid` (MVCC delete; the heap vacuum
    /// reclaims the dead chunk rows later). Returns the number of chunks removed.
    pub fn delete_large_object(&self, xid: Xid, lob_id: i64) -> Result<usize> {
        let _ws = serial_lock(&self.write_serial); // P5.e-3: serialize catalog/index writes
        let page_size = self.page_size;
        let table_def = cat_read(&self.catalog)
            .lookup(large_object::LOBS_TABLE)?
            .clone();
        let heap = Heap::open(page_size, table_def.fsm_meta, table_def.pages.clone());
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let store = LobStore::new(self.lob_index_meta, page_size);
        store.delete(
            xid,
            lob_id,
            &table_def,
            &heap,
            &self.pool,
            &self.wal,
            &self.lock_mgr,
            &self.txn_mgr,
            &snapshot,
        )
    }

    /// Persist `__lobs__`'s page list back to the catalog if the heap grew.
    fn persist_lobs_pages(&self, heap: &Heap, original: &[PageId]) -> Result<()> {
        if !heap.is_fsm_backed() && heap.page_ids() != original {
            let page_size = self.page_size;
            let mut cctx = CatalogCtx {
                pool: &self.pool,
                wal: &self.wal,
                control_path: &self.control_path,
                control: &self.control,
                page_size,
            };
            cat_write(&self.catalog).set_pages(
                large_object::LOBS_TABLE,
                heap.page_ids().to_vec(),
                &mut cctx,
            )?;
        }
        Ok(())
    }

    /// Begin a new transaction under READ COMMITTED (the default, D10).
    pub fn begin(&self) -> Result<Xid> {
        self.begin_with_isolation(IsolationLevel::ReadCommitted)
    }

    /// Begin a new transaction under an explicit isolation level (RC or RR,
    /// D10). The returned `xid` must eventually reach [`Self::commit`] or
    /// [`Self::abort`] — there is no timeout or automatic cleanup.
    pub fn begin_with_isolation(&self, isolation: IsolationLevel) -> Result<Xid> {
        self.txn_mgr.begin(isolation, &self.wal)
    }

    /// Commit `xid`, releasing every lock it held. `xid` is finished after
    /// this call and must not be reused.
    ///
    /// Under `SERIALIZABLE` (P1.d) the commit can be refused: if `xid` turned
    /// out to be a pivot in a dangerous rw-antidependency structure (e.g.
    /// write-skew), `TransactionManager::commit` returns `SerializationFailure`
    /// with `xid` still live, and this method rolls it back before returning
    /// the error — so the caller just sees `SerializationFailure` on a fully
    /// cleaned-up transaction, and should retry.
    pub fn commit(&self, xid: Xid) -> Result<()> {
        let commit_lsn = match self.txn_mgr.commit(xid, &self.wal, &self.lock_mgr) {
            Err(DbError::SerializationFailure { xid }) => {
                self.abort(xid)?;
                return Err(DbError::SerializationFailure { xid });
            }
            Err(e) => return Err(e),
            Ok(lsn) => lsn,
        };
        // Group commit (P5.e-3): force this transaction's commit record durable
        // before returning, coalescing with any concurrent committers behind a
        // single fsync. In the default (non-deferred) mode `commit_user_txn`
        // already fsynced, so this is a no-op fast path; in the server's
        // deferred mode this is where durability is actually forced, and the
        // more writers commit at once, the fewer fsyncs they collectively pay.
        // A read-only transaction (`None`) wrote no commit record and skips it.
        if let Some(lsn) = commit_lsn {
            self.wal.sync_up_to(lsn)?;
        }
        // P1.e: a commit is a quiescence boundary — the natural point to run an
        // auto-checkpoint if a trigger has fired.
        self.maybe_auto_checkpoint()?;
        self.commits.fetch_add(1, Ordering::Relaxed); // P6.g stat
        Ok(())
    }

    /// Auto-checkpoint (P1.e): if enabled, the engine is quiescent (no open
    /// transaction), and either the time or WAL-size trigger has fired, run the
    /// existing checkpoint path inline. Quiescence is required so the
    /// checkpoint's WAL truncation cannot discard an in-flight transaction's
    /// undo records. The WAL is synced first so a deferred-sync session's pages
    /// are durable before `flush_all` (D5).
    fn maybe_auto_checkpoint(&self) -> Result<()> {
        let cfg = *ctrl_lock_ac(&self.auto_checkpoint);
        if !cfg.enabled || self.txn_mgr.active_count() > 0 {
            return Ok(());
        }
        let by_time = ctrl_lock_lc(&self.last_checkpoint).elapsed() >= cfg.timeout;
        let by_size = self.wal.wal_bytes() >= cfg.max_wal_size;
        if by_time || by_size {
            tracing::info!(
                by_time,
                by_size,
                wal_bytes = self.wal.wal_bytes(),
                "auto-checkpoint triggered (P1.e)"
            );
            self.sync_wal()?;
            self.checkpoint()?;
            *ctrl_lock_lc(&self.last_checkpoint) = Instant::now();
            self.checkpoints_triggered.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Current auto-checkpoint policy (P1.e).
    pub fn auto_checkpoint_config(&self) -> AutoCheckpointConfig {
        *ctrl_lock_ac(&self.auto_checkpoint)
    }

    /// Replace the auto-checkpoint policy (P1.e). Resets the time trigger's
    /// clock so a freshly-lowered `timeout` doesn't fire on stale elapsed time.
    pub fn set_auto_checkpoint_config(&self, cfg: AutoCheckpointConfig) {
        *ctrl_lock_ac(&self.auto_checkpoint) = cfg;
        *ctrl_lock_lc(&self.last_checkpoint) = Instant::now();
    }

    /// How many auto-checkpoints have fired this session (P1.e) — for tests and
    /// observability.
    pub fn checkpoints_triggered(&self) -> u64 {
        self.checkpoints_triggered.load(Ordering::SeqCst)
    }

    /// Current autovacuum policy (A2).
    pub fn autovacuum_config(&self) -> AutoVacuumConfig {
        *ctrl_lock_av(&self.autovacuum)
    }

    /// Replace the autovacuum policy (A2). Takes effect on the launcher's next
    /// naptime wake-up.
    pub fn set_autovacuum_config(&self, cfg: AutoVacuumConfig) {
        *ctrl_lock_av(&self.autovacuum) = cfg;
    }

    /// How many autovacuum passes the background launcher has run this session
    /// (A4) — distinct from manual `Engine::vacuum` calls.
    pub fn autovacuums_triggered(&self) -> u64 {
        self.autovacuums_triggered.load(Ordering::Relaxed)
    }

    /// Whether the autovacuum trigger currently fires for the live estimates
    /// (A2): `dead > threshold + scale_factor * live`, and the policy is
    /// enabled. The background launcher (A3) calls this each naptime; exposed so
    /// tests can assert the policy without waiting on the thread.
    pub fn autovacuum_should_run(&self) -> bool {
        self.autovacuum_config()
            .should_vacuum(self.dead_tuple_estimate(), self.live_tuple_estimate())
    }

    /// Estimated dead tuple versions accumulated since the last vacuum (A1).
    /// The autovacuum trigger's numerator; an approximation, like Postgres's
    /// `n_dead_tup` (see the `dead_tuples` field).
    pub fn dead_tuple_estimate(&self) -> u64 {
        self.dead_tuples.load(Ordering::Relaxed)
    }

    /// Estimated live tuple count (A1) — Postgres `reltuples`. The autovacuum
    /// trigger's `live` term; refreshed exactly at each vacuum.
    pub fn live_tuple_estimate(&self) -> u64 {
        self.live_tuples.load(Ordering::Relaxed)
    }

    /// Record `n` freshly-dead versions (one `xmax` stamp each) for the
    /// autovacuum estimate (A1). Called from the raw-CRUD and SQL-statement
    /// chokepoints — never from `heap.rs`/recovery redo, which must not count.
    fn note_dead_tuples(&self, n: u64) {
        if n != 0 {
            self.dead_tuples.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Fold one successful SQL statement's row-count into the autovacuum
    /// estimates (A1). Mirrors the raw-CRUD chokepoints: UPDATE stamps dead
    /// versions, DELETE stamps dead versions and drops live rows, INSERT adds
    /// live rows. Other statement kinds don't churn tuples.
    fn note_dml_result(&self, result: &ExecResult) {
        match result {
            ExecResult::Updated { count } => self.note_dead_tuples(*count as u64),
            ExecResult::Deleted { count } => {
                self.note_dead_tuples(*count as u64);
                self.note_live_delta(-(*count as i64));
            }
            ExecResult::Inserted { count } => self.note_live_delta(*count as i64),
            _ => {}
        }
    }

    /// Adjust the live-tuple estimate by `delta` (A1): `+n` on INSERT, `-n` on
    /// DELETE, 0 on UPDATE (a new version replaces the old). Saturates at 0 so a
    /// drifted estimate can never wrap.
    fn note_live_delta(&self, delta: i64) {
        if delta == 0 {
            return;
        }
        // `fetch_update` keeps the saturating-subtract atomic against the
        // concurrent writers that share this counter (P5.e).
        let _ = self
            .live_tuples
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(if delta >= 0 {
                    cur.saturating_add(delta as u64)
                } else {
                    cur.saturating_sub((-delta) as u64)
                })
            });
    }

    /// A cloneable, `Send + Sync` handle for concurrent reads that run off the
    /// single writer thread (6b). Derived from the buffer pool's shared mmap
    /// and the shared transaction snapshot state, so many readers execute in
    /// parallel with each other and with the writer, coordinating only through
    /// MVCC snapshots. See [`crate::read_handle::ReadHandle`].
    pub fn read_handle(&self) -> ReadHandle {
        ReadHandle::new(
            self.pool.shared_reader(),
            self.txn_mgr.shared(),
            Arc::clone(&self.catalog),
        )
    }

    /// Toggle statement-level WAL fsync deferral. **`true` is now the default**
    /// (set by [`Self::open`]): group-committed force-log-at-commit, where
    /// statement mini-txns append without a per-statement fsync and
    /// [`Self::commit`] forces the transaction's commit record durable via the
    /// coalescing `Wal::sync_up_to` barrier — one fsync per transaction (C1).
    ///
    /// Passing `false` restores the legacy **per-statement** durability policy
    /// (every mini-txn fsyncs immediately). This is **not a user knob** — it
    /// exists so the crash-injection harness can exercise both policies; the
    /// buffer pool's eviction-forced sync (C2) makes the deferred default safe
    /// for working sets larger than the pool, so there is no longer a reason
    /// for a caller to opt out. `#[doc(hidden)]` for that reason.
    #[doc(hidden)]
    pub fn set_deferred_sync(&self, deferred: bool) {
        self.wal.set_deferred_sync(deferred);
    }

    /// Force the WAL to durable storage — the single fsync a group-commit
    /// batch issues after appending many transactions' commit records. Also
    /// advances the buffer pool's durable-frontier view (D5) so eviction can
    /// steal any now-durable dirty page.
    pub fn sync_wal(&self) -> Result<()> {
        self.wal.sync()?;
        self.pool.set_durable_wal_lsn(self.wal.durable_lsn());
        Ok(())
    }

    /// Abort `xid`, physically undoing its writes and releasing every lock
    /// it held. `xid` is finished after this call and must not be reused.
    pub fn abort(&self, xid: Xid) -> Result<()> {
        let r = self
            .txn_mgr
            .abort(xid, &self.pool, &self.heap, &self.wal, &self.lock_mgr);
        if r.is_ok() {
            self.aborts.fetch_add(1, Ordering::Relaxed); // P6.g stat
        }
        r
    }

    /// Insert one untyped byte-slice row, the lowest-level write primitive
    /// in this crate. Requires an already-open `xid` (from [`Self::begin`]
    /// or [`Self::begin_with_isolation`]); does not itself begin, commit,
    /// or abort anything — the caller owns the transaction's whole
    /// lifetime, exactly like every other method taking an `xid` parameter.
    pub fn insert(&self, xid: Xid, data: &[u8]) -> Result<RowId> {
        let rid = self.heap.insert(data, xid, &self.pool, &self.wal)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )?;
        self.note_live_delta(1); // A1: one new live tuple.
        Ok(rid)
    }

    /// Read one row by `RowId`, MVCC-filtered against `xid`'s snapshot.
    /// Requires an already-open `xid`; a purely-read call still needs one
    /// (there is no snapshot without a transaction) — the caller is
    /// responsible for eventually calling [`Self::commit`] or
    /// [`Self::abort`] on it, even for a read-only `xid`.
    pub fn get(&self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        self.heap.get(row_id, &snapshot, xid, &self.pool)
    }

    /// Update `row_id`, returning the new version's RowId (M1: UPDATE
    /// creates a new tuple version rather than overwriting in place, so the
    /// physical location may change; re-resolve via the returned RowId).
    pub fn update(&self, xid: Xid, row_id: RowId, new_data: &[u8]) -> Result<RowId> {
        let new_rid =
            self.heap
                .update(row_id, new_data, xid, &self.pool, &self.wal, &self.lock_mgr)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: new_rid.page_id,
                slot: new_rid.slot,
            },
        )?;
        self.note_dead_tuples(1); // A1: old version now dead; live count unchanged.
        Ok(new_rid)
    }

    /// Delete one row by `RowId`. Requires an already-open `xid`; does not
    /// commit or abort it.
    pub fn delete(&self, xid: Xid, row_id: RowId) -> Result<()> {
        self.heap
            .delete(row_id, xid, &self.pool, &self.wal, &self.lock_mgr)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        self.note_dead_tuples(1); // A1: deleted version now dead …
        self.note_live_delta(-1); // … and no longer live.
        Ok(())
    }

    /// Flush dirty pages, write a checkpoint WAL record, update the control
    /// file, and truncate the WAL. Operational/administrative — takes no
    /// `xid`, is not part of any user transaction's lifecycle, and is safe
    /// to call at any time (it only touches already-committed state).
    pub fn checkpoint(&self) -> Result<()> {
        // P6.b: hold the WAL truncation floor back to the minimum replication
        // slot position so a consumer's un-streamed segments survive the
        // checkpoint. No slots → `Lsn::MAX` → truncate freely to the ckpt LSN.
        let wal_retain_lsn = self.replication.min_restart_lsn().unwrap_or(Lsn::MAX);
        checkpoint::run(
            &self.pool,
            &self.wal,
            &self.control_path,
            &self.control,
            self.txn_mgr.next_xid(),
            wal_retain_lsn,
        )
    }

    /// Flush all dirty pages without a full checkpoint (used in tests).
    pub fn flush(&self) -> Result<()> {
        self.pool.flush_all(self.wal.durable_lsn())
    }

    // ── Replication slots + WAL shipping (P6.b) ────────────────────────────────

    /// Create a replication slot starting at the current WAL tail — the consumer
    /// (read replica, archiver) streams everything committed from now on and the
    /// slot pins the WAL so those records survive a checkpoint until confirmed.
    pub fn create_replication_slot(
        &self,
        name: &str,
        kind: replication::SlotKind,
    ) -> Result<replication::SlotInfo> {
        let start = self.wal.current_lsn();
        self.replication.create(name, start, kind)
    }

    /// Drop a replication slot, releasing its WAL retention (a dropped slot no
    /// longer holds the checkpoint truncation floor back).
    pub fn drop_replication_slot(&self, name: &str) -> Result<()> {
        self.replication.drop_slot(name)
    }

    /// Advance a slot's confirmed position after a consumer has durably applied
    /// up to `lsn`. Monotonic — a stale confirmation never rewinds retention.
    pub fn advance_replication_slot(&self, name: &str, lsn: Lsn) -> Result<()> {
        self.replication.advance(name, lsn)
    }

    /// Snapshot of every replication slot (for the REST layer + monitoring).
    pub fn replication_slots(&self) -> Vec<replication::SlotInfo> {
        self.replication.list()
    }

    /// The current WAL tail LSN — a replica's starting point for streaming.
    pub fn wal_current_lsn(&self) -> Lsn {
        self.wal.current_lsn()
    }

    /// The durable WAL frontier — the LSN of the last fsync'd record. Under the
    /// group-committed default this can trail [`Self::wal_current_lsn`] between
    /// commits; WAL shipping is capped here so a replica never receives records
    /// the primary has not made durable (C3, see `Wal::records_from`).
    pub fn wal_durable_lsn(&self) -> Lsn {
        self.wal.durable_lsn()
    }

    /// The database directory (parent of the control file) — used by backup and
    /// base-snapshot tooling (P6.d).
    pub fn data_dir(&self) -> &Path {
        self.control_path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// The control-file state a replica must adopt alongside the shipped WAL
    /// (P6.c): the live catalog root and next-xid counter (the catalog *content*
    /// rides the WAL, but its root pointer + xid counter are control-file state).
    pub fn primary_control(&self) -> crate::replication::PrimaryControl {
        // Read both control fields under a single lock — taking the `control`
        // Mutex twice in one statement would keep both guards alive to the end
        // of the statement and self-deadlock (the Mutex is not reentrant).
        let (page_size, catalog_root) = {
            let c = ctrl_lock(&self.control);
            (c.page_size, c.catalog_root)
        };
        crate::replication::PrimaryControl {
            page_size,
            catalog_root,
            next_xid: self.txn_mgr.next_xid(),
        }
    }

    /// Ship every WAL record after `from_lsn` as a framed byte stream (P6.b) the
    /// replica decodes with [`crate::wal::decode_stream`] and applies via redo.
    pub fn ship_wal(&self, from_lsn: Lsn) -> Result<Vec<u8>> {
        self.wal.ship_from(from_lsn)
    }

    // ── Backups + PITR (P6.d) ──────────────────────────────────────────────────

    /// Take an online **base backup** into `dest`: checkpoint (flush all pages +
    /// truncate WAL to a consistent point), then copy the DB directory. The
    /// result is directly openable (restore-to-base) and is the starting point
    /// for point-in-time recovery via [`crate::backup::restore`]. Returns the
    /// WAL LSN the backup is consistent as of.
    pub fn base_backup(&self, dest: &Path) -> Result<Lsn> {
        self.checkpoint()?;
        crate::backup::base_backup_dir(self.data_dir(), dest)?;
        Ok(self.wal_current_lsn())
    }

    /// Archive the WAL segment files into `archive_dir` (P6.d) for point-in-time
    /// recovery — a plain copy of the append-only segments. Re-run to pick up
    /// newly written records. Returns the number of segments archived.
    pub fn archive_wal(&self, archive_dir: &Path) -> Result<usize> {
        let wal_dir = self.data_dir().join("db.wal");
        crate::backup::archive_wal_dir(&wal_dir, archive_dir)
    }

    /// The synchronous-replica durability option (P6.c): block until every
    /// **synchronous** slot has confirmed (advanced past) `lsn`, or `timeout`
    /// elapses. A primary calls this after a commit's WAL is durable but before
    /// acknowledging it, so a failover to a sync replica loses no acknowledged
    /// commit. Returns `true` if all sync slots caught up, `false` on timeout
    /// (the caller decides whether to still acknowledge). No sync slots →
    /// returns `true` immediately (pure async replication). Opt-in: the default
    /// commit path stays async (the documented tradeoff — see the phase6 spec).
    pub fn wait_for_sync_replicas(&self, lsn: Lsn, timeout: Duration) -> Result<bool> {
        if !self.replication.has_sync() {
            return Ok(true);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let all_caught_up = self
                .replication
                .list()
                .iter()
                .filter(|s| s.kind == crate::replication::SlotKind::Sync)
                .all(|s| s.restart_lsn >= lsn);
            if all_caught_up {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Reclaim physical space held by dead tuple versions (M10) — the explicit,
    /// manually-triggered GC pass (there is no autovacuum in v1; this mirrors
    /// `vacuum_events`'s explicit-call model). For every table it: (a) computes
    /// the conservative visibility horizon over all live transactions **and**
    /// concurrent readers; (b) marks reclaimable versions' line pointers DEAD
    /// (redo-only `WAL_VACUUM`, D5); (c) scrubs every reclaimed `RowId` from
    /// the secondary indexes **before** any slot becomes reusable (the aliasing
    /// gate — see [`VacuumReport`] and MEMORY.md's M10.c note); then (d)
    /// compacts each touched page, promoting DEAD→UNUSED so the freed space can
    /// be handed to new tuples.
    ///
    /// Safe to call at any time — it only touches already-committed, dead-to-
    /// everyone state — and crash-safe: a crash mid-vacuum leaves either the
    /// pre- or post-mark state, never a lost committed row (its WAL records are
    /// idempotent redo, no undo).
    pub fn vacuum(&self) -> Result<VacuumReport> {
        self.vacuum_inner(true)
    }

    /// Vacuum with an explicit choice of whether to run the secondary-index
    /// clean pass (M10.c). `clean_indexes = true` is the only correct value for
    /// production (`Engine::vacuum`); `false` exists solely to *reproduce* the
    /// index-aliasing hazard in tests (skipping the gate lets a reused slot
    /// alias a stale index entry — see `lib.rs`'s M10.c regression test).
    fn vacuum_inner(&self, clean_indexes: bool) -> Result<VacuumReport> {
        // P5.e-3: vacuum mutates the same secondary-index trees + compacts heap
        // pages that the guarded write paths touch — serialize it with them.
        let _ws = serial_lock(&self.write_serial);
        let horizon = self.txn_mgr.vacuum_horizon();
        let page_size = self.page_size;
        let mut report = VacuumReport {
            horizon,
            horizon_blocked: horizon < self.txn_mgr.next_xid(),
            ..Default::default()
        };

        // Every catalog table (user tables + the system __edges__/__events__/
        // __consumers__ heaps). The raw byte-slice CRUD heap (`self.heap`,
        // untracked in the catalog and never secondary-indexed) is vacuumed
        // separately below.
        let table_defs: Vec<TableDef> = cat_read(&self.catalog).tables().cloned().collect();
        for table in &table_defs {
            let heap = Heap::open(page_size, table.fsm_meta, table.pages.clone());
            report.rows_scanned += count_live_slots(&heap, &self.pool)?;
            let reclaimable = heap.collect_reclaimable(horizon, &self.pool)?;
            if reclaimable.is_empty() {
                continue;
            }

            // P3.a/P3.b/P3.c: gather each reclaimable version's durable-index
            // key(s) *before* marking it DEAD — the tuple body is only readable
            // while the slot is still LIVE. These are scrubbed from the on-disk
            // structures in the aliasing gate below. BTree (one key, the value),
            // FullText (one key per token), and the durable edge index all become
            // `(meta_page, key, rid)` triples over a `DiskBTree`; the vector
            // (Hnsw/IVF) index instead records `(meta_page, vector, rid)` so the
            // IVF can re-derive the cell from the vector.
            let mut durable_removals: Vec<(PageId, OrderedValue, RowId)> = Vec::new();
            let mut ivf_removals: Vec<(PageId, Vec<f32>, RowId)> = Vec::new();
            let has_durable = table
                .columns
                .iter()
                .any(|c| !c.dropped && c.index_root.is_some());
            if clean_indexes && has_durable {
                for rid in &reclaimable {
                    let Ok(bytes) = heap.get_raw(*rid, &self.pool) else {
                        continue;
                    };
                    let row = executor::decode_row(&bytes, &table.columns)?;
                    for (i, col) in table.columns.iter().enumerate() {
                        let Some(root) = (if col.dropped { None } else { col.index_root }) else {
                            continue;
                        };
                        match col.index {
                            Some(IndexKind::BTree) => {
                                if let Ok(v) = OrderedValue::try_from(&row[i]) {
                                    durable_removals.push((root, v, *rid));
                                }
                            }
                            Some(IndexKind::FullText) => {
                                if let Literal::Text(text) = &row[i] {
                                    for token in fulltext::tokenize(text) {
                                        durable_removals.push((
                                            root,
                                            OrderedValue::Text(token),
                                            *rid,
                                        ));
                                    }
                                }
                            }
                            Some(IndexKind::Hnsw) => {
                                if let Literal::Vector(v) = &row[i] {
                                    ivf_removals.push((root, v.clone(), *rid));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // (b) Mark every reclaimable version DEAD (not yet reusable).
            for rid in &reclaimable {
                heap.mark_dead(*rid, &self.pool, &self.wal)?;
            }

            // (c) The aliasing gate: scrub the reclaimed RowIds from every
            // secondary index BEFORE their slots can be reused. Skipped only
            // when a test deliberately reproduces the hazard. All indexes are
            // durable now (synchronous, WAL-logged), so a reused slot can't
            // surface a stale candidate.
            if clean_indexes {
                for (root, value, rid) in &durable_removals {
                    let tree = DiskBTree::new(*root, page_size);
                    tree.remove(value, *rid, &self.pool, &self.wal)?;
                }
                for (root, vector, rid) in &ivf_removals {
                    let ivf = DiskIvfIndex::open(*root, page_size);
                    ivf.remove(*rid, vector, &self.pool, &self.wal)?;
                }
            }

            // (d) Compact each touched page: drop dead bodies, coalesce free
            // space, promote DEAD→UNUSED.
            for pid in unique_pages(&reclaimable) {
                report.bytes_reclaimed += heap.compact_page(pid, &self.pool, &self.wal)?;
            }
            report.versions_reclaimed += reclaimable.len();
            report.slots_freed += reclaimable.len();
        }

        // The raw-CRUD heap: no secondary indexes reference it, so no index
        // gate is needed — pure physical reclamation.
        report.rows_scanned += count_live_slots(&self.heap, &self.pool)?;
        let raw_reclaimable = self.heap.collect_reclaimable(horizon, &self.pool)?;
        if !raw_reclaimable.is_empty() {
            for rid in &raw_reclaimable {
                self.heap.mark_dead(*rid, &self.pool, &self.wal)?;
            }
            for pid in unique_pages(&raw_reclaimable) {
                report.bytes_reclaimed += self.heap.compact_page(pid, &self.pool, &self.wal)?;
            }
            report.versions_reclaimed += raw_reclaimable.len();
            report.slots_freed += raw_reclaimable.len();
        }

        // C1 durability-claim audit — vacuum is a **standalone** operation (no
        // enclosing user transaction whose commit `sync_up_to` would cover it),
        // so it self-syncs: force its `WAL_VACUUM` records durable before
        // returning so a caller that observes reclaimed space also observes it
        // durably. Crash-safety does not depend on this (the vacuum records are
        // idempotent redo-only, and D5 keeps any flushed page behind the durable
        // frontier), but the durability *claim* on return does. Cheap when
        // nothing was reclaimed (the WAL is already at its frontier).
        self.sync_wal()?;

        // A1: refresh the autovacuum estimates. `live` is now exactly the scanned
        // live-slot count (corrects any accumulated drift). `dead` drops by what
        // we physically reclaimed — normally to 0, but if the horizon was held
        // back (a long-lived reader / replication slot) the un-reclaimable
        // remainder stays counted, so the trigger re-fires once the horizon
        // advances rather than losing the signal (Postgres keeps not-yet-removable
        // dead tuples counted too).
        self.live_tuples
            .store(report.rows_scanned as u64, Ordering::Relaxed);
        let reclaimed = report.versions_reclaimed as u64;
        let _ = self
            .dead_tuples
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(reclaimed))
            });

        tracing::info!(
            horizon,
            versions_reclaimed = report.versions_reclaimed,
            bytes_reclaimed = report.bytes_reclaimed,
            horizon_blocked = report.horizon_blocked,
            "vacuum complete"
        );
        Ok(report)
    }
}

/// The distinct pages touched by a set of reclaimed `RowId`s, so each is
/// compacted exactly once (M10.d).
fn unique_pages(rows: &[RowId]) -> Vec<PageId> {
    let mut seen = std::collections::HashSet::new();
    rows.iter()
        .map(|r| r.page_id)
        .filter(|&p| seen.insert(p))
        .collect()
}

/// Count LIVE slots across a heap's pages (for the vacuum report's
/// `rows_scanned`), tolerating reclaimed (DEAD/UNUSED) slots.
fn count_live_slots(heap: &Heap, pool: &BufferPool) -> Result<usize> {
    heap.ensure_directory(pool)?; // FSM-backed: load the page directory first
    let mut n = 0;
    for page_id in heap.page_ids() {
        let page = pool.read_page(page_id)?;
        let sc = page.slot_count_pub();
        for slot in 0..sc {
            if matches!(page.slot_state(slot), Ok(crate::page::SlotState::Live)) {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Ensure the durable edge-adjacency index exists and return its meta page id
/// (P3.b). The edge index is a `DiskBTree` over `__edges__.from_id`, stored in
/// that column's `ColumnDef.index_root`. If it already exists (the normal case
/// on every reopen), this just returns the stored meta page — **no rebuild**,
/// the Phase-3 win. It is created (and any pre-existing committed edges
/// backfilled once) only the first time, e.g. on a database created before
/// P3.b or a freshly-created `__edges__` table. Idempotent.
#[allow(clippy::too_many_arguments)] // open-time wiring, mirrors rebuild_* helpers
fn ensure_edge_index(
    catalog: &mut Catalog,
    txn_mgr: &TransactionManager,
    pool: &BufferPool,
    wal: &Wal,
    lock_mgr: &LockManager,
    control_path: &Path,
    control: &Mutex<ControlData>,
    page_size: usize,
) -> Result<PageId> {
    // Already built? Reuse it — this is the no-rebuild-on-open fast path.
    let existing = catalog
        .lookup(edges::EDGES_TABLE)?
        .columns
        .iter()
        .find(|c| c.name == "from_id")
        .and_then(|c| c.index_root);
    if let Some(meta) = existing {
        return Ok(meta);
    }

    // First-time creation: build the tree and backfill committed edges (empty
    // on a fresh database; non-empty only when upgrading a pre-P3.b `__edges__`).
    let tree = DiskBTree::create(pool, wal)?;
    let table = catalog.lookup(edges::EDGES_TABLE)?.clone();
    let heap = Heap::open(page_size, table.fsm_meta, table.pages.clone());
    let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    for (row_id, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, &table.columns)?;
        if let Literal::Int(from_id) = row[0] {
            tree.insert(OrderedValue::Int(from_id), row_id, pool, wal)?;
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;

    // Persist `from_id`'s index = BTree + its meta page. Marking the column a
    // real BTree index means vacuum scrubs it via the generic durable-index
    // path and `SELECT * FROM __edges__ WHERE from_id = ?` is index-assisted for
    // free — `create_edge`/`delete_edge` keep it current via the same tree.
    let mut cctx = CatalogCtx {
        pool,
        wal,
        control_path,
        control,
        page_size,
    };
    catalog.set_column_index(
        edges::EDGES_TABLE,
        "from_id",
        Some(IndexKind::BTree),
        &mut cctx,
    )?;
    catalog.set_column_index_root(
        edges::EDGES_TABLE,
        "from_id",
        Some(tree.meta_page()),
        &mut cctx,
    )?;
    Ok(tree.meta_page())
}

/// Derive the next `seq` to assign in `__events__`, from its own
/// currently-committed rows — mirrors `TransactionManager::
/// recover_next_xid`'s "resume past the highest ever seen" approach and
/// `rebuild_edge_index`'s exact begin/scan/commit read-only transaction
/// template. Returns 1 if `__events__` is empty.
/// Derive the next `lob_id` (P3.d) from `__lobs__`'s highest committed `lob_id`
/// — mirrors `derive_next_event_seq`. Crash-safe (persisted as ordinary rows).
fn derive_next_lob_id(
    catalog: &Catalog,
    txn_mgr: &TransactionManager,
    pool: &BufferPool,
    wal: &Wal,
    lock_mgr: &LockManager,
    page_size: usize,
) -> Result<i64> {
    let table = catalog.lookup(large_object::LOBS_TABLE)?;
    let heap = Heap::open(page_size, table.fsm_meta, table.pages.clone());
    let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    let mut max_id: i64 = 0;
    for (_, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, &table.columns)?;
        if let Literal::Int(lob_id) = row[0] {
            max_id = max_id.max(lob_id);
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;
    Ok(max_id + 1)
}

fn derive_next_event_seq(
    catalog: &Catalog,
    txn_mgr: &TransactionManager,
    pool: &BufferPool,
    wal: &Wal,
    lock_mgr: &LockManager,
    page_size: usize,
) -> Result<u64> {
    let table = catalog.lookup(EVENTS_TABLE)?;
    let heap = Heap::open(page_size, table.fsm_meta, table.pages.clone());
    let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    let mut max_seq: u64 = 0;
    for (_, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, &table.columns)?;
        if let Literal::Int(seq) = row[0] {
            max_seq = max_seq.max(seq as u64);
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;
    Ok(max_seq + 1)
}

/// Initialize a `tracing_subscriber` with `RUST_LOG` env filter.
/// Call once at the start of your binary or test suite.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_insert_get_roundtrip() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"hello world").unwrap();
        let data = engine.get(xid, rid).unwrap();
        assert_eq!(data, b"hello world");
        engine.commit(xid).unwrap();
    }

    #[test]
    fn update_and_verify() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"initial_value").unwrap();
        let new_rid = engine.update(xid, rid, b"updated").unwrap();
        assert_eq!(engine.get(xid, new_rid).unwrap(), b"updated");
        engine.commit(xid).unwrap();
    }

    /// B1 regression: the SQL INSERT path builds a table **past the old
    /// ~1,450-page catalog-blob ceiling**. Before the durable FSM, every heap
    /// page alloc rewrote the whole page list into the single JSON catalog blob
    /// (`set_pages`); at ~1,450 pages that blob overflowed an 8 KiB page and the
    /// next INSERT died with `HeapFull { size: 8138 }`, capping SQL-built tables
    /// at ~145k small rows. The durable FSM moves the page directory out of the
    /// catalog into the per-table `DiskBTree`, so there is no blob to overflow.
    #[test]
    fn sql_insert_path_clears_old_catalog_pagelist_ceiling() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine.commit(x).unwrap();

        // ~4 KiB bodies pack ~2 rows per 8 KiB page, so a few thousand rows
        // clear the ~1,450-page ceiling without a million inserts. One
        // transaction (group-committed) keeps it fast.
        let body = "x".repeat(4000);
        let ins = engine
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap();
        const N: i64 = 3_400; // > 1,450 pages at ~2 rows/page
        let x = engine.begin().unwrap();
        for i in 0..N {
            engine
                .execute_prepared(x, &ins, &[Literal::Int(i), Literal::Text(body.clone())])
                .unwrap();
        }
        engine.commit(x).unwrap();

        // The durable directory holds more pages than the old ceiling — direct
        // proof the O(pages) blob cap is gone.
        let fsm_meta = cat_read(&engine.catalog).lookup("t").unwrap().fsm_meta;
        assert!(fsm_meta.is_some(), "table must be FSM-backed");
        let heap = Heap::open(engine.page_size, fsm_meta, Vec::new());
        heap.ensure_directory(&engine.pool).unwrap();
        let pages = heap.page_ids().len();
        assert!(
            pages > 1_450,
            "expected to clear the ~1,450-page ceiling, built only {pages} pages"
        );

        // And every row is durably readable back through the SQL path.
        let x = engine.begin().unwrap();
        let rows = match engine
            .execute_sql(x, "SELECT id FROM t")
            .unwrap()
            .pop()
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        engine.commit(x).unwrap();
        assert_eq!(rows.len(), N as usize);
    }

    #[test]
    fn delete_makes_row_gone() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"transient").unwrap();
        engine.delete(xid, rid).unwrap();
        assert!(engine.get(xid, rid).is_err());
        engine.commit(xid).unwrap();
    }

    #[test]
    fn reopen_after_flush_recovers_data() {
        let dir = tempdir().unwrap();
        let rid = {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            let rid = engine.insert(xid, b"durable").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            rid
        };
        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid2 = engine2.begin().unwrap();
        assert_eq!(engine2.get(xid2, rid).unwrap(), b"durable");
    }

    #[test]
    fn read_committed_sees_other_txns_committed_write() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        let b = engine.begin().unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        engine.commit(b).unwrap();
    }

    #[test]
    fn repeatable_read_does_not_see_write_committed_after_begin() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        // b begins under RR before a's write... actually a already committed
        // above, so instead: b begins RR, then c writes and commits, and b's
        // fixed snapshot must not see c's write even after it commits.
        let b = engine
            .begin_with_isolation(Isolation::RepeatableRead)
            .unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1"); // sees a's already-committed write

        let c = engine.begin().unwrap();
        let new_rid = engine.update(c, rid, b"v2").unwrap();
        engine.commit(c).unwrap();

        // b's RR snapshot predates c's commit, so it must still see v1 at
        // the original row_id (walking the version chain stops at v1).
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        // A fresh READ COMMITTED transaction sees the new committed version.
        let d = engine.begin().unwrap();
        assert_eq!(engine.get(d, new_rid).unwrap(), b"v2");
        engine.commit(b).unwrap();
        engine.commit(d).unwrap();
    }

    #[test]
    fn rollback_undoes_insert() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"oops").unwrap();
        engine.abort(a).unwrap();

        let b = engine.begin().unwrap();
        assert!(engine.get(b, rid).is_err());
    }

    #[test]
    fn xid_counter_survives_reopen() {
        let dir = tempdir().unwrap();
        let first_xid = {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.insert(xid, b"row").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            xid
        };
        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let next_xid = engine2.begin().unwrap();
        assert!(next_xid > first_xid, "reopened engine must not reuse xids");
    }

    // ── M5: xid continuity survives a checkpoint (WAL truncation), not just
    // an ordinary flush — regression test for a real bug found during M5's
    // manual server testing: `checkpoint::run` truncates every WAL record
    // before the checkpoint LSN, which in ordinary use is *every* prior
    // transaction's WAL_TXN_BEGIN record (a checkpoint only runs after
    // they've all committed). `recover_next_xid`'s WAL-scan-only approach
    // therefore has nothing left to find on the next open unless the
    // control file's own `next_xid` (persisted at checkpoint time, see
    // control.rs's module doc) also participates in the resume decision —
    // exactly the gap `xid_counter_survives_reopen` above never exercised,
    // since it calls `flush()` (no truncation), not `checkpoint()`.
    #[test]
    fn xid_counter_survives_reopen_after_checkpoint() {
        let dir = tempdir().unwrap();
        let last_xid_before_checkpoint = {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let mut last = 0;
            for i in 0..5u32 {
                let xid = engine.begin().unwrap();
                engine.insert(xid, &i.to_le_bytes()).unwrap();
                engine.commit(xid).unwrap();
                last = xid;
            }
            // Checkpoint truncates the WAL — every WAL_TXN_BEGIN record
            // above is now gone from the WAL file.
            engine.checkpoint().unwrap();
            last
        };

        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let resumed_xid = engine2.begin().unwrap();
        assert!(
            resumed_xid > last_xid_before_checkpoint,
            "reopening after a checkpoint must not reuse an already-committed xid: \
             resumed at {resumed_xid}, but xid {last_xid_before_checkpoint} was already used"
        );
    }

    // ── M1.b: SI abort-on-conflict (D12) ────────────────────────────────────

    #[test]
    fn concurrent_update_aborts_second_writer_immediately() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        // Two transactions both try to update the same row. Per D12, SI's
        // conflict handling is "abort immediately," not "block and wait" —
        // the second writer must fail right at the write call, not at
        // commit time (see txn.rs::commit's doc comment: because the lock
        // is held for the whole transaction lifetime, there's no separate
        // race window that a commit-time recheck would need to catch).
        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();

        let b = engine.begin().unwrap();
        let err = engine.update(b, rid, b"b-loses");
        assert!(
            matches!(err, Err(DbError::WriteConflict { .. })),
            "second writer must abort immediately on conflict, got {:?}",
            err
        );

        engine.commit(a).unwrap();
        engine.abort(b).unwrap();

        // a's write is the one that stuck.
        let c = engine.begin().unwrap();
        assert_eq!(engine.get(c, new_rid).unwrap(), b"a-wins");
    }

    #[test]
    fn commit_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();
        engine.commit(a).unwrap();

        // Now that a released its lock, a fresh writer can update the
        // *new* version without any conflict.
        let b = engine.begin().unwrap();
        engine.update(b, new_rid, b"b-after-a").unwrap();
        engine.commit(b).unwrap();
    }

    #[test]
    fn abort_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        engine.update(a, rid, b"a-abandoned").unwrap();
        engine.abort(a).unwrap();

        // a's abort released the lock (and undid the write), so b can
        // update the still-live original row.
        let b = engine.begin().unwrap();
        engine.update(b, rid, b"b-wins").unwrap();
        engine.commit(b).unwrap();

        let c = engine.begin().unwrap();
        assert!(engine.get(c, rid).is_err()); // superseded by b's update
    }

    // ── M1.c: SQL end-to-end ─────────────────────────────────────────────────

    #[test]
    fn execute_sql_full_round_trip() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                "CREATE TABLE accounts (id INT, name TEXT, balance INT)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (1, 'alice', 100)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (2, 'bob', 50)",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(xid2, "SELECT * FROM accounts WHERE id = 1")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SqlResult::Rows(rows) if rows.len() == 1));

        engine
            .execute_sql(xid2, "UPDATE accounts SET balance = 200 WHERE id = 1")
            .unwrap();
        let reselect = engine
            .execute_sql(xid2, "SELECT balance FROM accounts WHERE id = 1")
            .unwrap();
        match &reselect[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(200))
            }
            other => panic!("expected Rows, got {other:?}"),
        }

        engine
            .execute_sql(xid2, "DELETE FROM accounts WHERE id = 2")
            .unwrap();
        engine.commit(xid2).unwrap();

        let xid3 = engine.begin().unwrap();
        let remaining = engine.execute_sql(xid3, "SELECT * FROM accounts").unwrap();
        assert!(matches!(&remaining[0], SqlResult::Rows(rows) if rows.len() == 1));
    }

    #[test]
    fn failed_multi_statement_request_rolls_back_ddl() {
        // P2.c: a `;`-separated request whose first statement is DDL and whose
        // second statement fails must leave the schema untouched — the catalog
        // change is rolled back even though the catalog persists eagerly.
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let res = engine.execute_sql(
            xid,
            "CREATE TABLE t (id INT); INSERT INTO missing_table (id) VALUES (1)",
        );
        assert!(res.is_err());
        engine.abort(xid).unwrap();

        // `t` must not exist — the CREATE TABLE was rolled back.
        let xid2 = engine.begin().unwrap();
        assert!(matches!(
            engine.execute_sql(xid2, "SELECT * FROM t"),
            Err(DbError::TableNotFound(_))
        ));
        engine.abort(xid2).unwrap();

        // And the catalog is still usable afterwards.
        let xid3 = engine.begin().unwrap();
        engine
            .execute_sql(xid3, "CREATE TABLE ok (id INT)")
            .unwrap();
        engine.commit(xid3).unwrap();
    }

    #[test]
    fn alter_and_drop_survive_reopen() {
        // P2.c: schema changes persist across an engine reopen.
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, "CREATE TABLE t (a INT, b INT)")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (a, b) VALUES (1, 2)")
                .unwrap();
            engine
                .execute_sql(xid, "ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'")
                .unwrap();
            engine
                .execute_sql(xid, "ALTER TABLE t DROP COLUMN b")
                .unwrap();
            engine.commit(xid).unwrap();
            engine.checkpoint().unwrap();
        }
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rows = engine.execute_sql(xid, "SELECT a, c FROM t").unwrap();
        match &rows[0] {
            SqlResult::Rows(r) => assert_eq!(
                r,
                &vec![vec![
                    crate::sql::logical::Literal::Int(1),
                    crate::sql::logical::Literal::Text("x".to_string())
                ]]
            ),
            other => panic!("expected Rows, got {other:?}"),
        }
        // Dropped column stays gone after reopen.
        assert!(matches!(
            engine.execute_sql(xid, "SELECT b FROM t"),
            Err(DbError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn serial_sequence_survives_reopen() {
        // P2.d: the SERIAL counter is durable — after a reopen it continues
        // past the last-handed-out value, never reusing an id.
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, "CREATE TABLE t (id SERIAL, v INT)")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (v) VALUES (10)")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (v) VALUES (20)")
                .unwrap();
            engine.commit(xid).unwrap();
            engine.checkpoint().unwrap();
        }
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (v) VALUES (30)")
            .unwrap();
        engine.commit(xid).unwrap();
        let xid2 = engine.begin().unwrap();
        let rows = engine
            .execute_sql(xid2, "SELECT id FROM t WHERE v = 30")
            .unwrap();
        match &rows[0] {
            SqlResult::Rows(r) => {
                // Must be 3, not a reused 1 — the sequence resumed after reopen.
                assert_eq!(r, &vec![vec![crate::sql::logical::Literal::Int(3)]]);
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn bind_params_treats_malicious_value_as_data() {
        // P2.e: a bound value that would be catastrophic as an interpolated
        // string literal is treated purely as data — no SQL is re-parsed.
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine.commit(xid).unwrap();

        let attack = Literal::Text("'; DROP TABLE t; --".to_string());
        let xid2 = engine.begin().unwrap();
        // The malicious string matches no row and executes no injected SQL.
        let rows = engine
            .execute_sql_params(
                xid2,
                "SELECT * FROM t WHERE name = $1",
                std::slice::from_ref(&attack),
            )
            .unwrap();
        assert!(matches!(&rows[0], SqlResult::Rows(r) if r.is_empty()));
        engine.commit(xid2).unwrap();

        // The table still exists and its row is intact — nothing was dropped.
        let xid3 = engine.begin().unwrap();
        let all = engine.execute_sql(xid3, "SELECT * FROM t").unwrap();
        assert!(matches!(&all[0], SqlResult::Rows(r) if r.len() == 1));

        // Binding that exact string as an INSERT value stores it verbatim.
        engine
            .execute_sql_params(
                xid3,
                "INSERT INTO t (id, name) VALUES ($1, $2)",
                &[Literal::Int(2), attack.clone()],
            )
            .unwrap();
        let found = engine
            .execute_sql_params(xid3, "SELECT id FROM t WHERE name = $1", &[attack])
            .unwrap();
        match &found[0] {
            SqlResult::Rows(r) => assert_eq!(r, &vec![vec![Literal::Int(2)]]),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn bind_params_out_of_range_errors() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        // `$2` referenced but only one value supplied.
        let res =
            engine.execute_sql_params(xid, "SELECT * FROM t WHERE id = $2", &[Literal::Int(1)]);
        assert!(matches!(res, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn prepared_plan_reused_across_executions() {
        // P2.e: parse once, execute many with different bind values.
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'a')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (2, 'b')")
            .unwrap();
        engine.commit(xid).unwrap();

        let stmt = engine.prepare("SELECT name FROM t WHERE id = $1").unwrap();
        let xid2 = engine.begin().unwrap();
        let r1 = engine
            .execute_prepared(xid2, &stmt, &[Literal::Int(1)])
            .unwrap();
        let r2 = engine
            .execute_prepared(xid2, &stmt, &[Literal::Int(2)])
            .unwrap();
        match (&r1[0], &r2[0]) {
            (SqlResult::Rows(a), SqlResult::Rows(b)) => {
                assert_eq!(a, &vec![vec![Literal::Text("a".to_string())]]);
                assert_eq!(b, &vec![vec![Literal::Text("b".to_string())]]);
            }
            _ => panic!("expected Rows"),
        }
    }

    // ── M2.a: VECTOR(n) end-to-end ──────────────────────────────────────────

    #[test]
    fn execute_sql_vector_round_trip() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(4))")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2, 0.3, 0.4])",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(xid2, "SELECT * FROM t WHERE id = 1")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(
                    rows[0][1],
                    crate::sql::logical::Literal::Vector(vec![0.1, 0.2, 0.3, 0.4])
                );
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_sql_vector_dimension_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(4))")
            .unwrap();
        let err = engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2])")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    // ── P3.c: durable vector index (IVF-Flat) ───────────────────────────────

    /// Collect the integer `id`s a NEAR query returns, in order.
    fn near_ids(engine: &mut Engine, xid: Xid, sql: &str) -> Vec<i64> {
        match &engine.execute_sql(xid, sql).unwrap()[0] {
            SqlResult::Rows(rows) => rows
                .iter()
                .map(|r| match r[0] {
                    crate::sql::logical::Literal::Int(n) => n,
                    ref other => panic!("expected Int id, got {other:?}"),
                })
                .collect(),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn live_insert_into_durable_vector_index_is_queryable() {
        // A row inserted after CREATE INDEX is maintained synchronously in the
        // durable IVF index (no async worker) and immediately queryable by NEAR.
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2])")
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let ids = near_ids(
            &mut engine,
            xid2,
            "SELECT id FROM t WHERE NEAR(embedding, [0.1, 0.2], 1)",
        );
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn vector_index_is_durable_no_rebuild_on_reopen() {
        // P3.c moat: the vector index is durable, so a fresh open reconstructs
        // nothing from the heap — it reads the IVF meta/centroid pages straight
        // from disk — and NEAR still returns the right nearest neighbor.
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
                .unwrap();
            engine
                .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [1.0, 1.0])")
                .unwrap();
            engine
                .execute_sql(
                    xid,
                    "INSERT INTO t (id, embedding) VALUES (2, [50.0, 50.0])",
                )
                .unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }

        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let ids = near_ids(
            &mut engine2,
            xid,
            "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 1)",
        );
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn engine_drop_returns_promptly() {
        // The async worker is retired; Drop is trivial. This just guards against
        // a future field re-introducing a blocking teardown.
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        drop(engine);
    }

    #[test]
    fn index_status_is_ready_for_durable_index() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        assert_eq!(engine.index_status("t", "embedding"), None);
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine.commit(xid).unwrap();
        assert_eq!(
            engine.index_status("t", "embedding"),
            Some(IndexStatus::Ready)
        );
    }

    // ── M2.c: CREATE INDEX (full-text) ──────────────────────────────────────

    #[test]
    fn create_index_fulltext_backfills_immediately_and_is_queryable() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, body) VALUES (1, 'rust database engine')",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, body) VALUES (2, 'python web framework')",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine
            .execute_sql(xid2, "CREATE INDEX idx ON t USING FULLTEXT (body)")
            .unwrap();
        assert_eq!(result[0], SqlResult::CreatedIndex);
        engine.commit(xid2).unwrap();

        // P3.b: the full-text index is durable and synchronous — no `Ready`
        // wait — and now has a real read path (`Engine::search_fulltext`).
        let xid3 = engine.begin().unwrap();
        let rust_hits = engine.search_fulltext(xid3, "t", "body", "rust").unwrap();
        let python_hits = engine.search_fulltext(xid3, "t", "body", "python").unwrap();
        assert_eq!(rust_hits.len(), 1);
        assert_eq!(python_hits.len(), 1);
        assert_ne!(rust_hits, python_hits);
        assert!(engine
            .search_fulltext(xid3, "t", "body", "nonexistent")
            .unwrap()
            .is_empty());
        // AND-only intersection: only row 1 has both "rust" and "database".
        assert_eq!(
            engine
                .search_fulltext(xid3, "t", "body", "rust database")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn create_index_rejects_type_mismatch_via_sql() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        let err = engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (body)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    // ── M2.d: NEAR ───────────────────────────────────────────────────────────

    #[test]
    fn near_query_returns_nearest_neighbors_in_order() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.0, 0.0])")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, embedding) VALUES (2, [100.0, 100.0])",
            )
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (3, [0.1, 0.1])")
            .unwrap();
        engine.commit(xid).unwrap();
        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid2,
                "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 2)",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(1));
                assert_eq!(rows[1][0], crate::sql::logical::Literal::Int(3));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn near_composes_with_ordinary_where_predicate() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                "CREATE TABLE t (id INT, tag TEXT, embedding VECTOR(2))",
            )
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, tag, embedding) VALUES (1, 'a', [0.0, 0.0])",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, tag, embedding) VALUES (2, 'b', [0.1, 0.1])",
            )
            .unwrap();
        engine.commit(xid).unwrap();
        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid2,
                "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 5) AND tag = 'b'",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(2));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn sql_survives_reopen() {
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id) VALUES (7)")
                .unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }
        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let result = engine2.execute_sql(xid, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // ── P1.d: isolation correctness ──────────────────────────────────────────

    /// The canonical SSI test (P1.d): a write-skew schedule commits under
    /// REPEATABLE READ / snapshot isolation (the anomaly SI permits) but is
    /// aborted under SERIALIZABLE. Two transactions each read the on-call set
    /// then take a *different* doctor off call; row-disjoint writes, so neither
    /// blocks the other, yet together they violate the "at least one on call"
    /// invariant — exactly what SSI's rw-antidependency (pivot) detection stops.
    #[test]
    fn write_skew_commits_under_rr_but_aborts_under_serializable() {
        fn run(iso: Isolation) -> (Result<()>, Result<()>) {
            let dir = tempdir().unwrap();
            let engine = Engine::open(dir.path(), 0).unwrap();
            let s = engine.begin().unwrap();
            engine
                .execute_sql(s, "CREATE TABLE doctors (id INT, on_call INT)")
                .unwrap();
            engine
                .execute_sql(s, "INSERT INTO doctors (id, on_call) VALUES (1, 1)")
                .unwrap();
            engine
                .execute_sql(s, "INSERT INTO doctors (id, on_call) VALUES (2, 1)")
                .unwrap();
            engine.commit(s).unwrap();

            let t1 = engine.begin_with_isolation(iso).unwrap();
            let t2 = engine.begin_with_isolation(iso).unwrap();
            // Each reads the whole on-call set (the overlapping read set).
            engine
                .execute_sql(t1, "SELECT id FROM doctors WHERE on_call = 1")
                .unwrap();
            engine
                .execute_sql(t2, "SELECT id FROM doctors WHERE on_call = 1")
                .unwrap();
            // Each takes a *different* doctor off call (row-disjoint writes).
            engine
                .execute_sql(t1, "UPDATE doctors SET on_call = 0 WHERE id = 1")
                .unwrap();
            engine
                .execute_sql(t2, "UPDATE doctors SET on_call = 0 WHERE id = 2")
                .unwrap();
            let c1 = engine.commit(t1);
            let c2 = engine.commit(t2);
            (c1, c2)
        }

        // RR/SI permits write-skew: both commit.
        let (c1, c2) = run(Isolation::RepeatableRead);
        assert!(
            c1.is_ok() && c2.is_ok(),
            "RR must permit write-skew (both commit): {c1:?} {c2:?}"
        );

        // SERIALIZABLE catches the pivot: at least one transaction aborts with
        // a SerializationFailure.
        let (c1, c2) = run(Isolation::Serializable);
        assert!(
            c1.is_err() || c2.is_err(),
            "SERIALIZABLE must abort a write-skew transaction"
        );
        for c in [c1, c2] {
            if let Err(e) = c {
                assert!(
                    matches!(e, DbError::SerializationFailure { .. }),
                    "SSI abort must be a SerializationFailure, got {e:?}"
                );
            }
        }
    }

    /// P1.d: under READ COMMITTED, updating a row another transaction already
    /// updated-and-committed must NOT spuriously abort — RC's fresh
    /// per-statement snapshot re-reads the latest committed version and applies
    /// to it (EvalPlanQual via re-scan).
    #[test]
    fn read_committed_concurrent_update_does_not_spuriously_abort() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let s = engine.begin().unwrap();
        engine
            .execute_sql(s, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(s, "INSERT INTO t (id, v) VALUES (1, 10)")
            .unwrap();
        engine.commit(s).unwrap();

        // A commits an update first.
        let a = engine.begin().unwrap();
        engine
            .execute_sql(a, "UPDATE t SET v = 20 WHERE id = 1")
            .unwrap();
        engine.commit(a).unwrap();

        // B (RC) updates the same row afterward — no spurious conflict.
        let b = engine.begin().unwrap();
        let r = engine.execute_sql(b, "UPDATE t SET v = 30 WHERE id = 1");
        assert!(
            r.is_ok(),
            "RC update after a committed concurrent update must not abort: {r:?}"
        );
        engine.commit(b).unwrap();

        let q = engine.begin().unwrap();
        match &engine
            .execute_sql(q, "SELECT v FROM t WHERE id = 1")
            .unwrap()[0]
        {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    /// P1.d: under REPEATABLE READ, writing a row a concurrent transaction
    /// has updated-and-committed since this txn's snapshot is a genuine
    /// serialization anomaly — surfaced as `SerializationFailure`, not a raw
    /// `WriteConflict` (the M1.c "conflicts propagate regardless of isolation"
    /// gap, now closed).
    #[test]
    fn repeatable_read_write_over_committed_update_is_serialization_failure() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let s = engine.begin().unwrap();
        engine
            .execute_sql(s, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(s, "INSERT INTO t (id, v) VALUES (1, 10)")
            .unwrap();
        engine.commit(s).unwrap();

        // B fixes its snapshot (RR) by reading before A commits.
        let b = engine
            .begin_with_isolation(Isolation::RepeatableRead)
            .unwrap();
        engine
            .execute_sql(b, "SELECT v FROM t WHERE id = 1")
            .unwrap();

        // A updates and commits after B's snapshot.
        let a = engine.begin().unwrap();
        engine
            .execute_sql(a, "UPDATE t SET v = 20 WHERE id = 1")
            .unwrap();
        engine.commit(a).unwrap();

        // B writes the version it still sees — a lost-update conflict.
        let r = engine.execute_sql(b, "UPDATE t SET v = 30 WHERE id = 1");
        assert!(
            matches!(r, Err(DbError::SerializationFailure { .. })),
            "RR write over a committed concurrent update must be SerializationFailure: {r:?}"
        );
        engine.abort(b).unwrap();
    }

    /// P1.d: a serializable transaction with no rw-conflict (touches rows
    /// nobody else reads/writes) commits normally — SSI must not over-abort the
    /// common case.
    #[test]
    fn serializable_non_conflicting_transaction_commits() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let s = engine.begin().unwrap();
        engine
            .execute_sql(s, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(s, "INSERT INTO t (id, v) VALUES (1, 1)")
            .unwrap();
        engine.commit(s).unwrap();

        let t = engine
            .begin_with_isolation(Isolation::Serializable)
            .unwrap();
        engine
            .execute_sql(t, "SELECT v FROM t WHERE id = 1")
            .unwrap();
        engine
            .execute_sql(t, "UPDATE t SET v = 2 WHERE id = 1")
            .unwrap();
        assert!(
            engine.commit(t).is_ok(),
            "a lone serializable txn must commit"
        );
    }

    // ── P1.e: auto-checkpoint ─────────────────────────────────────────────────

    /// P1.e: the WAL-size trigger fires an auto-checkpoint inline on commit
    /// (bounding WAL growth), and the committed data survives a reopen even
    /// though the auto-checkpoint truncated the WAL along the way.
    #[test]
    fn auto_checkpoint_fires_on_wal_size_and_data_survives() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        // A tiny WAL-size threshold so a handful of inserts crosses it; disable
        // the time trigger so the test doesn't depend on wall-clock.
        engine.set_auto_checkpoint_config(AutoCheckpointConfig {
            enabled: true,
            timeout: std::time::Duration::from_secs(3600),
            max_wal_size: 2048,
        });

        let s = engine.begin().unwrap();
        engine.execute_sql(s, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(s).unwrap();

        for i in 0..50 {
            let x = engine.begin().unwrap();
            engine
                .execute_sql(x, &format!("INSERT INTO t (id) VALUES ({i})"))
                .unwrap();
            engine.commit(x).unwrap();
        }
        assert!(
            engine.checkpoints_triggered() > 0,
            "auto-checkpoint must fire once the WAL crosses max_wal_size"
        );

        // Reopen: the auto-checkpoints truncated the WAL, so recovery must come
        // from the checkpointed pages — all 50 rows must still be present.
        drop(engine);
        let engine = Engine::open(dir.path(), 0).unwrap();
        let q = engine.begin().unwrap();
        match &engine.execute_sql(q, "SELECT id FROM t").unwrap()[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 50, "all rows must survive"),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    /// P1.e: auto-checkpoint never fires while a transaction is open (a
    /// non-quiescent point) — running it there could truncate an in-flight
    /// transaction's undo records.
    #[test]
    fn auto_checkpoint_does_not_fire_mid_transaction() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_auto_checkpoint_config(AutoCheckpointConfig {
            enabled: true,
            timeout: std::time::Duration::from_secs(3600),
            max_wal_size: 1, // would fire on the very first append if not gated
        });
        let s = engine.begin().unwrap();
        engine.execute_sql(s, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(s).unwrap();

        // One long-lived transaction stays open across many writes: the commit
        // boundary that checks the trigger is never reached with a quiescent
        // engine, so no auto-checkpoint fires mid-transaction.
        let x = engine.begin().unwrap();
        for i in 0..20 {
            engine
                .execute_sql(x, &format!("INSERT INTO t (id) VALUES ({i})"))
                .unwrap();
        }
        // Still open → the only commit so far (the CREATE TABLE txn) is the only
        // quiescence point that ran a check; the open txn blocks further ones.
        let before = engine.checkpoints_triggered();
        engine.commit(x).unwrap(); // now quiescent → a checkpoint may fire here
        assert!(
            engine.checkpoints_triggered() >= before,
            "counter is monotonic"
        );
        // The point is that no checkpoint fired *while x was open*; if the gate
        // were missing, max_wal_size=1 would have fired one per statement.
        assert!(
            before <= 1,
            "no auto-checkpoint may fire mid-transaction (got {before})"
        );
    }

    #[test]
    fn rls_policy_filters_rows() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, owner TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (1, 'alice')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (2, 'bob')")
            .unwrap();
        engine.commit(xid).unwrap();

        let policy = crate::sql::logical::Expr::BinOp {
            op: crate::sql::logical::CmpOp::Eq,
            lhs: Box::new(crate::sql::logical::Expr::Column("owner".to_string())),
            rhs: Box::new(crate::sql::logical::Expr::Literal(
                crate::sql::logical::Literal::Text("alice".to_string()),
            )),
        };
        engine.set_rls_policy("t", policy).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine.execute_sql(xid2, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    fn sorted_names(result: &SqlResult) -> Vec<String> {
        match result {
            SqlResult::Rows(rows) => {
                let mut names: Vec<String> = rows
                    .iter()
                    .map(|r| match &r[0] {
                        crate::sql::logical::Literal::Text(s) => s.clone(),
                        other => panic!("expected Text, got {other:?}"),
                    })
                    .collect();
                names.sort();
                names
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    /// M6 differential proof: an index-assisted equality/range `SELECT`
    /// must return exactly the same rows as an unindexed full scan of
    /// identical data — the index is purely a performance optimization,
    /// invisible in the result set.
    #[test]
    fn btree_assisted_select_matches_full_scan_equality_and_range() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE indexed (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE plain (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON indexed USING BTREE (id)")
            .unwrap();
        for (id, name) in [(1, "a"), (2, "b"), (3, "c"), (2, "d"), (4, "e")] {
            engine
                .execute_sql(
                    xid,
                    &format!("INSERT INTO indexed (id, name) VALUES ({id}, '{name}')"),
                )
                .unwrap();
            engine
                .execute_sql(
                    xid,
                    &format!("INSERT INTO plain (id, name) VALUES ({id}, '{name}')"),
                )
                .unwrap();
        }
        engine.commit(xid).unwrap();

        // P3.a: the durable B-Tree is built synchronously with each INSERT —
        // no async worker, no `Ready` status to wait on.
        let xid2 = engine.begin().unwrap();

        // Equality.
        let indexed_eq = engine
            .execute_sql(xid2, "SELECT name FROM indexed WHERE id = 2")
            .unwrap();
        let plain_eq = engine
            .execute_sql(xid2, "SELECT name FROM plain WHERE id = 2")
            .unwrap();
        assert_eq!(sorted_names(&indexed_eq[0]), sorted_names(&plain_eq[0]));
        assert_eq!(sorted_names(&indexed_eq[0]), vec!["b", "d"]);

        // Range (>).
        let indexed_gt = engine
            .execute_sql(xid2, "SELECT name FROM indexed WHERE id > 2")
            .unwrap();
        let plain_gt = engine
            .execute_sql(xid2, "SELECT name FROM plain WHERE id > 2")
            .unwrap();
        assert_eq!(sorted_names(&indexed_gt[0]), sorted_names(&plain_gt[0]));
        assert_eq!(sorted_names(&indexed_gt[0]), vec!["c", "e"]);

        // Range (<=).
        let indexed_le = engine
            .execute_sql(xid2, "SELECT name FROM indexed WHERE id <= 2")
            .unwrap();
        let plain_le = engine
            .execute_sql(xid2, "SELECT name FROM plain WHERE id <= 2")
            .unwrap();
        assert_eq!(sorted_names(&indexed_le[0]), sorted_names(&plain_le[0]));
        assert_eq!(sorted_names(&indexed_le[0]), vec!["a", "b", "d"]);
    }

    /// M6: the index-assisted `exec_select` path must still respect RLS.
    /// Both rows share `id = 1` (so a BTree lookup on `id` returns both as
    /// raw candidates), but only one has `owner = 'alice'` — proving the
    /// RLS-AND'd predicate is still applied to every index-sourced
    /// candidate, not bypassed by the index shortcut.
    #[test]
    fn btree_assisted_select_still_respects_rls() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, owner TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING BTREE (id)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (1, 'alice')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (1, 'bob')")
            .unwrap();
        engine.commit(xid).unwrap();

        // P3.a: the durable B-Tree is always consistent with committed data —
        // no `Ready` wait — so the index-assisted path is exercised directly.
        let policy = crate::sql::logical::Expr::BinOp {
            op: crate::sql::logical::CmpOp::Eq,
            lhs: Box::new(crate::sql::logical::Expr::Column("owner".to_string())),
            rhs: Box::new(crate::sql::logical::Expr::Literal(
                crate::sql::logical::Literal::Text("alice".to_string()),
            )),
        };
        engine.set_rls_policy("t", policy).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine
            .execute_sql(xid2, "SELECT owner FROM t WHERE id = 1")
            .unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 1, "RLS must filter out bob's row: {rows:?}");
                assert_eq!(
                    rows[0][0],
                    crate::sql::logical::Literal::Text("alice".into())
                );
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // ── M3.a: graph edges ────────────────────────────────────────────────────

    #[test]
    fn edges_table_exists_and_is_ordinary_sql_queryable() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine
            .execute_sql(xid2, "SELECT * FROM __edges__ WHERE from_id = 1")
            .unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn create_edge_then_edges_from_returns_it() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let mut edges = engine.edges_from(xid2, 1).unwrap();
        edges.sort_by_key(|e| e.to_id);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].to_id, 2);
        assert_eq!(edges[0].edge_type, "KNOWS");
        assert_eq!(edges[1].to_id, 3);
    }

    #[test]
    fn delete_edge_removes_from_index_and_traversal() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let row_id = engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        engine.delete_edge(xid2, row_id, 1).unwrap();
        engine.commit(xid2).unwrap();

        let xid3 = engine.begin().unwrap();
        assert!(engine.edges_from(xid3, 1).unwrap().is_empty());
    }

    #[test]
    fn edges_from_on_from_id_with_no_edges_returns_empty() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        assert!(engine.edges_from(xid, 999).unwrap().is_empty());
    }

    #[test]
    fn edge_index_rebuilds_on_reopen() {
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
            engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }

        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let edges = engine2.edges_from(xid, 1).unwrap();
        assert_eq!(edges.len(), 2);
    }

    // ── M3.c: Cypher subset ──────────────────────────────────────────────────

    #[test]
    fn execute_cypher_match_where_return_uses_index_fast_path() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 99, 100, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                let mut to_ids: Vec<i64> = rows
                    .iter()
                    .map(|r| match &r[0] {
                        Literal::Int(n) => *n,
                        other => panic!("expected Int, got {other:?}"),
                    })
                    .collect();
                to_ids.sort();
                assert_eq!(to_ids, vec![2, 3]);
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_filters_by_edge_type() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:LIKES]->(b) WHERE a = 1 RETURN b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Literal::Int(3));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_without_from_id_predicate_falls_back_to_full_scan() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 5, 6, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        // No `a = ...` equality anywhere — `find_from_id_eq` finds nothing,
        // so this must go through the full-`__edges__`-scan fallback path,
        // not the index fast path, and still return every matching edge.
        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:KNOWS]->(b) RETURN a, b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 2),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_returns_edge_type_and_props() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .create_edge(xid, 1, 2, "KNOWS", "{\"since\":2020}")
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(
                xid2,
                "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b, type, props",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows[0][0], Literal::Int(2));
                assert_eq!(rows[0][1], Literal::Text("KNOWS".to_string()));
                assert_eq!(rows[0][2], Literal::Json("{\"since\":2020}".to_string()));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_rejects_property_access() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let err = engine
            .execute_cypher(
                xid,
                "MATCH (a)-[:KNOWS]->(b) WHERE a.name = 'alice' RETURN b",
            )
            .unwrap_err();
        assert!(matches!(err, DbError::SqlUnsupported(_)));
    }

    // ── M4.a: event capture foundation ──────────────────────────────────────

    fn events_for_table(engine: &mut Engine, table: &str) -> Vec<Vec<Literal>> {
        let xid = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid,
                &format!("SELECT * FROM __events__ WHERE table_name = '{table}'"),
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => rows.clone(),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn queue_tables_exist_and_are_ordinary_sql_queryable() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let events = engine.execute_sql(xid, "SELECT * FROM __events__").unwrap();
        assert_eq!(events, vec![SqlResult::Rows(vec![])]);
        let consumers = engine
            .execute_sql(xid, "SELECT * FROM __consumers__")
            .unwrap();
        assert_eq!(consumers, vec![SqlResult::Rows(vec![])]);
    }

    #[test]
    fn events_disabled_by_default_produces_no_event_rows() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'a')")
            .unwrap();
        engine.commit(xid).unwrap();

        assert!(events_for_table(&mut engine, "t").is_empty());
    }

    #[test]
    fn enable_events_rejects_system_tables() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        assert!(matches!(
            engine.enable_events(queue::EVENTS_TABLE),
            Err(DbError::SqlPlan(_))
        ));
        assert!(matches!(
            engine.enable_events(queue::CONSUMERS_TABLE),
            Err(DbError::SqlPlan(_))
        ));
    }

    #[test]
    fn insert_on_events_enabled_table_captures_one_tagged_event() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("insert".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload, got {:?}", rows[0][4]);
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["id"], serde_json::json!(1));
        assert_eq!(parsed["name"], serde_json::json!("alice"));
    }

    #[test]
    fn update_on_events_enabled_table_captures_new_value() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, balance INT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, balance) VALUES (1, 100)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "UPDATE t SET balance = 200 WHERE id = 1")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("update".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload");
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["balance"], serde_json::json!(200));
    }

    #[test]
    fn delete_on_events_enabled_table_captures_pre_delete_row() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "DELETE FROM t WHERE id = 1")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("delete".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload");
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["name"], serde_json::json!("alice"));
    }

    #[test]
    fn aborted_transaction_event_is_self_visible_then_invisible_to_fresh_txn() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let doomed = engine.begin().unwrap();
        engine
            .execute_sql(doomed, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        let self_view = engine
            .execute_sql(doomed, "SELECT * FROM __events__ WHERE table_name = 't'")
            .unwrap();
        match &self_view[0] {
            SqlResult::Rows(rows) => assert_eq!(
                rows.len(),
                1,
                "inserting transaction must see its own uncommitted event"
            ),
            other => panic!("expected Rows, got {other:?}"),
        }

        engine.abort(doomed).unwrap();

        assert!(
            events_for_table(&mut engine, "t").is_empty(),
            "aborted transaction's event leaked into a fresh transaction's view"
        );
    }

    #[test]
    fn event_seq_derivation_resumes_past_highest_seen_after_reopen() {
        let dir = tempdir().unwrap();
        {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            engine.commit(xid).unwrap();
            engine.enable_events("t").unwrap();

            let xid2 = engine.begin().unwrap();
            engine
                .execute_sql(xid2, "INSERT INTO t (id) VALUES (1)")
                .unwrap();
            engine
                .execute_sql(xid2, "INSERT INTO t (id) VALUES (2)")
                .unwrap();
            engine.commit(xid2).unwrap();
            engine.flush().unwrap();
        }

        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        engine2
            .execute_sql(xid, "INSERT INTO t (id) VALUES (3)")
            .unwrap();
        engine2.commit(xid).unwrap();

        let rows = events_for_table(&mut engine2, "t");
        let mut seqs: Vec<i64> = rows
            .iter()
            .map(|r| match r[0] {
                Literal::Int(n) => n,
                ref other => panic!("expected Int seq, got {other:?}"),
            })
            .collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2, 3], "seq must not reuse after reopen");
    }

    // ── M4.b: poll/ack, consumer offsets ────────────────────────────────────

    fn setup_events_table(engine: &mut Engine, n: i64) {
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        for i in 1..=n {
            engine
                .execute_sql(xid2, &format!("INSERT INTO t (id) VALUES ({i})"))
                .unwrap();
        }
        engine.commit(xid2).unwrap();
    }

    #[test]
    fn poll_events_does_not_advance_offset() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let first = engine.poll_events(xid, "c1", 10).unwrap();
        let second = engine.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert_eq!(first, second);
    }

    #[test]
    fn ack_events_advances_offset_so_next_poll_only_returns_newer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(batch.len(), 3);
        let up_to = batch[0].seq;
        engine.ack_events(xid, "c1", up_to).unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let remaining = engine.poll_events(xid2, "c1", 10).unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|e| e.seq > up_to));
    }

    #[test]
    fn consumer_offsets_persist_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            setup_events_table(&mut engine, 3);
            let xid = engine.begin().unwrap();
            let batch = engine.poll_events(xid, "c1", 10).unwrap();
            engine.ack_events(xid, "c1", batch[1].seq).unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }

        let engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let remaining = engine2.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn independent_consumers_do_not_interfere() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "c1", 10).unwrap();
        engine.ack_events(xid, "c1", batch[2].seq).unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let c1_remaining = engine.poll_events(xid2, "c1", 10).unwrap();
        let c2_remaining = engine.poll_events(xid2, "c2", 10).unwrap();
        assert!(c1_remaining.is_empty());
        assert_eq!(c2_remaining.len(), 3);
    }

    #[test]
    fn poll_events_for_unregistered_consumer_starts_at_offset_zero_without_writing() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 2);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "never-registered", 10).unwrap();
        assert_eq!(batch.len(), 2);

        let consumers = engine
            .execute_sql(xid, "SELECT * FROM __consumers__")
            .unwrap();
        match &consumers[0] {
            SqlResult::Rows(rows) => assert!(
                rows.is_empty(),
                "poll_events must not write a __consumers__ row"
            ),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // ── M10: heap vacuum / GC ────────────────────────────────────────────────

    /// The M10 analogue of `graph_mvcc.rs`'s "single most important test":
    /// reproduce the slot-reuse index-aliasing hazard with the index-clean
    /// pass disabled (a wrong-but-visible row surfaces), then prove the real
    /// `Engine::vacuum` (index-clean pass on) makes that wrong answer
    /// impossible.
    #[test]
    fn vacuum_index_aliasing_hazard_reproduced_then_fixed() {
        // (1) With the index-clean gate DISABLED, a reused slot aliases a
        // stale EdgeIndex entry left by an aborted create_edge.
        {
            let dir = tempdir().unwrap();
            let engine = Engine::open(dir.path(), 0).unwrap();
            let t1 = engine.begin().unwrap();
            let stale = engine.create_edge(t1, 100, 999, "T", "{}").unwrap();
            engine.abort(t1).unwrap(); // row dead; EdgeIndex[100]->stale lingers

            let report = engine.vacuum_inner(false).unwrap();
            assert!(
                report.versions_reclaimed >= 1,
                "the dead edge must be reclaimed: {report:?}"
            );

            let t2 = engine.begin().unwrap();
            let reused = engine.create_edge(t2, 200, 888, "T", "{}").unwrap();
            engine.commit(t2).unwrap();
            assert_eq!(reused, stale, "vacuum must have freed the slot for reuse");

            let q = engine.begin().unwrap();
            let wrong = engine.edges_from(q, 100).unwrap();
            assert!(
                !wrong.is_empty(),
                "hazard must reproduce: stale index entry aliases the reused live edge"
            );
        }

        // (2) The real Engine::vacuum scrubs the stale entry before the slot is
        // reusable — the wrong answer can no longer occur.
        {
            let dir = tempdir().unwrap();
            let engine = Engine::open(dir.path(), 0).unwrap();
            let t1 = engine.begin().unwrap();
            let stale = engine.create_edge(t1, 100, 999, "T", "{}").unwrap();
            engine.abort(t1).unwrap();

            engine.vacuum().unwrap();

            let t2 = engine.begin().unwrap();
            let reused = engine.create_edge(t2, 200, 888, "T", "{}").unwrap();
            engine.commit(t2).unwrap();
            assert_eq!(reused, stale, "slot is still reused");

            let q = engine.begin().unwrap();
            assert!(
                engine.edges_from(q, 100).unwrap().is_empty(),
                "index vacuum must scrub the stale entry"
            );
            assert_eq!(
                engine.edges_from(q, 200).unwrap().len(),
                1,
                "the genuine edge from 200 is intact"
            );
        }
    }

    #[test]
    fn vacuum_reclaims_dead_update_versions() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        let mut rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();
        for i in 0..20 {
            let x = engine.begin().unwrap();
            rid = engine.update(x, rid, format!("v{i}").as_bytes()).unwrap();
            engine.commit(x).unwrap();
        }
        let report = engine.vacuum().unwrap();
        assert!(
            report.versions_reclaimed >= 15,
            "dead update versions must be reclaimed: {report:?}"
        );
        assert!(report.bytes_reclaimed > 0);
        // The current version still reads correctly after compaction.
        let x = engine.begin().unwrap();
        assert_eq!(engine.get(x, rid).unwrap(), b"v19");
    }

    // ── A1: dead-tuple accounting ─────────────────────────────────────────────

    #[test]
    fn dead_tuple_estimate_tracks_raw_crud_and_resets_on_vacuum() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        assert_eq!(engine.dead_tuple_estimate(), 0);
        assert_eq!(engine.live_tuple_estimate(), 0);

        // INSERT bumps live only.
        let x = engine.begin().unwrap();
        let rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();
        assert_eq!(engine.live_tuple_estimate(), 1);
        assert_eq!(engine.dead_tuple_estimate(), 0);

        // Each UPDATE stamps one dead version; live unchanged.
        let mut rid = rid;
        for i in 0..5 {
            let x = engine.begin().unwrap();
            rid = engine.update(x, rid, format!("v{i}").as_bytes()).unwrap();
            engine.commit(x).unwrap();
        }
        assert_eq!(engine.dead_tuple_estimate(), 5);
        assert_eq!(engine.live_tuple_estimate(), 1);

        // DELETE: one more dead, live back to 0.
        let x = engine.begin().unwrap();
        engine.delete(x, rid).unwrap();
        engine.commit(x).unwrap();
        assert_eq!(engine.dead_tuple_estimate(), 6);
        assert_eq!(engine.live_tuple_estimate(), 0);

        // Vacuum reclaims the dead versions and refreshes both estimates.
        let report = engine.vacuum().unwrap();
        assert!(report.versions_reclaimed >= 6, "{report:?}");
        assert_eq!(engine.dead_tuple_estimate(), 0);
        assert_eq!(engine.live_tuple_estimate() as usize, report.rows_scanned);
    }

    #[test]
    fn autovacuum_policy_fires_at_the_postgres_threshold() {
        let cfg = AutoVacuumConfig {
            enabled: true,
            threshold: 50,
            scale_factor: 0.2,
            naptime: Duration::from_secs(60),
        };
        // trigger = 50 + 0.2 * 1000 = 250.
        assert!(!cfg.should_vacuum(250, 1000), "at the boundary: not yet");
        assert!(cfg.should_vacuum(251, 1000), "just over the boundary");
        assert!(!cfg.should_vacuum(49, 0), "below the flat threshold");
        assert!(cfg.should_vacuum(51, 0), "above the flat threshold");

        // Disabled ⇒ never fires, however much churn.
        let off = AutoVacuumConfig {
            enabled: false,
            ..cfg
        };
        assert!(!off.should_vacuum(1_000_000, 0));
    }

    #[test]
    fn autovacuum_should_run_reflects_live_estimates() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_autovacuum_config(AutoVacuumConfig {
            enabled: true,
            threshold: 5,
            scale_factor: 0.0,
            naptime: Duration::from_secs(60),
        });
        assert!(!engine.autovacuum_should_run(), "no churn yet");

        let x = engine.begin().unwrap();
        let mut rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();
        for i in 0..6 {
            let x = engine.begin().unwrap();
            rid = engine.update(x, rid, format!("v{i}").as_bytes()).unwrap();
            engine.commit(x).unwrap();
        }
        assert!(
            engine.autovacuum_should_run(),
            "6 dead > threshold 5: {} dead",
            engine.dead_tuple_estimate()
        );
        engine.vacuum().unwrap();
        assert!(
            !engine.autovacuum_should_run(),
            "vacuum reset the dead estimate"
        );
    }

    /// Poll `cond` up to `timeout`, sleeping briefly between checks. Returns
    /// whether it became true — for asserting an *asynchronous* background event.
    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn autovacuum_launcher_reclaims_without_a_manual_vacuum_call() {
        let dir = tempdir().unwrap();
        // Set an aggressive policy BEFORE spawning so the launcher's first nap
        // already uses the short naptime.
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_autovacuum_config(AutoVacuumConfig {
            enabled: true,
            threshold: 10,
            scale_factor: 0.0,
            naptime: Duration::from_millis(25),
        });
        let engine = Arc::new(engine);
        engine.spawn_autovacuum();
        assert!(engine.autovacuum_running());

        // Churn one row hard: 40 dead versions, no manual vacuum() anywhere.
        let x = engine.begin().unwrap();
        let mut rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();
        for i in 0..40 {
            let x = engine.begin().unwrap();
            rid = engine.update(x, rid, format!("v{i}").as_bytes()).unwrap();
            engine.commit(x).unwrap();
        }

        // The launcher must fire and drive the dead estimate back down on its
        // own — the defining autovacuum behaviour.
        let fired = wait_until(Duration::from_secs(5), || {
            engine.autovacuums_triggered() > 0 && engine.dead_tuple_estimate() <= 10
        });
        assert!(
            fired,
            "autovacuum did not reclaim: runs={}, dead={}",
            engine.autovacuums_triggered(),
            engine.dead_tuple_estimate()
        );

        // Data is intact after background reclamation.
        let x = engine.begin().unwrap();
        assert_eq!(engine.get(x, rid).unwrap(), b"v39");
    }

    #[test]
    fn autovacuum_launcher_shuts_down_cleanly_on_drop() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_autovacuum_config(AutoVacuumConfig {
            enabled: true,
            threshold: 5,
            scale_factor: 0.0,
            naptime: Duration::from_millis(25),
        });
        let engine = Arc::new(engine);
        engine.spawn_autovacuum();
        assert!(engine.autovacuum_running());

        // A Weak witness: after dropping the only strong Arc, the engine must be
        // freed — proving the worker holds no strong reference (no cycle leak).
        let witness = Arc::downgrade(&engine);
        assert!(witness.upgrade().is_some());
        drop(engine); // engine field-drop stops + joins the worker (bounded)

        assert!(
            wait_until(Duration::from_secs(5), || witness.upgrade().is_none()),
            "engine was not freed after drop — the launcher leaked a strong ref"
        );
    }

    #[test]
    fn autovacuum_respects_the_horizon_held_by_a_repeatable_read_reader() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_autovacuum_config(AutoVacuumConfig {
            enabled: true,
            threshold: 10,
            scale_factor: 0.0,
            naptime: Duration::from_millis(25),
        });
        let engine = Arc::new(engine);

        // Seed one row, then open a REPEATABLE READ transaction whose BEGIN-time
        // snapshot pins the vacuum horizon (M10.a / P5.c).
        let x = engine.begin().unwrap();
        let mut rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();
        let v0_rid = rid; // v0's version never moves; updates create new versions
        let reader = engine
            .begin_with_isolation(IsolationLevel::RepeatableRead)
            .unwrap();
        assert_eq!(engine.get(reader, v0_rid).unwrap(), b"v0"); // establish the snapshot

        // Churn hard AFTER the reader's snapshot: every dead version's xmax is
        // above the reader's xmin, so none is reclaimable while the reader lives.
        for i in 0..40 {
            let x = engine.begin().unwrap();
            rid = engine.update(x, rid, format!("v{i}").as_bytes()).unwrap();
            engine.commit(x).unwrap();
        }
        engine.spawn_autovacuum();

        // The launcher fires (dead=40 > threshold 10) but the horizon blocks it:
        // it runs yet reclaims nothing, so the dead estimate stays high.
        assert!(
            wait_until(Duration::from_secs(5), || engine.autovacuums_triggered()
                > 0),
            "autovacuum should still *run* while blocked"
        );
        std::thread::sleep(Duration::from_millis(100)); // let a few more passes run
        assert!(
            engine.dead_tuple_estimate() >= 40,
            "a live RR reader must block reclamation: dead={}",
            engine.dead_tuple_estimate()
        );
        // The reader still sees its snapshot — the versions it needs are intact.
        assert_eq!(engine.get(reader, v0_rid).unwrap(), b"v0");

        // Release the horizon: the reader commits. Now autovacuum can reclaim.
        engine.commit(reader).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || engine.dead_tuple_estimate()
                <= 10),
            "after the reader commits, autovacuum must reclaim: dead={}",
            engine.dead_tuple_estimate()
        );
    }

    #[test]
    fn disabled_policy_starts_no_launcher() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        engine.set_autovacuum_config(AutoVacuumConfig {
            enabled: false,
            ..AutoVacuumConfig::default()
        });
        let engine = Arc::new(engine);
        engine.spawn_autovacuum();
        assert!(
            !engine.autovacuum_running(),
            "disabled ⇒ no background thread"
        );
    }

    #[test]
    fn dead_tuple_estimate_tracks_sql_dml() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        for i in 0..10 {
            engine
                .execute_sql(x, &format!("INSERT INTO t VALUES ({i}, 0)"))
                .unwrap();
        }
        engine.commit(x).unwrap();
        assert_eq!(engine.live_tuple_estimate(), 10);
        assert_eq!(engine.dead_tuple_estimate(), 0);

        // UPDATE of 10 rows → 10 dead versions; live unchanged.
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "UPDATE t SET v = 1").unwrap();
        engine.commit(x).unwrap();
        assert_eq!(engine.dead_tuple_estimate(), 10);
        assert_eq!(engine.live_tuple_estimate(), 10);

        // DELETE of 4 rows → 4 more dead, live 10 → 6.
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "DELETE FROM t WHERE id < 4").unwrap();
        engine.commit(x).unwrap();
        assert_eq!(engine.dead_tuple_estimate(), 14);
        assert_eq!(engine.live_tuple_estimate(), 6);
    }

    #[test]
    fn vacuum_horizon_blocked_flag_tracks_open_transactions() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let open = engine.begin().unwrap();
        // Advance next_xid past `open`'s snapshot so it genuinely holds the
        // horizon below where a quiescent database would sit.
        let bump = engine.begin().unwrap();
        engine.commit(bump).unwrap();
        let report = engine.vacuum().unwrap();
        assert!(
            report.horizon_blocked,
            "an open txn must hold the horizon back"
        );
        engine.commit(open).unwrap();
        let report2 = engine.vacuum().unwrap();
        assert!(
            !report2.horizon_blocked,
            "a quiescent database must not report the horizon blocked"
        );
    }

    #[test]
    fn vacuum_does_not_reclaim_versions_a_live_reader_still_needs() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        let rid = engine.insert(x, b"v0").unwrap();
        engine.commit(x).unwrap();

        // A long-lived RR reader fixes its snapshot on v0.
        let reader = engine
            .begin_with_isolation(Isolation::RepeatableRead)
            .unwrap();
        assert_eq!(engine.get(reader, rid).unwrap(), b"v0");

        // A newer transaction supersedes v0 with v1 and commits.
        let w = engine.begin().unwrap();
        engine.update(w, rid, b"v1").unwrap();
        engine.commit(w).unwrap();

        // Vacuum must NOT reclaim v0 — `reader` still needs it.
        let report = engine.vacuum().unwrap();
        assert!(report.horizon_blocked);
        assert_eq!(
            engine.get(reader, rid).unwrap(),
            b"v0",
            "the reader's version must survive vacuum while it is live"
        );
        engine.commit(reader).unwrap();
    }

    #[test]
    fn vacuumed_database_survives_reopen() {
        let dir = tempdir().unwrap();
        let keep = {
            let engine = Engine::open(dir.path(), 0).unwrap();
            let x = engine.begin().unwrap();
            let keep = engine.insert(x, b"keep").unwrap();
            let drop_it = engine.insert(x, b"drop").unwrap();
            engine.commit(x).unwrap();
            let x2 = engine.begin().unwrap();
            engine.delete(x2, drop_it).unwrap();
            engine.commit(x2).unwrap();
            engine.vacuum().unwrap();
            engine.flush().unwrap();
            keep
        };
        // Reopen runs recovery (which must idempotently redo the vacuum).
        let engine = Engine::open(dir.path(), 0).unwrap();
        let x = engine.begin().unwrap();
        assert_eq!(engine.get(x, keep).unwrap(), b"keep");
    }

    /// C2 (D5 eviction-forced sync): under the commit-time-fsync default a
    /// single transaction can dirty more pages than the buffer pool holds
    /// without any statement fsync. Eviction must then force a WAL sync and
    /// steal a now-durable page rather than dead-ending at `BufferPoolFull`.
    /// A tiny pool + one large transaction inserting far more pages' worth of
    /// rows than the pool has frames must still commit and read back every row,
    /// with the D5 invariant (page LSN never ahead of the durable WAL at the
    /// steal point — a debug tripwire in `find_victim`) intact.
    #[test]
    fn large_deferred_transaction_survives_pool_smaller_than_working_set() {
        let dir = tempdir().unwrap();
        // Minimum pool (16 frames). Each row is ~1 KiB, so a page holds only a
        // handful — 400 rows dirties dozens of pages, many times the 16 frames.
        let engine = Engine::open_with_pool_capacity(dir.path(), 0, 16).unwrap();
        let payload = vec![0xABu8; 1024];

        let xid = engine.begin().unwrap();
        let mut rids = Vec::new();
        for i in 0..400u32 {
            // Distinguish rows by a small prefix so read-back is meaningful.
            let mut row = i.to_le_bytes().to_vec();
            row.extend_from_slice(&payload);
            // Every insert goes through `fetch_page_for_write`, which forces a
            // WAL sync + retry when the pool is full of not-yet-durable dirty
            // pages — this is the assertion under test: no `BufferPoolFull`.
            rids.push(engine.insert(xid, &row).unwrap());
        }
        engine.commit(xid).unwrap();

        // Every row is present and intact after eviction churn.
        let reader = engine.begin().unwrap();
        for (i, rid) in rids.iter().enumerate() {
            let got = engine.get(reader, *rid).unwrap();
            assert_eq!(&got[..4], &(i as u32).to_le_bytes(), "row {i} prefix");
            assert_eq!(&got[4..], &payload[..], "row {i} payload");
        }
        engine.commit(reader).unwrap();

        // And they survive a reopen (recovery redoes the committed inserts even
        // though most pages were evicted, not checkpoint-flushed).
        drop(engine);
        let engine = Engine::open_with_pool_capacity(dir.path(), 0, 16).unwrap();
        let reader = engine.begin().unwrap();
        let last = rids.len() - 1;
        let got = engine.get(reader, rids[last]).unwrap();
        assert_eq!(&got[..4], &(last as u32).to_le_bytes());
        engine.commit(reader).unwrap();
    }
}
