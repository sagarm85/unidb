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
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::{DbError, Result},
    format::{PageId, INVALID_PAGE_ID, PAGE_TYPE_META},
    heap::encode_insert_redo,
    page::SlottedPage,
    sql::logical::{Expr, Literal},
    wal::Wal,
};

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
/// or, for the table-level form, inside [`ForeignKey`] (M11). Enforcement in
/// M11 is deliberately limited to **referenced-table existence** (see
/// `sql/executor.rs::enforce_referenced_tables_exist` and this milestone's
/// `PROGRESS.md` entry) — full referential integrity (referenced *row*
/// existence, `ON DELETE`/`ON UPDATE` actions) is out of scope, since there
/// is no `DROP TABLE` yet and row-level FK checks are a materially larger
/// lift than the "you can't reference a table that isn't there" guard this
/// milestone commits to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyRef {
    /// Referenced table name.
    pub table: String,
    /// Referenced column (informational in M11 — recorded for a future
    /// row-existence check, but only the `table` is enforced today).
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
    /// This table's own data heap's page list (distinct from the catalog's
    /// own storage page) — persisted here, not just kept in-memory on the
    /// `Heap` struct, so `Heap::from_pages` can reconstruct a working FSM
    /// and `scan()` after a reopen (see `heap.rs`'s tech-debt note this
    /// closes: an in-memory-only page list was silently losing track of a
    /// table's existing pages across restarts).
    pub pages: Vec<PageId>,
    pub rls_policy: Option<Expr>,
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
    pub fn load(control: &ControlData, pool: &BufferPool) -> Result<Self> {
        if control.catalog_root == INVALID_PAGE_ID {
            return Ok(Self::new());
        }
        let page = pool.fetch_page(control.catalog_root)?;
        let payload = page.get(0)?.to_vec();
        pool.unpin(control.catalog_root);
        // Backward compatible: a pre-P4.d catalog is a bare `{name: TableDef}`
        // map; the P4.d format wraps it as `{tables, stats}`. Try the old shape
        // first (it fails to parse the new one, since "tables"/"stats" aren't
        // TableDefs), then the new one.
        if let Ok(tables) = serde_json::from_slice::<HashMap<String, TableDef>>(&payload) {
            return Ok(Self {
                tables,
                stats: HashMap::new(),
            });
        }
        let p: PersistedCatalog =
            serde_json::from_slice(&payload).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;
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

    pub fn create_table(&mut self, def: TableDef, ctx: &mut CatalogCtx) -> Result<()> {
        if self.tables.contains_key(&def.name) {
            return Err(DbError::TableAlreadyExists(def.name));
        }
        self.tables.insert(def.name.clone(), def);
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
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
        self.persist(ctx)
    }

    /// `TRUNCATE` (P2.c). Drops every row by clearing the table's page list; the
    /// orphaned pages are reclaimed once Phase 1's FSM lands (as with
    /// `DROP TABLE`). The schema (columns/constraints) is unchanged.
    pub fn truncate(&mut self, table: &str, ctx: &mut CatalogCtx) -> Result<()> {
        let t = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        t.pages.clear();
        // Row set is now empty; previously gathered stats are stale.
        self.stats.remove(table);
        self.persist(ctx)
    }

    fn persist(&self, ctx: &mut CatalogCtx) -> Result<()> {
        let blob = PersistedCatalogRef {
            tables: &self.tables,
            stats: &self.stats,
        };
        let encoded =
            serde_json::to_vec(&blob).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;
        let page_id = ctx.pool.alloc_page()?;
        let (txn_id, begin_lsn) = ctx.wal.begin_mini_txn()?;
        let mut page = SlottedPage::new(page_id, PAGE_TYPE_META, ctx.page_size);
        let slot = page.insert(&encoded)?;
        debug_assert_eq!(slot, 0, "catalog page must hold exactly one blob at slot 0");
        let redo = encode_insert_redo(0, None, &encoded);
        let lsn = ctx
            .wal
            .log_insert(txn_id, begin_lsn, page_id, slot, &redo)?;
        page.set_lsn(lsn);
        ctx.pool.write_page(&page)?;
        ctx.wal.commit_mini_txn(txn_id, lsn)?;
        {
            let mut control = ctx.control.lock().unwrap_or_else(|e| e.into_inner());
            control.catalog_root = page_id;
            control::write(ctx.control_path, &control)?;
        }
        Ok(())
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
        let (mut pool, _wal, _cp, control) = setup(dir.path());
        let catalog = Catalog::load(&control.lock().unwrap(), &pool).unwrap();
        assert!(catalog.lookup("t").is_err());
    }

    #[test]
    fn create_table_then_lookup() {
        let dir = tempdir().unwrap();
        let (mut pool, mut wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "accounts".to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Int64,
            }],
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
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
        let (mut pool, mut wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = || TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
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
        let (mut pool, mut wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "widgets".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    index: None,
                    index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                    ty: ColumnType::Int64,
                },
                ColumnDef {
                    name: "data".to_string(),
                    index: None,
                    index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                    ty: ColumnType::Json,
                },
            ],
            pages: vec![7],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
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
        let (mut pool, mut wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "embeddings".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    ty: ColumnType::Int64,
                    index: None,
                    index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                },
                ColumnDef {
                    name: "vec".to_string(),
                    ty: ColumnType::Vector(384),
                    index: Some(IndexKind::Hnsw),
                    index_root: None,
                    dropped: false,
                    constraints: Default::default(),
                },
            ],
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
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
        let (mut pool, mut wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
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
}
