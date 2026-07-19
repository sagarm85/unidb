//! System catalog introspection (Milestone 18, Epic C).
//!
//! Exposes `information_schema.*` / `unidb_catalog.*` as **synthesized virtual
//! relations** you `SELECT` from over the ordinary query surface — no bespoke
//! REST endpoints, no on-disk tables, no vacuum/MVCC interaction. When a `FROM`
//! name is one of the reserved introspection names below, the planner supplies
//! the fixed schema from [`virtual_schema`] and the runner materializes the rows
//! from the live in-memory [`Catalog`] via [`virtual_rows`] at scan time — so
//! the result is *always current* and reachable identically from embed, attach,
//! and the server (all three funnel through the same executor). See
//! `docs/backlog/18_engine_access_contract.md` (design note) for the landmine
//! decisions this implements.
//!
//! **Read-only projection of metadata that already exists.** FK / PK / UNIQUE /
//! CHECK all already parse and persist on [`Catalog`] (M11); this module only
//! reads them back. It stores nothing and bumps no format version.
//!
//! Constraint names are *synthesized* deterministically in the Postgres style
//! (`<table>_pkey`, `<table>_<col>_key`, `<table>_<cols>_fkey`,
//! `<table>_<col>_check`) because unidb does not retain named constraints —
//! see [`pk_name`] / [`unique_name`] / [`fk_name`] / [`check_name`].

use crate::btree_index::{DiskBTree, OrderedValue, RangeOp};
use crate::bufferpool::BufferPool;
use crate::catalog::{Catalog, ColumnType, ForeignKey, IndexKind, TableDef};
use crate::error::Result;
use crate::format::PageId;
use crate::format::Xid;
use crate::mvcc::Snapshot;
use crate::queue::{consumers_table_def, events_table_def, CONSUMERS_TABLE, EVENTS_TABLE};
use crate::sql::executor;
use crate::sql::logical::Literal;
use crate::sql::plan::ColumnRef;

/// Database "catalog" name reported by `*_catalog` columns. unidb is
/// single-database per instance, so this is a constant.
const DB_CATALOG: &str = "unidb";
/// The one schema unidb exposes — it has no schema namespacing, so every object
/// lives in `public` and every `*_schema` column reports it. Saying so plainly
/// beats inventing a namespace (design note, landmine 4).
const PUBLIC_SCHEMA: &str = "public";

/// Every reserved introspection relation name. A `FROM` reference matching one
/// (case-insensitively) resolves to a virtual relation, never a base table.
pub const RELATIONS: &[&str] = &[
    "information_schema.tables",
    "information_schema.columns",
    "information_schema.table_constraints",
    "information_schema.key_column_usage",
    "information_schema.referential_constraints",
    "unidb_catalog.indexes",
    // item 29 C3: per-consumer CDC lag (reads __consumers__ + __events__ via the
    // heap, so virtual_rows cannot materialize it — query_exec.rs routes it to
    // subscription_lag_rows which has access to pool+snapshot).
    "unidb_catalog.subscription_lag",
    // item-24 Z5: AuthZ introspection — roles, grants, named policies.
    // Materialized from the live RoleStore (roles/grants) and Catalog (policies).
    // These are read-only via SELECT; the write path is Z1 DDL.
    "unidb_catalog.roles",
    "unidb_catalog.grants",
    "unidb_catalog.policies",
    // item-24 Z4: role membership + users catalog.
    "unidb_catalog.role_members",
    "unidb_catalog.users",
];

/// Is `name` one of the reserved introspection relations? Case-insensitive so
/// `INFORMATION_SCHEMA.TABLES` resolves too, matching SQL's case-insensitive
/// identifier handling for these well-known names.
pub fn is_virtual_relation(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RELATIONS.contains(&lower.as_str())
}

