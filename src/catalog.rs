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

use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::{DbError, Result},
    format::{PageId, INVALID_PAGE_ID, PAGE_TYPE_META},
    heap::encode_insert_redo,
    page::SlottedPage,
    sql::logical::Expr,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
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
}

/// Everything `Catalog` needs to durably persist itself, bundled so
/// mutating methods don't balloon into a long parameter list.
pub struct CatalogCtx<'a> {
    pub pool: &'a mut BufferPool,
    pub wal: &'a mut Wal,
    pub control_path: &'a Path,
    pub control: &'a mut ControlData,
    pub page_size: usize,
}

pub struct Catalog {
    tables: HashMap<String, TableDef>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
        }
    }

    /// Load the catalog from `control.catalog_root`, or return an empty
    /// catalog if this is a fresh database.
    pub fn load(control: &ControlData, pool: &mut BufferPool) -> Result<Self> {
        if control.catalog_root == INVALID_PAGE_ID {
            return Ok(Self::new());
        }
        let page = pool.fetch_page(control.catalog_root)?;
        let payload = page.get(0)?.to_vec();
        pool.unpin(control.catalog_root);
        let tables: HashMap<String, TableDef> =
            serde_json::from_slice(&payload).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;
        Ok(Self { tables })
    }

    pub fn lookup(&self, name: &str) -> Result<&TableDef> {
        self.tables
            .get(name)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))
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

    fn persist(&self, ctx: &mut CatalogCtx) -> Result<()> {
        let encoded =
            serde_json::to_vec(&self.tables).map_err(|e| DbError::CatalogCorrupt(e.to_string()))?;
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
        ctx.control.catalog_root = page_id;
        control::write(ctx.control_path, ctx.control)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, def: TableDef) {
        self.tables.insert(def.name.clone(), def);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::DEFAULT_PAGE_SIZE;
    use tempfile::tempdir;

    fn setup(dir: &std::path::Path) -> (BufferPool, Wal, std::path::PathBuf, ControlData) {
        let control_path = dir.join("control");
        let control = control::create(&control_path, DEFAULT_PAGE_SIZE).unwrap();
        let pool = BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&dir.join("db.wal"), crate::format::INVALID_LSN).unwrap();
        (pool, wal, control_path, control)
    }

    #[test]
    fn fresh_database_has_empty_catalog() {
        let dir = tempdir().unwrap();
        let (mut pool, _wal, _cp, control) = setup(dir.path());
        let catalog = Catalog::load(&control, &mut pool).unwrap();
        assert!(catalog.lookup("t").is_err());
    }

    #[test]
    fn create_table_then_lookup() {
        let dir = tempdir().unwrap();
        let (mut pool, mut wal, cp, mut control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "accounts".to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                ty: ColumnType::Int64,
            }],
            pages: vec![],
            rls_policy: None,
        };
        let mut ctx = CatalogCtx {
            pool: &mut pool,
            wal: &mut wal,
            control_path: &cp,
            control: &mut control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        catalog.create_table(def, &mut ctx).unwrap();
        let looked_up = catalog.lookup("accounts").unwrap();
        assert_eq!(looked_up.columns.len(), 1);
    }

    #[test]
    fn duplicate_create_table_is_rejected() {
        let dir = tempdir().unwrap();
        let (mut pool, mut wal, cp, mut control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = || TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            rls_policy: None,
        };
        let mut ctx = CatalogCtx {
            pool: &mut pool,
            wal: &mut wal,
            control_path: &cp,
            control: &mut control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        catalog.create_table(def(), &mut ctx).unwrap();
        let err = catalog.create_table(def(), &mut ctx);
        assert!(matches!(err, Err(DbError::TableAlreadyExists(_))));
    }

    #[test]
    fn catalog_survives_reload() {
        let dir = tempdir().unwrap();
        let (mut pool, mut wal, cp, mut control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "widgets".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    ty: ColumnType::Int64,
                },
                ColumnDef {
                    name: "data".to_string(),
                    ty: ColumnType::Json,
                },
            ],
            pages: vec![7],
            rls_policy: None,
        };
        {
            let mut ctx = CatalogCtx {
                pool: &mut pool,
                wal: &mut wal,
                control_path: &cp,
                control: &mut control,
                page_size: DEFAULT_PAGE_SIZE as usize,
            };
            catalog.create_table(def, &mut ctx).unwrap();
        }

        // Reload from the persisted control-file pointer.
        let reloaded = Catalog::load(&control, &mut pool).unwrap();
        let t = reloaded.lookup("widgets").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.pages, vec![7]);
    }

    #[test]
    fn set_rls_policy_persists() {
        use crate::sql::logical::{CmpOp, Literal};
        let dir = tempdir().unwrap();
        let (mut pool, mut wal, cp, mut control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let def = TableDef {
            name: "t".to_string(),
            columns: vec![],
            pages: vec![],
            rls_policy: None,
        };
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::Literal(Literal::Text("alice".to_string()))),
        };
        {
            let mut ctx = CatalogCtx {
                pool: &mut pool,
                wal: &mut wal,
                control_path: &cp,
                control: &mut control,
                page_size: DEFAULT_PAGE_SIZE as usize,
            };
            catalog.create_table(def, &mut ctx).unwrap();
            catalog
                .set_rls_policy("t", policy.clone(), &mut ctx)
                .unwrap();
        }

        let reloaded = Catalog::load(&control, &mut pool).unwrap();
        assert_eq!(reloaded.lookup("t").unwrap().rls_policy, Some(policy));
    }
}
