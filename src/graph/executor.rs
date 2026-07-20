// Cypher executor (M3.c): reuses `sql::executor`'s `ExecCtx`/`ExecResult`
// and (crucially) its `predicate_matches`/`eval_expr` expression evaluator
// verbatim — zero new expression-evaluation logic. By the time a
// `CypherQuery` reaches here, `predicate`/`edge_type` are already ordinary
// `sql::logical::Expr`s over `__edges__`'s real column names (the
// Cypher-variable-to-column mapping happened in `parser.rs`), so this file
// never needs to know Cypher variable names existed.
//
// The durable edge index's meta page id (P3.b) is passed as an explicit extra
// argument rather than folded into `ExecCtx` — keeps `sql::executor::ExecCtx`
// untouched (still exactly the storage/transaction infra M1–M2 built it as)
// while still letting a `from_id = <literal>` predicate route through the
// durable edge-adjacency `DiskBTree` instead of a full `__edges__` scan. It is
// a `PageId` (a `Copy` value), so it coexists cleanly with `&mut ctx.pool`.
//
// Item 95b: when `adjacency_cache` is `Some`, the hot-hub `from_id` fast path
// checks the cache before touching the B-tree. A cache hit skips all disk I/O;
// a miss runs the cold B-tree path and populates the cache for future reads.
// Setting `UNIDB_GRAPH_CACHE_HUBS=0` passes `None` here and the cold path is
// always used (no behaviour change for existing callers/tests).

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    error::{DbError, Result},
    format::PageId,
    heap::Heap,
    sql::{
        executor::{decode_row, predicate_matches, ExecCtx, ExecResult},
        logical::{CmpOp, Expr, Literal},
    },
};

use super::{
    adjacency_cache::{AdjacencyCache, EdgeRef, PROPS_INLINE_LIMIT},
    edges::{edges_table_def, EDGES_TABLE},
    index::resolve_candidates_batched_with_self_flag,
    logical::{CypherQuery, ReturnItem},
};

/// Project and wrap a completed `rows` list into an `ExecResult::Rows`.
///
/// Shared by the cache-hit fast path and the cold B-tree path so the
/// projection logic lives in exactly one place.
fn finish_projection(
    rows: Vec<Vec<Literal>>,
    query: CypherQuery,
    table_def: crate::catalog::TableDef,
) -> Result<ExecResult> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut projected = Vec::with_capacity(query.returns.len());
        for item in &query.returns {
            let val = match item {
                ReturnItem::FromVar => row[0].clone(),
                ReturnItem::ToVar => row[1].clone(),
                ReturnItem::EdgeColumn(name) => {
                    let idx = table_def
                        .columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| DbError::ColumnNotFound {
                            table: EDGES_TABLE.to_string(),
                            column: name.clone(),
                        })?;
                    row[idx].clone()
                }
            };
            projected.push(val);
        }
        out.push(projected);
    }
    let columns = query
        .returns
        .iter()
        .map(|item| match item {
            ReturnItem::FromVar => "from_id".to_string(),
            ReturnItem::ToVar => "to_id".to_string(),
            ReturnItem::EdgeColumn(name) => name.clone(),
        })
        .collect();
    Ok(ExecResult::Rows { columns, rows: out })
}

/// Find a top-level (or top-level-AND'd) `from_id = <literal>` equality —
/// mirrors `sql/executor.rs`'s `find_near` walk over the same AND-only
/// predicate shape. When found, traversal routes through the edge-list
/// index (M3.a/M3.b) instead of a full table scan.
fn find_from_id_eq(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::BinOp {
            op: CmpOp::Eq,
            lhs,
            rhs,
        } => match (lhs.as_ref(), rhs.as_ref()) {
            (Expr::Column(c), Expr::Literal(Literal::Int(n))) if c == "from_id" => Some(*n),
            (Expr::Literal(Literal::Int(n)), Expr::Column(c)) if c == "from_id" => Some(*n),
            _ => None,
        },
        Expr::And(lhs, rhs) => find_from_id_eq(lhs).or_else(|| find_from_id_eq(rhs)),
        _ => None,
    }
}