/// The fixed output schema of a virtual relation, or `None` if `name` is not
/// one. Column order here is the on-the-wire order the rows in [`virtual_rows`]
/// must match exactly.
pub fn virtual_schema(name: &str) -> Option<Vec<ColumnRef>> {
    let cols: &[(&str, ColumnType)] = match name.to_ascii_lowercase().as_str() {
        "information_schema.tables" => &[
            ("table_catalog", ColumnType::Text),
            ("table_schema", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("table_type", ColumnType::Text),
        ],
        "information_schema.columns" => &[
            ("table_catalog", ColumnType::Text),
            ("table_schema", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("column_name", ColumnType::Text),
            ("ordinal_position", ColumnType::Int64),
            ("column_default", ColumnType::Text),
            ("is_nullable", ColumnType::Text),
            ("data_type", ColumnType::Text),
        ],
        "information_schema.table_constraints" => &[
            ("constraint_catalog", ColumnType::Text),
            ("constraint_schema", ColumnType::Text),
            ("constraint_name", ColumnType::Text),
            ("table_catalog", ColumnType::Text),
            ("table_schema", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("constraint_type", ColumnType::Text),
        ],
        "information_schema.key_column_usage" => &[
            ("constraint_catalog", ColumnType::Text),
            ("constraint_schema", ColumnType::Text),
            ("constraint_name", ColumnType::Text),
            ("table_catalog", ColumnType::Text),
            ("table_schema", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("column_name", ColumnType::Text),
            ("ordinal_position", ColumnType::Int64),
            ("position_in_unique_constraint", ColumnType::Int64),
        ],
        "information_schema.referential_constraints" => &[
            ("constraint_catalog", ColumnType::Text),
            ("constraint_schema", ColumnType::Text),
            ("constraint_name", ColumnType::Text),
            ("unique_constraint_catalog", ColumnType::Text),
            ("unique_constraint_schema", ColumnType::Text),
            ("unique_constraint_name", ColumnType::Text),
            ("match_option", ColumnType::Text),
            ("update_rule", ColumnType::Text),
            ("delete_rule", ColumnType::Text),
        ],
        "unidb_catalog.indexes" => &[
            ("table_name", ColumnType::Text),
            ("index_name", ColumnType::Text),
            ("column_name", ColumnType::Text),
            ("index_type", ColumnType::Text),
            ("is_unique", ColumnType::Bool),
        ],
        // item 29 C3: per-consumer CDC lag (materialized by subscription_lag_rows).
        "unidb_catalog.subscription_lag" => &[
            ("consumer", ColumnType::Text),
            ("offset", ColumnType::Int64),
            ("max_seq", ColumnType::Int64),
            ("lag_events", ColumnType::Int64),
            ("oldest_unconsumed_ts_ms", ColumnType::Int64),
            ("lag_seconds", ColumnType::Float),
        ],
        // item-24 Z5: AuthZ introspection (read-only; write via Z1 DDL).
        "unidb_catalog.roles" => &[("name", ColumnType::Text)],
        "unidb_catalog.grants" => &[
            ("role", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("operation", ColumnType::Text),
        ],
        "unidb_catalog.policies" => &[
            ("name", ColumnType::Text),
            ("table_name", ColumnType::Text),
            ("operation", ColumnType::Text),
            ("using_expr", ColumnType::Text),
        ],
        // item-24 Z4: role membership + users catalog.
        "unidb_catalog.role_members" => &[("role", ColumnType::Text), ("member", ColumnType::Text)],
        "unidb_catalog.users" => &[
            ("name", ColumnType::Text),
            ("is_superuser", ColumnType::Bool),
        ],
        _ => return None,
    };
    Some(
        cols.iter()
            .map(|(n, ty)| ColumnRef {
                qualifier: name.to_string(),
                name: (*n).to_string(),
                ty: *ty,
            })
            .collect(),
    )
}

/// Materialize a virtual relation's rows from the live catalog. Rows are in the
/// same column order as [`virtual_schema`]. Tables are visited in name order for
/// deterministic output; engine-internal `__…__` tables are hidden (they have no
/// user-facing schema, matching `GET /tables`).
///
/// `authz` is required for the item-24 Z5 relations (`unidb_catalog.roles`,
/// `unidb_catalog.grants`, `unidb_catalog.policies`). When absent, those
/// relations return empty rows (unit-test fallback).
pub fn virtual_rows(
    name: &str,
    catalog: &Catalog,
    authz: Option<&crate::authz::RoleStore>,
) -> Result<Vec<Vec<Literal>>> {
    let mut defs: Vec<&TableDef> = catalog
        .tables()
        .filter(|t| !t.name.starts_with("__"))
        .collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));

    let rows = match name.to_ascii_lowercase().as_str() {
        "information_schema.tables" => tables_rows(&defs),
        "information_schema.columns" => columns_rows(&defs),
        "information_schema.table_constraints" => table_constraints_rows(&defs, catalog),
        "information_schema.key_column_usage" => key_column_usage_rows(&defs, catalog),
        "information_schema.referential_constraints" => {
            referential_constraints_rows(&defs, catalog)
        }
        "unidb_catalog.indexes" => indexes_rows(&defs),
        // item-24 Z5: AuthZ introspection.
        "unidb_catalog.roles" => authz.map(roles_rows).unwrap_or_default(),
        "unidb_catalog.grants" => authz.map(grants_rows).unwrap_or_default(),
        "unidb_catalog.policies" => policies_rows(&defs),
        // item-24 Z4: role membership + users catalog.
        "unidb_catalog.role_members" => authz.map(role_members_rows).unwrap_or_default(),
        "unidb_catalog.users" => authz.map(users_rows).unwrap_or_default(),
        // Only reached if a caller passes a non-relation name; the planner
        // guards this, so an empty set is a safe, non-panicking fallback.
        _ => Vec::new(),
    };
    Ok(rows)
}

// ── type & constraint-name vocabulary ───────────────────────────────────────

/// Canonical `data_type` string for a column type (design note landmine 4 +
/// guide B3). unidb's own vocabulary — close to SQL-standard where unambiguous,
/// documented in the Application Builder's Guide so builders never guess.
pub fn data_type_name(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Int64 => "bigint".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Bool => "boolean".to_string(),
        ColumnType::Json => "json".to_string(),
        ColumnType::Vector(n) => format!("vector({n})"),
        ColumnType::Decimal(p, s) => format!("numeric({p},{s})"),
        ColumnType::Timestamp => "timestamp".to_string(),
        ColumnType::Float => "double precision".to_string(),
        ColumnType::Uuid => "uuid".to_string(),
        ColumnType::Bytea => "bytea".to_string(),
        ColumnType::Date => "date".to_string(),
        ColumnType::Time => "time".to_string(),
    }
}

fn index_type_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Hnsw => "hnsw",
        IndexKind::FullText => "fulltext",
        IndexKind::BTree => "btree",
        IndexKind::Csr => "csr",
    }
}

