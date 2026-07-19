// Catalog (M1.c): table name -> schema + storage root.
//
// The catalog is persisted as a single serialized blob (JSON via serde,
// not the hand-rolled binary encoding the rest of the on-disk format uses —
// schema metadata is infrequent control-plane data, not the per-row page/WAL
// hot path D9's "no serde" rule is about) written to a fresh page on every
// change, with `control.catalog_root` pointing at the latest page and slot 0
// holding the blob by convention. Old catalog pages become garbage on
// rewrite — the same "no reclamation in M1" tradeoff already accepted for
// heap pages (see MEMORY.md).
//
// Catalog rows are NOT MVCC-versioned: DDL takes effect immediately and
// globally, visible to every transaction as soon as it returns, with no
// rollback if the surrounding transaction later aborts. This is a
// deliberate M1 simplification — full snapshot-isolated DDL would require
// every single catalog lookup during SQL execution to carry a snapshot and
// walk visibility, which is disproportionate to what M1.c needs to prove
// (that SQL works end-to-end). Real databases have historically shipped
// with much weaker DDL isolation guarantees than this for the same reason.
//
// Bootstrap: a fresh database has `control.catalog_root == INVALID_PAGE_ID`;
// `Catalog::load` returns an empty catalog in that case. The catalog's own
// page is read directly via `BufferPool`/`SlottedPage`, never through the
// SQL executor — there is no chicken-and-egg problem of needing a catalog
// entry to read the catalog.

use std::sync::Mutex;
use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    btree_index::DiskBTree,
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::{DbError, Result},
    format::{u32_from_le, u32_to_le, Lsn, PageId, INVALID_PAGE_ID, PAGE_TYPE_META},
    heap::encode_insert_redo,
    page::{SlottedPage, PAGE_HEADER_SIZE, SLOT_SIZE, TUPLE_HEADER_SIZE},
    sql::logical::{Expr, Literal},
    wal::Wal,
};

/// Magic prefix for chain-format catalog pages (4 bytes LE = 0xC0DA7A10).
/// The first LE byte is 0x10, which is not '{' (0x7B), so a page starting with
/// this magic is unambiguously a new-format chain page, not a legacy JSON blob.
const CATALOG_CHAIN_MAGIC: u32 = 0xC0DA_7A10;

/// Bytes used by the per-page chain header (magic u32 + next_page_id u32).
const CHAIN_HEADER_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnType {
    Int64,
    Text,
    Bool,
    /// Raw JSON text storage + `->`/`->>` path extraction (M1.c). No
    /// containment (`@>`), no binary JSONB encoding, no index — those are
    /// disproportionate until something actually indexes into JSON.
    Json,
    /// Fixed-dimension `f32` embedding (M2). `n` is the dimension, validated
    /// `> 0` at `CREATE TABLE` time; every inserted vector must match it
    /// exactly (checked in `sql/executor.rs::coerce_and_validate_row`).
    Vector(u32),
    /// Exact fixed-point decimal (P2.a): `Decimal(precision, scale)`. Stored
    /// as an `i128` scaled by `10^scale` — exact arithmetic, no float error.
    /// `precision` is the maximum number of significant decimal digits and
    /// `scale` the number of fractional digits (`scale <= precision`, both
    /// validated at `CREATE TABLE` time). The `i128` backing bounds the usable
    /// precision to ~38 digits; larger `NUMERIC` is out of scope (see the
    /// Phase 2 spec's "known limitations").
    Decimal(u8, u8),
    /// Timestamp (P2.a): microseconds since the Unix epoch, UTC, stored as an
    /// `i64` (8 bytes LE). `TIMESTAMPTZ` normalizes to UTC on input; v1 only
    /// stores UTC and does not track the original zone.
    Timestamp,
    /// IEEE-754 double (P2.b): `f64`, 8 bytes LE. `FLOAT`/`REAL`/`DOUBLE
    /// PRECISION` all map here — inexact by nature (use `Decimal` for money).
    Float,
    /// UUID (P2.b): 16 raw bytes. Accepts canonical hyphenated or 32-hex-digit
    /// text on input; renders canonical lowercase hyphenated on output.
    Uuid,
    /// Opaque binary blob (P2.b): variable-length bytes, same length-prefixed
    /// on-disk shape as `Text`/`Json`. Text input is hex (`\xDEADBEEF`) or the
    /// string's raw UTF-8 bytes.
    Bytea,
    /// Calendar date (P2.b): days since the Unix epoch, `i32`, 4 bytes LE.
    Date,
    /// Time of day (P2.b): microseconds since midnight, `i64`, 8 bytes LE. No
    /// zone.
    Time,
}

/// Which secondary index (if any) a column has. `None` by default — indexing
/// is always an explicit `CREATE INDEX` opt-in, never automatic, since
/// indexing every column by default would silently impose background-worker
/// overhead on tables that never query it (M2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    /// Vector similarity index, valid on `ColumnType::Vector(_)`. **Since P3.c
    /// (production) this denotes the durable on-disk IVF-Flat index**
    /// (`src/disk_vector.rs`) — the in-RAM HNSW graph and its async rebuild
    /// worker were retired. `CREATE INDEX ... USING HNSW`/`USING IVF` both build
    /// it; the `Hnsw` name is kept for on-disk catalog and SQL compatibility.
    Hnsw,
    /// Only valid on `ColumnType::Text`.
    FullText,
    /// Valid on `Int64`/`Text`/`Bool` — anything `Ord` (M6). Accelerates
    /// equality/range `WHERE` predicates; see `sql/executor.rs::exec_select`.
    BTree,
    /// Engine-managed only (M7), retired in P3.b — never set via
    /// `ColumnDef.index`/`CREATE INDEX`; there is no SQL keyword for it. Kept as
    /// a catalog-format variant so pre-P3.b blobs still deserialize; adjacency is
    /// now served by the durable edge `BTree`, not the CSR index.
    Csr,
}

