// Cypher logical query (M3.c): deliberately tiny, matching the locked-in
// v1 grammar (`MATCH (a)-[:TYPE]->(b) WHERE <predicate> RETURN <items>` —
// single fixed-length directed hop only, no `OPTIONAL MATCH`, no
// variable-length paths, no aggregation).
//
// The Cypher-variable-to-edge-column mapping (`a`/`b` -> `from_id`/`to_id`)
// happens once, at parse time, in `parser.rs` — `predicate` is an ordinary
// `sql::logical::Expr` referencing `__edges__`'s real column names, so the
// executor never needs to know Cypher variable names existed at all. This
// is what lets `graph::executor::execute` reuse
// `sql::executor::predicate_matches`/`eval_expr` verbatim instead of a
// second expression evaluator.

use crate::sql::logical::Expr;

#[derive(Debug, Clone, PartialEq)]
pub enum ReturnItem {
    FromVar,
    ToVar,
    EdgeColumn(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CypherQuery {
    pub from_var: String,
    pub to_var: String,
    /// `None` matches any edge type.
    pub edge_type: Option<String>,
    pub predicate: Option<Expr>,
    pub returns: Vec<ReturnItem>,
}