fn pk_name(table: &str) -> String {
    format!("{table}_pkey")
}

fn unique_name(table: &str, cols: &[String]) -> String {
    format!("{table}_{}_key", cols.join("_"))
}

fn fk_name(table: &str, cols: &[String]) -> String {
    format!("{table}_{}_fkey", cols.join("_"))
}

fn check_name(table: &str, ordinal: usize) -> String {
    if ordinal == 0 {
        format!("{table}_check")
    } else {
        format!("{table}_check{ordinal}")
    }
}

/// The PK columns of a table, in key order, unifying the two catalog
/// representations: a table-level `PRIMARY KEY (a, b)` (`constraints.
/// primary_key`) or column-level `id … PRIMARY KEY` markers. The two are
/// mutually exclusive in practice; table-level wins if present.
fn pk_columns(def: &TableDef) -> Vec<String> {
    if !def.constraints.primary_key.is_empty() {
        return def.constraints.primary_key.clone();
    }
    def.columns
        .iter()
        .filter(|c| !c.dropped && c.constraints.primary_key)
        .map(|c| c.name.clone())
        .collect()
}

/// Every UNIQUE constraint of a table as an ordered column set, from both the
/// table-level `UNIQUE (a, b)` form and column-level `UNIQUE` markers. A PK
/// column's implied uniqueness is reported as the PK, not a second UNIQUE row
/// (matching Postgres), so column-level PK markers are excluded here.
fn unique_constraints(def: &TableDef) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = def.constraints.unique.clone();
    for c in &def.columns {
        if !c.dropped && c.constraints.unique && !c.constraints.primary_key {
            out.push(vec![c.name.clone()]);
        }
    }
    out
}

/// Resolve one foreign key into `(referencing_columns, ref_table,
/// ref_columns)`, unifying the column-level `REFERENCES t(c)` and table-level
/// `FOREIGN KEY (…) REFERENCES t(…)` forms. For a column-level reference with no
/// explicit target column, the referenced table's PK columns are used (the
/// implicit `REFERENCES t` form).
fn resolved_fks(def: &TableDef, catalog: &Catalog) -> Vec<(Vec<String>, String, Vec<String>)> {
    let mut out = Vec::new();
    // Column-level references.
    for c in &def.columns {
        if c.dropped {
            continue;
        }
        if let Some(fkref) = &c.constraints.references {
            let ref_cols = match &fkref.column {
                Some(col) => vec![col.clone()],
                None => catalog
                    .lookup(&fkref.table)
                    .ok()
                    .map(pk_columns)
                    .unwrap_or_default(),
            };
            out.push((vec![c.name.clone()], fkref.table.clone(), ref_cols));
        }
    }
    // Table-level foreign keys.
    for fk in &def.constraints.foreign_keys {
        let ForeignKey {
            columns,
            ref_table,
            ref_columns,
        } = fk;
        out.push((columns.clone(), ref_table.clone(), ref_columns.clone()));
    }
    out
}