/// Build status of a secondary index, surfaced by the REST server's
/// `GET /indexes/:table/:column/status`. **Since P3.c every secondary index is
/// durable and built synchronously** (B-Tree/full-text/edge as `DiskBTree`,
/// vector as the on-disk IVF-Flat index), so a live index is always `Ready` the
/// moment `CREATE INDEX` returns — there is no async backfill window anymore.
/// The `Building` variant is retained for wire/format compatibility but is no
/// longer produced. (Historically this lived in the now-removed async
/// `index_worker` module.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum IndexStatus {
    Building { rows_done: u64 },
    Ready,
}

/// A foreign-key reference recorded on a column (`REFERENCES table(column)`)
/// or, for the table-level form, inside [`ForeignKey`] (M11).
///
/// Enforcement as of item 36:
/// - **Child INSERT/UPDATE**: the referenced parent key is verified to exist
///   and be visible via the parent's `unique_index_root` DiskBTree (O(log n));
///   falls back to a heap scan for composite FKs or missing secondary index.
/// - **Parent DELETE/UPDATE (RESTRICT)**: rejected when a visible child row
///   references the key being removed; uses the child's secondary BTree index
///   when available, heap-scan fallback otherwise.
/// - **NULL FK values** are not checked (SQL standard).
/// - `ON DELETE CASCADE / SET NULL` is not yet implemented (RESTRICT only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyRef {
    /// Referenced table name.
    pub table: String,
    /// Referenced column — enforced via the parent's unique_index_root (item 36).
    pub column: Option<String>,
}

/// Column-level constraints (M11), grouped into one struct so adding them to
/// [`ColumnDef`] is a single new field rather than six — and so every
/// existing `ColumnDef { .. }` literal only needs `constraints:
/// Default::default()`. Every field is `#[serde(default)]` so catalog blobs
/// written before M11 deserialize unchanged (forward-compatible on-disk
/// format, same discipline as `TableDef.events_enabled` in M4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ColumnConstraints {
    /// `NOT NULL`. `primary_key` implies this too (see enforcement).
    #[serde(default)]
    pub not_null: bool,
    /// Column-level `UNIQUE`. `primary_key` implies this too.
    #[serde(default)]
    pub unique: bool,
    /// Column-level `PRIMARY KEY` (implies `NOT NULL` + `UNIQUE`).
    #[serde(default)]
    pub primary_key: bool,
    /// `DEFAULT <literal>` — filled in for a NULL/omitted value at INSERT
    /// time (never on UPDATE), before NOT NULL / CHECK / type coercion run.
    #[serde(default)]
    pub default: Option<Literal>,
    /// Column-level `CHECK (<expr>)`. Reuses the executor's predicate
    /// evaluator; violation only on a definite `false` (NULL/true pass, per
    /// SQL three-valued logic).
    #[serde(default)]
    pub check: Option<Expr>,
    /// Column-level `REFERENCES <table>(<column>)`.
    #[serde(default)]
    pub references: Option<ForeignKeyRef>,
    /// `SERIAL` / `GENERATED ... AS IDENTITY` (P2.d): the column auto-fills
    /// from the table's monotonic counter (`TableDef.serial_next`) when its
    /// value is omitted/NULL on INSERT. Only valid on `Int64`.
    #[serde(default)]
    pub identity: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub index: Option<IndexKind>,
    /// For a durable `BTree` index (P3.a): the stable meta page id of its
    /// on-disk B+tree. Set once at `CREATE INDEX`, never changes (a root split
    /// repoints the meta page in place, not this pointer), so `Engine::open`
    /// reconstructs the tree from it with no heap rescan. `None` for the
    /// still-rebuilt-on-open kinds (Hnsw/FullText/Csr) and un-indexed columns.
    #[serde(default)]
    pub index_root: Option<PageId>,
    /// Stable meta page id of the **implicit** unique-enforcement B-tree,
    /// auto-created at `CREATE TABLE` for every `PRIMARY KEY` or `UNIQUE`
    /// column (item 35). Distinct from `index_root` (the explicit secondary
    /// index) so a column can carry both without conflict. `None` for columns
    /// without PK/UNIQUE constraints, and for tables predating this feature
    /// (those fall back to the O(n) heap-scan path). `#[serde(default)]` so
    /// pre-item-35 catalog blobs deserialize cleanly with no FORMAT_VERSION bump.
    #[serde(default)]
    pub unique_index_root: Option<PageId>,
    #[serde(default)]
    pub constraints: ColumnConstraints,
    /// Logically dropped by `ALTER TABLE DROP COLUMN` (P2.c). A dropped column
    /// keeps its physical slot in the row layout (so rows written before the
    /// drop still decode positionally) but is invisible to `SELECT *`, cannot
    /// be referenced by name, and is always written as NULL on new inserts.
    /// This is the standard "tombstone" approach (cf. Postgres
    /// `pg_attribute.attisdropped`) — dropping a column never rewrites the heap.
    #[serde(default)]
    pub dropped: bool,
}