pub fn execute(
    query: CypherQuery,
    ctx: &mut ExecCtx,
    edge_index_meta: PageId,
    adjacency_cache: Option<&AdjacencyCache>,
) -> Result<ExecResult> {
    let table_def = edges_table_def();
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // The pattern's `:TYPE` filter (if any) ANDs into the same predicate
    // `WHERE` already parsed into, so both apply through the identical
    // `predicate_matches` call every candidate goes through below.
    let type_filter = query.edge_type.as_ref().map(|t| Expr::BinOp {
        op: CmpOp::Eq,
        lhs: Box::new(Expr::Column("edge_type".to_string())),
        rhs: Box::new(Expr::Literal(Literal::Text(t.clone()))),
    });
    let full_predicate = match (query.predicate.clone(), type_filter) {
        (Some(p), Some(t)) => Some(Expr::And(Box::new(p), Box::new(t))),
        (Some(p), None) => Some(p),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    };

    let rows: Vec<Vec<Literal>> =
        if let Some(from_id) = full_predicate.as_ref().and_then(find_from_id_eq) {
            // ── Cache fast path (item 95b) ────────────────────────────────────
            // Check the adjacency cache before touching the B-tree.  A hit
            // means zero disk I/O for a hot hub; a miss falls through to the
            // cold B-tree path and populates the cache so the next query hits.
            if let Some(cache) = adjacency_cache {
                if let Some(cached) = cache.get(EDGES_TABLE, from_id) {
                    // Reconstruct `Vec<Literal>` rows from the cached EdgeRef
                    // slice.  Props that were inlined are decoded directly;
                    // large props (props_inline = None) require a pool fetch
                    // via the stored RowId, preserving full correctness.
                    let mut out = Vec::with_capacity(cached.len());
                    for eref in cached.iter() {
                        let props_str = match &eref.props_inline {
                            Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                            None => {
                                // Large props: fetch the heap page via RowId.
                                let page = ctx.pool.fetch_page(eref.edge_row_id.page_id)?;
                                let raw = page.get(eref.edge_row_id.slot)?.to_vec();
                                ctx.pool.unpin(eref.edge_row_id.page_id);
                                let row = decode_row(&raw, &table_def.columns)?;
                                match row.into_iter().nth(3) {
                                    Some(Literal::Json(s)) => s,
                                    _ => String::new(),
                                }
                            }
                        };
                        let row = vec![
                            Literal::Int(from_id),
                            Literal::Int(eref.to_id),
                            Literal::Text(eref.edge_type.clone()),
                            Literal::Json(props_str),
                        ];
                        if predicate_matches(&full_predicate, &table_def.columns, &row)? {
                            out.push(row);
                        }
                    }
                    // Return directly — no B-tree touched.
                    return finish_projection(out, query, table_def);
                }
            }

            // ── Cold path: B-tree + heap scan ────────────────────────────────
            let candidates = DiskBTree::new(edge_index_meta, ctx.page_size)
                .search_eq(&OrderedValue::Int(from_id), ctx.pool)?;
            // Use the self-flag variant to guard the cache against caching
            // uncommitted self-writes (see cache population guard below).
            let (resolved, has_self_write) = resolve_candidates_batched_with_self_flag(
                &candidates,
                &snapshot,
                ctx.xid,
                ctx.pool,
                &table_def.columns,
            )?;
            let mut out = Vec::with_capacity(resolved.len());
            let mut cache_entries: Vec<EdgeRef> = Vec::with_capacity(resolved.len());
            for (row_id, row) in resolved {
                // Build a cache entry in parallel with the filtered row list.
                let edge_type = match &row[2] {
                    Literal::Text(s) => s.clone(),
                    _ => String::new(),
                };
                let props_bytes = match &row[3] {
                    Literal::Json(s) => s.as_bytes().to_vec(),
                    _ => Vec::new(),
                };
                let props_inline = if props_bytes.len() <= PROPS_INLINE_LIMIT {
                    Some(props_bytes)
                } else {
                    None
                };
                let to_id = match &row[1] {
                    Literal::Int(n) => *n,
                    _ => 0,
                };
                cache_entries.push(EdgeRef {
                    to_id,
                    edge_row_id: row_id,
                    edge_type,
                    props_inline,
                });
                if predicate_matches(&full_predicate, &table_def.columns, &row)? {
                    out.push(row);
                }
            }
            // Populate the cache so the next Cypher MATCH hits the fast path.
            //
            // Guard: skip caching when any resolved row has xmin == xid (a
            // self-written uncommitted edge).  If we cache and the transaction
            // aborts, the stale entry persists — no abort hook fires an
            // invalidation.  Pure readers (no self-written rows) safely cache
            // even while their transaction is still open.
            if let Some(cache) = adjacency_cache {
                if !has_self_write {
                    cache.insert(EDGES_TABLE, from_id, cache_entries);
                }
            }
            out
        } else {
            let edges_stored = ctx.catalog.lookup(EDGES_TABLE)?;
            let heap = Heap::open(
                ctx.page_size,
                edges_stored.fsm_meta,
                edges_stored.pages.clone(),
            );
            let mut out = Vec::new();
            for (_, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
                let row = decode_row(&bytes, &table_def.columns)?;
                if predicate_matches(&full_predicate, &table_def.columns, &row)? {
                    out.push(row);
                }
            }
            out
        };

    finish_projection(rows, query, table_def)
}
