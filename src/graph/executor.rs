// Cypher executor (M3.c): reuses `sql::executor`'s `ExecCtx`/`ExecResult`
// and (crucially) its `predicate_matches`/`eval_expr` expression evaluator
// verbatim — zero new expression-evaluation logic. By the time a
// `CypherQuery` reaches here, `predicate`/`edge_type` are already ordinary
// `sql::logical::Expr`s over `__edges__`'s real column names (the
// Cypher-variable-to-column mapping happened in `parser.rs`), so this file
// never needs to know Cypher variable names existed.
//
// `edge_index` is passed as an explicit extra argument rather than folded
// into `ExecCtx` — keeps `sql::executor::ExecCtx` untouched (still exactly
// the storage/transaction infra M1–M2 built it as) while still letting a
// `from_id = <literal>` predicate route through the edge-list index
// instead of a full `__edges__` scan.

use crate::{
    error::{DbError, Result},
    heap::Heap,
    index_worker::IndexHandle,
    sql::{
        executor::{decode_row, predicate_matches, ExecCtx, ExecResult},
        logical::{CmpOp, Expr, Literal},
    },
};

use super::{
    edges::{edges_table_def, EDGES_TABLE},
    index::{graph_candidates, resolve_candidates_batched, EdgeIndex},
    logical::{CypherQuery, ReturnItem},
};

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
    edge_index: &EdgeIndex,
    index_worker: &IndexHandle,
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
            let candidates = graph_candidates(from_id, edge_index, index_worker);
            let resolved = resolve_candidates_batched(
                &candidates,
                &snapshot,
                ctx.xid,
                ctx.pool,
                &table_def.columns,
            )?;
            let mut out = Vec::with_capacity(resolved.len());
            for (_, row) in resolved {
                if predicate_matches(&full_predicate, &table_def.columns, &row)? {
                    out.push(row);
                }
            }
            out
        } else {
            let pages = ctx.catalog.lookup(EDGES_TABLE)?.pages.clone();
            let heap = Heap::from_pages(ctx.page_size, pages);
            let mut out = Vec::new();
            for (_, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
                let row = decode_row(&bytes, &table_def.columns)?;
                if predicate_matches(&full_predicate, &table_def.columns, &row)? {
                    out.push(row);
                }
            }
            out
        };

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
    Ok(ExecResult::Rows(out))
}