/// A table-level `FOREIGN KEY (cols) REFERENCES table(cols)` (M11). As with
/// [`ForeignKeyRef`], only referenced-table existence is enforced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// Table-level constraints (M11), grouped for the same reason as
/// [`ColumnConstraints`]. Column-level `PRIMARY KEY`/`UNIQUE`/`REFERENCES`
/// stay on the column; these carry the *table-level* forms
/// (`PRIMARY KEY (a, b)`, `UNIQUE (a, b)`, `FOREIGN KEY (...)`, table `CHECK`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TableConstraints {
    /// Table-level `PRIMARY KEY (cols)`. Its columns are also treated as
    /// `NOT NULL` (set at parse time) and, together, as a UNIQUE set.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Each entry is one `UNIQUE (cols)` column set.
    #[serde(default)]
    pub unique: Vec<Vec<String>>,
    /// Table-level `CHECK (<expr>)` expressions.
    #[serde(default)]
    pub checks: Vec<Expr>,
    /// Table-level foreign keys.
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// **Legacy** (pre-durable-FSM) in-catalog page list. Retained only so a
    /// catalog written before the durable FSM (`fsm_meta == None`) still opens
    /// and scans its existing pages — no data-dir migration (per the durable-FSM
    /// spec). Tables created since carry `fsm_meta` and leave this empty; it is
    /// never grown again (that O(heap-pages) blob rewrite was the `HeapFull`
    /// ceiling this milestone removes). `#[serde(default)]` so nothing else must
    /// set it.
    #[serde(default)]
    pub pages: Vec<PageId>,
    /// Stable meta-page id of this table's **durable free-space map** (a
    /// `DiskBTree` keyed `page_id -> free_bytes`; its keys are the page
    /// directory, replacing the legacy `pages` blob — see the durable-FSM spec).
    /// Minted once at `create_table`, then never changes (like `ColumnDef.
    /// index_root` / the edge & LOB index meta pages), so `Engine::open` stays
    /// O(1) — the heap opens the FSM from this id, never rescans. `None` only for
    /// a legacy catalog predating the FSM (falls back to `pages`).
    #[serde(default)]
    pub fsm_meta: Option<PageId>,
    /// RLS predicate for SELECT / UPDATE / DELETE: merged AND of all `FOR
    /// SELECT`, `FOR UPDATE`, `FOR DELETE`, and `FOR ALL` named policies, plus
    /// any direct `PUT /tables/{name}/rls` predicate. Applied by `apply_rls`
    /// in the logical planner.
    pub rls_policy: Option<Expr>,
    /// RLS predicate for INSERT: merged AND of all `FOR INSERT` and `FOR ALL`
    /// named policies. Evaluated row-by-row in `exec_insert` after coercion
    /// and before the heap write. `#[serde(default)]` so pre-Z1 catalog blobs
    /// deserialize with `None`.
    #[serde(default)]
    pub insert_policy: Option<Expr>,
    /// Named RLS policies (item-24 Z1). Each entry was created via
    /// `CREATE POLICY … ON <table> FOR <op> USING (…)`. Evaluated as an
    /// OR-of-permissive-policies per Postgres semantics: at plan time the
    /// engine ANDs the combined OR-result into the query's predicate, just
    /// like the existing single `rls_policy`. `#[serde(default)]` so
    /// pre-Z1 catalog blobs deserialize with an empty list.
    #[serde(default)]
    pub policies: Vec<crate::authz::PolicyDef>,
    /// Whether INSERT/UPDATE/DELETE on this table also durably capture a
    /// row in `__events__` (M4). `false` by default — event capture is
    /// always an explicit opt-in via `Engine::enable_events`, never
    /// automatic, mirroring M2's "indexing is always explicit" precedent.
    #[serde(default)]
    pub events_enabled: bool,
    /// Table-level constraints (M11). `#[serde(default)]` so pre-M11 catalog
    /// blobs deserialize with an empty set.
    #[serde(default)]
    pub constraints: TableConstraints,
    /// Next value to hand out for each `SERIAL`/identity column, keyed by
    /// column name (P2.d). Durable (persisted in the catalog blob, crash-safe
    /// via the same WAL-logged page write as any catalog change) and monotonic.
    #[serde(default)]
    pub serial_next: HashMap<String, i64>,
    /// Schema-shape generation counter (index-write-concurrency, Validation §5).
    /// Bumped by every DDL that changes this table's shape (column set, index
    /// attachment/root, TRUNCATE). A DML statement captures it when it clones the
    /// `TableDef` and `debug_assert!`s it is unchanged at write time: under the
    /// concurrent (`cat_read`) path the whole statement runs under a shared
    /// catalog lock, so no DDL can interleave and the counter must be stable —
    /// making this a cheap tripwire that turns a lock-discipline regression into a
    /// test/stress panic instead of a silent stale-schema write. `#[serde(default)]`
    /// so pre-existing catalog blobs deserialize with 0.
    #[serde(default)]
    pub generation: u64,
    /// Exact count of committed rows. Maintained on INSERT commit (+N) and
    /// DELETE commit (-N). Initialized to 0 at CREATE TABLE. Reset to 0 by
    /// TRUNCATE. Updated atomically under the catalog page latch. See item 97.
    /// `#[serde(default)]` so pre-v11 catalog blobs deserialise with 0 (treated
    /// as stale; recalibrated on the next DML commit that calls `persist_only`).
    #[serde(default)]
    pub row_count: i64,
}

/// Everything `Catalog` needs to durably persist itself, bundled so
/// mutating methods don't balloon into a long parameter list.
pub struct CatalogCtx<'a> {
    pub pool: &'a BufferPool,
    pub wal: &'a Wal,
    pub control_path: &'a Path,
    pub control: &'a Mutex<ControlData>,
    pub page_size: usize,
}