/// The name + key-column order of the referenced table's constraint an FK
/// targets: its PK if the referenced columns match the PK set, else a matching
/// UNIQUE constraint, else a best-effort `<ref_table>_pkey` (unidb does not
/// enforce that an FK target is actually a declared key — documented).
fn referenced_constraint(
    ref_table: &str,
    ref_columns: &[String],
    catalog: &Catalog,
) -> (String, Vec<String>) {
    let Ok(def) = catalog.lookup(ref_table) else {
        return (pk_name(ref_table), ref_columns.to_vec());
    };
    let pk = pk_columns(def);
    let same_set = |a: &[String], b: &[String]| {
        let mut a = a.to_vec();
        let mut b = b.to_vec();
        a.sort();
        b.sort();
        a == b
    };
    if !pk.is_empty() && same_set(&pk, ref_columns) {
        return (pk_name(ref_table), pk);
    }
    for u in unique_constraints(def) {
        if same_set(&u, ref_columns) {
            return (unique_name(ref_table, &u), u);
        }
    }
    (pk_name(ref_table), ref_columns.to_vec())
}

// ── row builders (one per relation) ─────────────────────────────────────────

fn t(s: &str) -> Literal {
    Literal::Text(s.to_string())
}

fn tables_rows(defs: &[&TableDef]) -> Vec<Vec<Literal>> {
    defs.iter()
        .map(|d| vec![t(DB_CATALOG), t(PUBLIC_SCHEMA), t(&d.name), t("BASE TABLE")])
        .collect()
}

fn columns_rows(defs: &[&TableDef]) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    for d in defs {
        let mut ordinal = 0i64;
        for c in &d.columns {
            if c.dropped {
                continue;
            }
            ordinal += 1;
            let default = match &c.constraints.default {
                Some(lit) => t(&render_default(lit)),
                None => Literal::Null,
            };
            let nullable = if c.constraints.not_null || c.constraints.primary_key {
                "NO"
            } else {
                "YES"
            };
            rows.push(vec![
                t(DB_CATALOG),
                t(PUBLIC_SCHEMA),
                t(&d.name),
                t(&c.name),
                Literal::Int(ordinal),
                default,
                t(nullable),
                t(&data_type_name(&c.ty)),
            ]);
        }
    }
    rows
}

fn table_constraints_rows(defs: &[&TableDef], catalog: &Catalog) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    let push = |table: &str, cname: String, ctype: &str, rows: &mut Vec<Vec<Literal>>| {
        rows.push(vec![
            t(DB_CATALOG),
            t(PUBLIC_SCHEMA),
            t(&cname),
            t(DB_CATALOG),
            t(PUBLIC_SCHEMA),
            t(table),
            t(ctype),
        ]);
    };
    for d in defs {
        let pk = pk_columns(d);
        if !pk.is_empty() {
            push(&d.name, pk_name(&d.name), "PRIMARY KEY", &mut rows);
        }
        for u in unique_constraints(d) {
            push(&d.name, unique_name(&d.name, &u), "UNIQUE", &mut rows);
        }
        for (referencing, _rt, _rc) in resolved_fks(d, catalog) {
            push(
                &d.name,
                fk_name(&d.name, &referencing),
                "FOREIGN KEY",
                &mut rows,
            );
        }
        // CHECK constraints: column-level then table-level, numbered in a stable
        // order so names are deterministic across reopens.
        let mut check_ord = 0usize;
        for c in &d.columns {
            if !c.dropped && c.constraints.check.is_some() {
                push(&d.name, check_name(&d.name, check_ord), "CHECK", &mut rows);
                check_ord += 1;
            }
        }
        for _ in &d.constraints.checks {
            push(&d.name, check_name(&d.name, check_ord), "CHECK", &mut rows);
            check_ord += 1;
        }
    }
    rows
}