pub struct Catalog {
    tables: HashMap<String, TableDef>,
    /// `ANALYZE`-computed per-table statistics (P4.d). Kept in a side map
    /// rather than on [`TableDef`] so adding it touched only this file — no
    /// storage-core or other-lane `TableDef` constructor needed a new field.
    stats: HashMap<String, crate::sql::statistics::TableStats>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned catalog blob for deserialization (P4.d format: `{tables, stats}`).
#[derive(serde::Deserialize)]
struct PersistedCatalog {
    #[serde(default)]
    tables: HashMap<String, TableDef>,
    #[serde(default)]
    stats: HashMap<String, crate::sql::statistics::TableStats>,
}

/// Borrowed catalog blob for serialization.
#[derive(serde::Serialize)]
struct PersistedCatalogRef<'a> {
    tables: &'a HashMap<String, TableDef>,
    stats: &'a HashMap<String, crate::sql::statistics::TableStats>,
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            stats: HashMap::new(),
        }
    }

    /// Load the catalog from `control.catalog_root`, or return an empty
    /// catalog if this is a fresh database.
    ///
    /// Supports two on-disk formats:
    /// - **Chain format** (item 25): slot 0 starts with `CATALOG_CHAIN_MAGIC`
    ///   (4 bytes LE), followed by `next_page_id` (4 bytes LE), then a JSON
    ///   chunk. Multiple pages are followed until `next_page_id ==
    ///   INVALID_PAGE_ID`, then the chunks are concatenated and parsed.
    /// - **Legacy format**: slot 0 is a raw JSON blob (no magic prefix). This
    ///   covers both the pre-P4.d bare `{name: TableDef}` map and the P4.d
    ///   `{tables, stats}` wrapper. Both are read unchanged.
    pub fn load(control: &ControlData, pool: &BufferPool) -> Result<Self> {
        if control.catalog_root == INVALID_PAGE_ID {
            return Ok(Self::new());
        }
        let page = pool.fetch_page(control.catalog_root)?;
        let first_payload = page.get(0)?.to_vec();
        pool.unpin(control.catalog_root);

        // Detect chain format: first 4 bytes must equal CATALOG_CHAIN_MAGIC.
        if first_payload.len() >= CHAIN_HEADER_SIZE {
            let magic = u32_from_le(first_payload[0..4].try_into().unwrap());
            if magic == CATALOG_CHAIN_MAGIC {
                let json = Self::collect_chain(first_payload, pool)?;
                return Self::parse_blob(&json);
            }
        }

        // Legacy single-page format (pre-item-25 blobs open unchanged).
        Self::parse_blob(&first_payload)
    }

    /// Reassemble the JSON from a page chain, given the first page's payload.
    fn collect_chain(first_payload: Vec<u8>, pool: &BufferPool) -> Result<Vec<u8>> {
        let mut json = Vec::new();
        let mut payload = first_payload;
        loop {
            if payload.len() < CHAIN_HEADER_SIZE {
                return Err(DbError::CatalogCorrupt(
                    "catalog chain page too short to hold header".into(),
                ));
            }
            let magic = u32_from_le(payload[0..4].try_into().unwrap());
            if magic != CATALOG_CHAIN_MAGIC {
                return Err(DbError::CatalogCorrupt(
                    "catalog chain page has wrong magic".into(),
                ));
            }
            let next_page_id = u32_from_le(payload[4..8].try_into().unwrap());
            json.extend_from_slice(&payload[CHAIN_HEADER_SIZE..]);
            if next_page_id == INVALID_PAGE_ID {
                break;
            }
            let page = pool.fetch_page(next_page_id)?;
            payload = page.get(0)?.to_vec();
            pool.unpin(next_page_id);
        }
        Ok(json)
    }

    /// Parse a complete JSON blob as a `PersistedCatalog`, with backward
    /// compatibility for the pre-P4.d bare `{name: TableDef}` map format.
    fn parse_blob(blob: &[u8]) -> Result<Self> {
        // Backward compatible: a pre-P4.d catalog is a bare `{name: TableDef}`
        // map; the P4.d format wraps it as `{tables, stats}`. Try the old shape
        // first (it fails to parse the new one), then the new one.
        if let Ok(tables) = serde_json::from_slice::<HashMap<String, TableDef>>(blob) {
            return Ok(Self {
                tables,
                stats: HashMap::new(),
            });
        }
        let p: PersistedCatalog =
            serde_json::from_slice(blob).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;
        Ok(Self {
            tables: p.tables,
            stats: p.stats,
        })
    }

    pub fn lookup(&self, name: &str) -> Result<&TableDef> {
        self.tables
            .get(name)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))
    }

    /// All tables, in no particular order — used by the M2 index-rebuild
    /// rescan to find every indexed column across the whole catalog.
    pub fn tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.values()
    }

    /// Mutable access to the tables map — used only by the policy-recompute
    /// path in `Engine::drop_policy` which needs to clear `rls_policy` and
    /// then call `persist_only`. Do not use this for new code; prefer dedicated
    /// mutators.
    pub fn tables_mut(&mut self) -> &mut std::collections::HashMap<String, TableDef> {
        &mut self.tables
    }

    /// Persist the current catalog blob without any other mutation. Used when
    /// `Engine::drop_policy` has already mutated a `TableDef` in place via
    /// `tables_mut`.
    ///
    /// Returns the WAL commit LSN of the catalog mini-txn so callers that need
    /// to advance `durable_lsn` (e.g. `Engine::commit` in item 97) can call
    /// `wal.sync_up_to(lsn)` after this returns.
    pub fn persist_only(&mut self, ctx: &mut CatalogCtx) -> Result<Lsn> {
        self.persist(ctx)
    }

    /// Apply a signed `delta` to `table.row_count` and durably persist the
    /// catalog. Called inside a mini-txn at user-transaction commit time (item
    /// 97). Saturating arithmetic guards against wrap-around on absurd inputs.
    pub fn update_row_count(
        &mut self,
        table: &str,
        delta: i64,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.row_count = t.row_count.saturating_add(delta);
        self.persist(ctx).map(|_| ())
    }

    pub fn create_table(&mut self, mut def: TableDef, ctx: &mut CatalogCtx) -> Result<()> {
        if self.tables.contains_key(&def.name) {
            return Err(DbError::TableAlreadyExists(def.name));
        }
        // Mint this table's durable free-space map up front (durable-FSM spec):
        // a fresh `DiskBTree` whose stable meta page id becomes the table's O(1)
        // page-directory handle. Every table born since the FSM landed carries
        // one, so its heap never needs the legacy O(heap-pages) `pages` blob.
        if def.fsm_meta.is_none() {
            let fsm = DiskBTree::create(ctx.pool, ctx.wal)?;
            def.fsm_meta = Some(fsm.meta_page());
        }
        self.tables.insert(def.name.clone(), def);
        self.persist(ctx).map(|_| ())
    }

    pub fn set_rls_policy(
        &mut self,
        table: &str,
        policy: Expr,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.rls_policy = Some(policy);
        self.persist(ctx).map(|_| ())
    }

    /// Set the INSERT-only policy predicate (item-24 Z1: `CREATE POLICY … FOR INSERT`).
    pub fn set_insert_policy(
        &mut self,
        table: &str,
        policy: Option<Expr>,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.insert_policy = policy;
        self.persist(ctx).map(|_| ())
    }

    /// Append a named RLS policy (item-24 Z1: `CREATE POLICY`). Rejects a
    /// duplicate policy name on the same table (case-sensitive).
    pub fn add_policy(
        &mut self,
        policy: crate::authz::PolicyDef,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(&policy.table)
            .ok_or_else(|| DbError::TableNotFound(policy.table.clone()))?;
        if t.policies.iter().any(|p| p.name == policy.name) {
            return Err(DbError::Authz(format!(
                "policy '{}' already exists on table '{}'",
                policy.name, policy.table
            )));
        }
        t.policies.push(policy);
        self.persist(ctx).map(|_| ())
    }

    /// Remove a named RLS policy (item-24 Z1: `DROP POLICY`).
    pub fn remove_policy(&mut self, name: &str, table: &str, ctx: &mut CatalogCtx) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let before = t.policies.len();
        t.policies.retain(|p| p.name != name);
        if t.policies.len() == before {
            return Err(DbError::Authz(format!(
                "policy '{name}' not found on table '{table}'"
            )));
        }
        self.persist(ctx).map(|_| ())
    }

    /// Enable or disable event capture on a table (M4). `Engine::
    /// enable_events` is the validated entry point (rejects `__events__`/
    /// `__consumers__` themselves); this is just the catalog-persistence
    /// primitive, mirroring `set_rls_policy`'s exact shape.
    pub fn set_events_enabled(
        &mut self,
        table: &str,
        enabled: bool,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.events_enabled = enabled;
        self.persist(ctx).map(|_| ())
    }

    /// Attach (or clear) a secondary-index kind on one column. `CREATE
    /// INDEX`'s SQL surface lands in M2.c and will call this same method
    /// after its own type-compatibility validation — this is just the
    /// catalog-persistence primitive, reused rather than duplicated.
    pub fn set_column_index(
        &mut self,
        table: &str,
        column: &str,
        kind: Option<IndexKind>,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let col = t
            .columns
            .iter_mut()
            .find(|c| c.name == column)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        col.index = kind;
        t.generation = t.generation.wrapping_add(1);
        self.persist(ctx).map(|_| ())
    }

    /// Record the stable meta page id of a column's durable B-Tree (P3.a). Set
    /// once at `CREATE INDEX` and persisted in the catalog blob so
    /// `Engine::open` can reconstruct the tree with no heap rescan.
    pub fn set_column_index_root(
        &mut self,
        table: &str,
        column: &str,
        index_root: Option<PageId>,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let col = t
            .columns
            .iter_mut()
            .find(|c| c.name == column)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        col.index_root = index_root;
        t.generation = t.generation.wrapping_add(1);
        self.persist(ctx).map(|_| ())
    }

    /// Record the stable meta page id of a column's implicit unique-enforcement
    /// B-tree (item 35). Set once at `create_table` for each PK/UNIQUE column
    /// whose type is indexable; `Engine::open` opens the tree from this id with
    /// no heap rescan.
    pub fn set_column_unique_index_root(
        &mut self,
        table: &str,
        column: &str,
        unique_index_root: Option<PageId>,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let col = t
            .columns
            .iter_mut()
            .find(|c| c.name == column)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        col.unique_index_root = unique_index_root;
        t.generation = t.generation.wrapping_add(1);
        self.persist(ctx).map(|_| ())
    }

    /// Update a table's stored page list (called by the executor after an
    /// INSERT/UPDATE allocates a new heap page for that table's data).
    pub fn set_pages(
        &mut self,
        table: &str,
        pages: Vec<PageId>,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.pages = pages;
        self.persist(ctx).map(|_| ())
    }

    /// Store `ANALYZE`-computed statistics for a table and durably persist the
    /// catalog (P4.d). Stats ride the catalog's own WAL-logged page write, so
    /// they survive reopen without recomputation.
    pub fn set_table_stats(
        &mut self,
        table: &str,
        stats: crate::sql::statistics::TableStats,
        ctx: &mut CatalogCtx,
    ) -> Result<()> {
        if !self.tables.contains_key(table) {
            return Err(DbError::TableNotFound(table.to_string()));
        }
        self.stats.insert(table.to_string(), stats);
        self.persist(ctx).map(|_| ())
    }

    /// The stored statistics for a table, if it has been `ANALYZE`d (P4.d).
    pub fn table_stats(&self, table: &str) -> Option<&crate::sql::statistics::TableStats> {
        self.stats.get(table)
    }

    /// Allocate and durably persist the next value for a `SERIAL`/identity
    /// column (P2.d). Monotonic; the counter starts at 1. Persisting on every
    /// allocation keeps the sequence crash-safe (it survives reopen at the
    /// last-handed-out value) at the cost of a catalog rewrite per serial
    /// INSERT — acceptable for v1 correctness; batching is a later optimization.
    pub fn alloc_serial(&mut self, table: &str, column: &str, ctx: &mut CatalogCtx) -> Result<i64> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let next = t.serial_next.entry(column.to_string()).or_insert(1);
        let value = *next;
        *next = value.checked_add(1).ok_or_else(|| {
            DbError::SqlPlan(format!("sequence for '{table}.{column}' exhausted i64"))
        })?;
        self.persist(ctx)?;
        Ok(value)
    }

    /// `ALTER TABLE ADD COLUMN` (P2.c). The new column is appended physically
    /// (so existing rows still decode positionally — they simply lack bytes for
    /// it, and `decode_row` fills the DEFAULT/NULL). A `NOT NULL` column with no
    /// `DEFAULT` is rejected: old rows can't satisfy it without a backfill,
    /// which this tombstone-based scheme deliberately does not do.
    pub fn add_column(&mut self, table: &str, col: ColumnDef, ctx: &mut CatalogCtx) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        if t.columns.iter().any(|c| !c.dropped && c.name == col.name) {
            return Err(DbError::SqlPlan(format!(
                "column '{}' already exists on table '{table}'",
                col.name
            )));
        }
        if (col.constraints.not_null || col.constraints.primary_key)
            && col.constraints.default.is_none()
        {
            return Err(DbError::SqlPlan(format!(
                "ADD COLUMN '{}' is NOT NULL but has no DEFAULT (existing rows would violate it)",
                col.name
            )));
        }
        t.columns.push(col);
        t.generation = t.generation.wrapping_add(1);
        self.persist(ctx).map(|_| ())
    }

    /// `ALTER TABLE DROP COLUMN` (P2.c). Logical tombstone: the column keeps its
    /// physical slot (so rows written before the drop still decode) but is
    /// marked `dropped`, its constraints/index cleared. Dropping a column that
    /// participates in a *table-level* constraint is rejected (drop the
    /// constraint first); dropping the last visible column is rejected.
    pub fn drop_column(&mut self, table: &str, column: &str, ctx: &mut CatalogCtx) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        let referenced = t.constraints.primary_key.iter().any(|c| c == column)
            || t.constraints.unique.iter().flatten().any(|c| c == column)
            || t.constraints
                .foreign_keys
                .iter()
                .any(|fk| fk.columns.iter().any(|c| c == column));
        if referenced {
            return Err(DbError::SqlPlan(format!(
                "cannot drop column '{column}': it participates in a table-level constraint"
            )));
        }
        if t.columns.iter().filter(|c| !c.dropped).count() <= 1 {
            return Err(DbError::SqlPlan(
                "cannot drop the table's only remaining column".into(),
            ));
        }
        let col = t
            .columns
            .iter_mut()
            .find(|c| !c.dropped && c.name == column)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        col.dropped = true;
        col.index = None;
        col.constraints = ColumnConstraints::default();
        t.generation = t.generation.wrapping_add(1);
        self.persist(ctx).map(|_| ())
    }

    /// `DROP TABLE` (P2.c). Removes the table from the catalog. Its heap pages
    /// are orphaned (reclaimed once Phase 1's free-space map / free-page list
    /// lands — until then, dropped-table space is not reused, same accepted
    /// tradeoff as pre-vacuum heap bloat).
    pub fn drop_table(&mut self, table: &str, ctx: &mut CatalogCtx) -> Result<()> {
        if self.tables.remove(table).is_none() {
            return Err(DbError::TableNotFound(table.to_string()));
        }
        self.stats.remove(table);
        self.persist(ctx).map(|_| ())
    }

    /// `TRUNCATE` (P2.c). Drops every row by clearing the table's page list; the
    /// orphaned pages are reclaimed once Phase 1's FSM lands (as with
    /// `DROP TABLE`). The schema (columns/constraints) is unchanged.
    pub fn truncate(&mut self, table: &str, ctx: &mut CatalogCtx) -> Result<()> {
        // An FSM-backed table empties by adopting a fresh, empty free-space map
        // (the old tree's pages are orphaned, same accepted tradeoff as DROP);
        // its keys were the page directory, so a new tree = zero pages. Legacy
        // tables still clear the in-catalog `pages` list. Mint outside the
        // borrow of `t` (needs `ctx.pool`/`ctx.wal`).
        let fresh_fsm = {
            let t = self
                .tables
                .get(table)
                .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
            if t.fsm_meta.is_some() {
                Some(DiskBTree::create(ctx.pool, ctx.wal)?.meta_page())
            } else {
                None
            }
        };
        let t = self.tables.get_mut(table).expect("checked above");
        t.pages.clear();
        if let Some(meta) = fresh_fsm {
            t.fsm_meta = Some(meta);
        }
        t.generation = t.generation.wrapping_add(1);
        t.row_count = 0; // item 97: TRUNCATE resets exact count
                         // Row set is now empty; previously gathered stats are stale.
        self.stats.remove(table);
        self.persist(ctx).map(|_| ())
    }

    fn persist(&self, ctx: &mut CatalogCtx) -> Result<Lsn> {
        let blob = PersistedCatalogRef {
            tables: &self.tables,
            stats: &self.stats,
        };
        let encoded =
            serde_json::to_vec(&blob).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;

        // Maximum JSON bytes per catalog page. Each page's slot-0 payload is:
        //   [CHAIN_HEADER_SIZE bytes chain header][json chunk bytes]
        // The SlottedPage overhead per slot is PAGE_HEADER_SIZE + SLOT_SIZE +
        // TUPLE_HEADER_SIZE, so the maximum payload (chain header + json chunk)
        // is page_size minus those three. Subtracting CHAIN_HEADER_SIZE gives
        // the max JSON chunk per page.
        let max_payload = ctx
            .page_size
            .saturating_sub(PAGE_HEADER_SIZE + SLOT_SIZE + TUPLE_HEADER_SIZE);
        let max_json = max_payload.saturating_sub(CHAIN_HEADER_SIZE);

        // Collect chunk slices (may be just one for small catalogs).
        let chunks: Vec<&[u8]> = if max_json == 0 {
            // Pathological tiny page size — keep old behavior.
            vec![encoded.as_slice()]
        } else {
            encoded.chunks(max_json).collect()
        };

        // Allocate all page IDs up front so we can embed forward next_page_id
        // pointers in each page before writing any of them.
        let page_ids: Vec<PageId> = (0..chunks.len())
            .map(|_| ctx.pool.alloc_page())
            .collect::<Result<_>>()?;

        // WAL-log all chain pages in one mini-txn, then flip catalog_root.
        // Crash before the control-file flip leaves old catalog_root intact;
        // crash after = new chain is fully WAL-recovered. (Landmine 2 decision.)
        let (txn_id, begin_lsn) = ctx.wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;
        for (i, (chunk, &page_id)) in chunks.iter().zip(page_ids.iter()).enumerate() {
            let next_page_id = if i + 1 < page_ids.len() {
                page_ids[i + 1]
            } else {
                INVALID_PAGE_ID
            };
            // Build per-page payload: chain header + JSON chunk.
            let mut payload = Vec::with_capacity(CHAIN_HEADER_SIZE + chunk.len());
            payload.extend_from_slice(&u32_to_le(CATALOG_CHAIN_MAGIC));
            payload.extend_from_slice(&u32_to_le(next_page_id));
            payload.extend_from_slice(chunk);

            let mut page = SlottedPage::new(page_id, PAGE_TYPE_META, ctx.page_size);
            let slot = page.insert(&payload)?;
            debug_assert_eq!(
                slot, 0,
                "catalog chain page must hold exactly one blob at slot 0"
            );
            let redo = encode_insert_redo(0, None, &payload);
            let lsn = ctx.wal.log_insert(txn_id, prev_lsn, page_id, slot, &redo)?;
            page.set_lsn(lsn);
            ctx.pool.write_page(&page)?;
            prev_lsn = lsn;
        }
        let commit_lsn = ctx.wal.commit_mini_txn(txn_id, prev_lsn)?;

        // Atomic commit point: flip catalog_root to the head of the new chain.
        {
            let mut control = ctx.control.lock().unwrap_or_else(|e| e.into_inner());
            control.catalog_root = page_ids[0];
            control::write(ctx.control_path, &control)?;
        }
        Ok(commit_lsn)
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, def: TableDef) {
        self.tables.insert(def.name.clone(), def);
    }

    #[cfg(test)]
    pub fn insert_stats_for_test(
        &mut self,
        table: &str,
        stats: crate::sql::statistics::TableStats,
    ) {
        self.stats.insert(table.to_string(), stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::DEFAULT_PAGE_SIZE;
    use tempfile::tempdir;

    fn setup(dir: &std::path::Path) -> (BufferPool, Wal, std::path::PathBuf, Mutex<ControlData>) {
        let control_path = dir.join("control");
        let control = control::create(&control_path, DEFAULT_PAGE_SIZE).unwrap();
        let pool = BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&dir.join("db.wal"), crate::format::INVALID_LSN).unwrap();
        (pool, wal, control_path, Mutex::new(control))
    }

    #[test]
    fn fresh_database_has_empty_catalog() {
        let dir = tempdir().unwrap();
        let (pool, _wal, _cp, control) = setup(dir.path());
        let catalog = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert!(catalog.lookup("t").is_err());
    }

    #[test]
    fn create_table_then_lookup() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "accounts".to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Int64,
            }],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        };
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        catalog.create_table(def, &mut ctx).unwrap();
        let looked_up = catalog.lookup("accounts").unwrap();
        assert_eq!(looked_up.columns.len(), 1);
    }

    #[test]
    fn duplicate_create_table_is_rejected() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = || TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        };
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        catalog.create_table(def(), &mut ctx).unwrap();
        let err = catalog.create_table(def(), &mut ctx);
        assert!(matches!(err, Err(DbError::TableAlreadyExists(_))));
    }

    #[test]
    fn catalog_survives_reload() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "widgets".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    index: None,
                    index_root: None,
                    unique_index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                    ty: ColumnType::Int64,
                },
                ColumnDef {
                    name: "data".to_string(),
                    index: None,
                    index_root: None,
                    unique_index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                    ty: ColumnType::Json,
                },
            ],
            pages: vec![7],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        };
        {
            let mut ctx = CatalogCtx {
                pool: &pool,
                wal: &wal,
                control_path: &cp,
                control: &control,
                page_size: DEFAULT_PAGE_SIZE as usize,
            };
            catalog.create_table(def, &mut ctx).unwrap();
        }

        // Reload from the persisted control-file pointer.
        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        let t = reloaded.lookup("widgets").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.pages, vec![7]);
    }

    #[test]
    fn vector_column_and_index_kind_survive_reload() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "embeddings".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    ty: ColumnType::Int64,
                    index: None,
                    index_root: None,
                    unique_index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                },
                ColumnDef {
                    name: "vec".to_string(),
                    ty: ColumnType::Vector(384),
                    index: Some(IndexKind::Hnsw),
                    index_root: None,
                    unique_index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                },
            ],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        };
        {
            let mut ctx = CatalogCtx {
                pool: &pool,
                wal: &wal,
                control_path: &cp,
                control: &control,
                page_size: DEFAULT_PAGE_SIZE as usize,
            };
            catalog.create_table(def, &mut ctx).unwrap();
        }

        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        let t = reloaded.lookup("embeddings").unwrap();
        assert_eq!(t.columns[1].ty, ColumnType::Vector(384));
        assert_eq!(t.columns[1].index, Some(IndexKind::Hnsw));
    }

    #[test]
    fn set_rls_policy_persists() {
        use crate::sql::logical::{CmpOp, Literal};
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        };
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::Literal(Literal::Text("alice".to_string()))),
        };
        {
            let mut ctx = CatalogCtx {
                pool: &pool,
                wal: &wal,
                control_path: &cp,
                control: &control,
                page_size: DEFAULT_PAGE_SIZE as usize,
            };
            catalog.create_table(def, &mut ctx).unwrap();
            catalog
                .set_rls_policy("t", policy.clone(), &mut ctx)
                .unwrap();
        }

        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert_eq!(reloaded.lookup("t").unwrap().rls_policy, Some(policy));
    }

    // ── item 25: multi-page catalog ──────────────────────────────────────────

    /// Helper: build a TableDef with `ncols` columns named col0..colN.
    fn wide_table(name: &str, ncols: usize) -> TableDef {
        let columns = (0..ncols)
            .map(|i| ColumnDef {
                name: format!("col{i}"),
                ty: ColumnType::Text,
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
            })
            .collect();
        TableDef {
            name: name.to_string(),
            columns,
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
        }
    }

    /// A catalog with enough tables/columns to overflow one 8 KiB page must
    /// persist and reload intact without HeapFull.
    #[test]
    fn multipage_catalog_roundtrip() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        // 50 tables × 20 columns each → blob well past 8 KiB.
        for i in 0..50 {
            catalog
                .create_table(wide_table(&format!("t{i}"), 20), &mut ctx)
                .unwrap();
        }
        // Reload and verify all tables survive.
        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        for i in 0..50 {
            let t = reloaded.lookup(&format!("t{i}")).unwrap();
            assert_eq!(t.columns.len(), 20, "table t{i} should have 20 columns");
        }
    }

    /// A catalog whose serialized JSON just barely exceeds one page triggers
    /// a two-page chain and reloads correctly.
    #[test]
    fn catalog_just_over_page_boundary() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        // 30 tables × 15 columns → ~9–11 KiB, forces a two-page chain.
        for i in 0..30 {
            catalog
                .create_table(wide_table(&format!("u{i}"), 15), &mut ctx)
                .unwrap();
        }
        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert_eq!(reloaded.tables().count(), 30);
        for i in 0..30 {
            assert!(reloaded.lookup(&format!("u{i}")).is_ok());
        }
    }

    /// Legacy single-page catalog blob (no chain header) still opens unchanged.
    /// We write a raw JSON blob directly to a page (bypassing the new persist
    /// path) and confirm that load() falls back to the legacy parser.
    #[test]
    fn legacy_single_page_catalog_backward_compat() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());

        // Build a legacy-style encoded blob (no CATALOG_CHAIN_MAGIC prefix).
        let legacy_tables: HashMap<String, TableDef> = {
            let mut m = HashMap::new();
            m.insert(
                "legacy".to_string(),
                TableDef {
                    name: "legacy".to_string(),
                    columns: vec![ColumnDef {
                        name: "id".to_string(),
                        ty: ColumnType::Int64,
                        index: None,
                        index_root: None,
                        unique_index_root: None,
                        dropped: false,
                        constraints: Default::default(),
                    }],
                    pages: vec![],
                    fsm_meta: None,
                    rls_policy: None,
                    insert_policy: None,
                    policies: vec![],
                    events_enabled: false,
                    serial_next: Default::default(),
                    constraints: Default::default(),
                    generation: 0,
                    row_count: 0,
                },
            );
            m
        };
        // P4.d `{tables, stats}` shape (legacy format without chain header)
        #[derive(serde::Serialize)]
        struct LegacyBlob<'a> {
            tables: &'a HashMap<String, TableDef>,
            stats: HashMap<String, ()>,
        }
        let blob = serde_json::to_vec(&LegacyBlob {
            tables: &legacy_tables,
            stats: HashMap::new(),
        })
        .unwrap();

        // Write legacy blob directly to a page (old persist path).
        let page_id = pool.alloc_page().unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        let mut page = SlottedPage::new(page_id, PAGE_TYPE_META, DEFAULT_PAGE_SIZE as usize);
        let slot = page.insert(&blob).unwrap();
        let redo = encode_insert_redo(0, None, &blob);
        let lsn = wal
            .log_insert(txn_id, begin_lsn, page_id, slot, &redo)
            .unwrap();
        page.set_lsn(lsn);
        pool.write_page(&page).unwrap();
        wal.commit_mini_txn(txn_id, lsn).unwrap();
        {
            let mut ctrl = control.lock().unwrap();
            ctrl.catalog_root = page_id;
            control::write(&cp, &ctrl).unwrap();
        }

        // Reload via the new Catalog::load — must fall back to legacy path.
        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert!(reloaded.lookup("legacy").is_ok());
        assert_eq!(reloaded.lookup("legacy").unwrap().columns.len(), 1);
    }

    /// The item-23 original layout (objects with storage_key + full 8-col DLQ)
    /// that previously hit HeapFull must now succeed.
    #[test]
    fn item23_original_schema_no_heap_full() {
        let dir = tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        // buckets(3 cols) + objects(11 cols incl. storage_key) + 8-col DLQ
        // These are exactly the tables that overflowed before item 25.
        let buckets = wide_table("buckets", 3);
        let objects = wide_table("objects", 11); // includes storage_key
        let dlq = wide_table("object_dlq", 8);
        catalog.create_table(buckets, &mut ctx).unwrap();
        catalog.create_table(objects, &mut ctx).unwrap();
        catalog.create_table(dlq, &mut ctx).unwrap();

        // Reload and confirm all three tables present.
        let reloaded = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert!(reloaded.lookup("buckets").is_ok());
        assert!(reloaded.lookup("objects").is_ok());
        assert!(reloaded.lookup("object_dlq").is_ok());
        assert_eq!(reloaded.lookup("objects").unwrap().columns.len(), 11);
    }
}