fn key_column_usage_rows(defs: &[&TableDef], catalog: &Catalog) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    let push = |table: &str,
                cname: &str,
                col: &str,
                ord: i64,
                pos_uniq: Literal,
                rows: &mut Vec<Vec<Literal>>| {
        rows.push(vec![
            t(DB_CATALOG),
            t(PUBLIC_SCHEMA),
            t(cname),
            t(DB_CATALOG),
            t(PUBLIC_SCHEMA),
            t(table),
            t(col),
            Literal::Int(ord),
            pos_uniq,
        ]);
    };
    for d in defs {
        // PRIMARY KEY key columns.
        let pk = pk_columns(d);
        if !pk.is_empty() {
            let name = pk_name(&d.name);
            for (i, col) in pk.iter().enumerate() {
                push(&d.name, &name, col, i as i64 + 1, Literal::Null, &mut rows);
            }
        }
        // UNIQUE key columns.
        for u in unique_constraints(d) {
            let name = unique_name(&d.name, &u);
            for (i, col) in u.iter().enumerate() {
                push(&d.name, &name, col, i as i64 + 1, Literal::Null, &mut rows);
            }
        }
        // FOREIGN KEY key columns: ordinal within the FK, plus
        // position_in_unique_constraint = the 1-based ordinal of the matching
        // referenced column inside the referenced constraint's key order. This
        // is what aligns FK column i with referenced column i for COMPOSITE keys
        // regardless of declaration order (see the worked-example join).
        for (referencing, ref_table, ref_columns) in resolved_fks(d, catalog) {
            let name = fk_name(&d.name, &referencing);
            let (_uniq_name, uniq_order) = referenced_constraint(&ref_table, &ref_columns, catalog);
            for (i, col) in referencing.iter().enumerate() {
                // The referenced column this FK column maps to (same index),
                // then its position within the referenced key order.
                let pos = ref_columns
                    .get(i)
                    .and_then(|rc| uniq_order.iter().position(|u| u == rc))
                    .map(|p| Literal::Int(p as i64 + 1))
                    .unwrap_or(Literal::Null);
                push(&d.name, &name, col, i as i64 + 1, pos, &mut rows);
            }
        }
    }
    rows
}

fn referential_constraints_rows(defs: &[&TableDef], catalog: &Catalog) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    for d in defs {
        for (referencing, ref_table, ref_columns) in resolved_fks(d, catalog) {
            let fname = fk_name(&d.name, &referencing);
            let (uniq_name, _order) = referenced_constraint(&ref_table, &ref_columns, catalog);
            rows.push(vec![
                t(DB_CATALOG),
                t(PUBLIC_SCHEMA),
                t(&fname),
                t(DB_CATALOG),
                t(PUBLIC_SCHEMA),
                t(&uniq_name),
                // MATCH SIMPLE reports as 'NONE'; unidb has no ON UPDATE/DELETE
                // actions (M11 scope), so both rules are the SQL default.
                t("NONE"),
                t("NO ACTION"),
                t("NO ACTION"),
            ]);
        }
    }
    rows
}

fn indexes_rows(defs: &[&TableDef]) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    for d in defs {
        for c in &d.columns {
            if c.dropped {
                continue;
            }
            let Some(kind) = c.index else { continue };
            // unidb secondary indexes are non-unique; uniqueness is a *constraint*
            // property. Report whether the indexed column carries a UNIQUE/PK
            // constraint so a tool can badge it (documented in the guide).
            let is_unique = c.constraints.unique
                || c.constraints.primary_key
                || d.constraints.primary_key.iter().any(|p| p == &c.name);
            rows.push(vec![
                t(&d.name),
                t(&format!("{}_{}_idx", d.name, c.name)),
                t(&c.name),
                t(index_type_name(kind)),
                Literal::Bool(is_unique),
            ]);
        }
    }
    rows
}

/// Render a column `DEFAULT` literal back to a canonical text form for
/// `information_schema.columns.column_default`. Text is single-quoted; other
/// scalars render as-written. This is *canonical, not byte-identical* to the
/// original DDL (design note landmine 2).
fn render_default(lit: &Literal) -> String {
    match lit {
        Literal::Int(i) => i.to_string(),
        Literal::Text(s) => format!("'{s}'"),
        Literal::Bool(b) => b.to_string(),
        Literal::Json(s) => format!("'{s}'"),
        Literal::Decimal(unscaled, scale) => render_decimal(*unscaled, *scale),
        Literal::Float(f) => f.to_string(),
        Literal::Null => "NULL".to_string(),
        // Temporal / binary / vector defaults are uncommon; render via Debug as
        // a best effort (documented as canonical-not-exact).
        other => format!("{other:?}"),
    }
}

fn render_decimal(unscaled: i128, scale: u8) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let neg = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale {
        format!("{}{}", "0".repeat(scale - digits.len() + 1), digits)
    } else {
        digits
    };
    let point = padded.len() - scale;
    let s = format!("{}.{}", &padded[..point], &padded[point..]);
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

// ── item-24 Z5 row builders ──────────────────────────────────────────────────

/// `unidb_catalog.roles` — one row per named role (not users, not grants).
fn roles_rows(authz: &crate::authz::RoleStore) -> Vec<Vec<Literal>> {
    let mut roles = authz.roles();
    roles.sort();
    roles.into_iter().map(|name| vec![t(&name)]).collect()
}

/// `unidb_catalog.grants` — one row per (grantee, table, privilege) triple.
fn grants_rows(authz: &crate::authz::RoleStore) -> Vec<Vec<Literal>> {
    let mut grants = authz.grants();
    // Stable order: (role, table, op).
    grants.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    grants
        .into_iter()
        .map(|(role, table, priv_)| vec![t(&role), t(&table), t(priv_.as_str())])
        .collect()
}

/// `unidb_catalog.role_members` — one row per `(role, member)` pair
/// (item-24 Z4). `member` is the user or role granted membership in `role`.
fn role_members_rows(authz: &crate::authz::RoleStore) -> Vec<Vec<Literal>> {
    let mut pairs = authz.memberships();
    // Stable order: (role, member).
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pairs
        .into_iter()
        .map(|(role, member)| vec![t(&role), t(&member)])
        .collect()
}

/// `unidb_catalog.users` — one row per user with superuser flag (item-24 Z4).
fn users_rows(authz: &crate::authz::RoleStore) -> Vec<Vec<Literal>> {
    let mut users = authz.users();
    // Stable order by username.
    users.sort_by(|a, b| a.0.cmp(&b.0));
    users
        .into_iter()
        .map(|(name, superuser)| vec![t(&name), Literal::Bool(superuser)])
        .collect()
}

/// `unidb_catalog.policies` — one row per named policy across all tables.
fn policies_rows(defs: &[&TableDef]) -> Vec<Vec<Literal>> {
    let mut rows = Vec::new();
    for def in defs {
        for p in &def.policies {
            rows.push(vec![
                t(&p.name),
                t(&p.table),
                t(p.op.as_str()),
                t(&p.using_expr),
            ]);
        }
    }
    // Stable order: (table, name).
    rows.sort_by(|a, b| match (&a[1], &b[1], &a[0], &b[0]) {
        (Literal::Text(ta), Literal::Text(tb), Literal::Text(na), Literal::Text(nb)) => {
            ta.cmp(tb).then(na.cmp(nb))
        }
        _ => std::cmp::Ordering::Equal,
    });
    rows
}

/// Materialize `unidb_catalog.subscription_lag` rows (item 29, C3).
/// Called from `query_exec::scan` rather than `virtual_rows` because it needs
/// pool + snapshot access that the plain catalog path doesn't carry.
///
/// Columns (in order, matching `virtual_schema`):
/// consumer, offset, max_seq, lag_events, oldest_unconsumed_ts_ms, lag_seconds
pub fn subscription_lag_rows(
    catalog: &Catalog,
    pool: &BufferPool,
    page_size: usize,
    snapshot: &Snapshot,
    xid: Xid,
    event_seq_index_meta: Option<PageId>,
) -> Result<Vec<Vec<Literal>>> {
    use crate::heap::Heap;

    let consumers_def = match catalog.lookup(CONSUMERS_TABLE) {
        Ok(t) => t.clone(),
        Err(_) => return Ok(Vec::new()),
    };
    let events_def = match catalog.lookup(EVENTS_TABLE) {
        Ok(t) => t.clone(),
        Err(_) => return Ok(Vec::new()),
    };
    let Some(index_meta) = event_seq_index_meta else {
        return Ok(Vec::new());
    };

    let consumers_heap = Heap::open(
        page_size,
        consumers_def.fsm_meta,
        consumers_def.pages.clone(),
    );
    let events_heap = Heap::open(page_size, events_def.fsm_meta, events_def.pages.clone());

    // Max seq across all events (O(log n) via the B-tree's rightmost leaf).
    let max_seq = DiskBTree::new(index_meta, page_size)
        .max_entry(pool)
        .unwrap_or(None)
        .and_then(|(k, _)| {
            if let OrderedValue::Int(s) = k {
                Some(s)
            } else {
                None
            }
        })
        .unwrap_or(0);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let consumer_bytes = consumers_heap.scan(snapshot, xid, pool).unwrap_or_default();
    let mut rows = Vec::new();

    for (_, bytes) in consumer_bytes {
        let row = match executor::decode_row(&bytes, &consumers_table_def().columns) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (name, offset) = match (&row[0], &row[1]) {
            (Literal::Text(n), Literal::Int(o)) => (n.clone(), *o),
            _ => continue,
        };

        // Oldest unacked event: first entry after `offset` in the seq index.
        let candidates = DiskBTree::new(index_meta, page_size)
            .search_range_limit(RangeOp::Gt, &OrderedValue::Int(offset), 1, pool)
            .unwrap_or_default();

        let (oldest_ts_ms, lag_seconds) = if let Some(&row_id) = candidates.first() {
            let ts = events_heap
                .get(row_id, snapshot, xid, pool)
                .ok()
                .and_then(|b| executor::decode_row(&b, &events_table_def().columns).ok())
                .and_then(|r| match &r[4] {
                    Literal::Json(s) => serde_json::from_str::<serde_json::Value>(s)
                        .ok()
                        .and_then(|v| v.get("ts_ms").and_then(|m| m.as_i64())),
                    _ => None,
                })
                .unwrap_or(0);
            let lag_s = if ts > 0 && now_ms >= ts {
                (now_ms - ts) as f64 / 1000.0
            } else {
                0.0
            };
            (ts, lag_s)
        } else {
            (0i64, 0.0f64)
        };

        let lag_events = (max_seq - offset).max(0);
        rows.push(vec![
            Literal::Text(name),
            Literal::Int(offset),
            Literal::Int(max_seq),
            Literal::Int(lag_events),
            Literal::Int(oldest_ts_ms),
            Literal::Float(lag_seconds),
        ]);
    }

    // Stable order by consumer name.
    rows.sort_by(|a, b| match (&a[0], &b[0]) {
        (Literal::Text(x), Literal::Text(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_are_recognized_case_insensitively() {
        assert!(is_virtual_relation("information_schema.tables"));
        assert!(is_virtual_relation("INFORMATION_SCHEMA.TABLES"));
        assert!(is_virtual_relation("unidb_catalog.indexes"));
        assert!(!is_virtual_relation("public.tables"));
        assert!(!is_virtual_relation("orders"));
        // Every registered relation has a schema, and vice versa.
        for r in RELATIONS {
            assert!(virtual_schema(r).is_some(), "{r} has no schema");
        }
        assert!(virtual_schema("not.a.relation").is_none());
    }

    #[test]
    fn data_type_names_are_stable() {
        assert_eq!(data_type_name(&ColumnType::Int64), "bigint");
        assert_eq!(data_type_name(&ColumnType::Text), "text");
        assert_eq!(data_type_name(&ColumnType::Bool), "boolean");
        assert_eq!(data_type_name(&ColumnType::Vector(384)), "vector(384)");
        assert_eq!(data_type_name(&ColumnType::Decimal(10, 2)), "numeric(10,2)");
        assert_eq!(data_type_name(&ColumnType::Float), "double precision");
    }

    #[test]
    fn constraint_name_synthesis_is_postgres_shaped() {
        assert_eq!(pk_name("orders"), "orders_pkey");
        assert_eq!(
            unique_name("users", &["email".to_string()]),
            "users_email_key"
        );
        assert_eq!(
            fk_name(
                "line_items",
                &["o_region".to_string(), "o_order_no".to_string()]
            ),
            "line_items_o_region_o_order_no_fkey"
        );
        assert_eq!(check_name("t", 0), "t_check");
        assert_eq!(check_name("t", 1), "t_check1");
    }

    #[test]
    fn decimal_default_renders_with_scale() {
        assert_eq!(render_decimal(990, 2), "9.90");
        assert_eq!(render_decimal(-5, 2), "-0.05");
        assert_eq!(render_decimal(100, 0), "100");
    }
}
