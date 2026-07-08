//! Request/response JSON shapes for the M5 REST server, kept separate from
//! `handlers.rs` so the wire schema is easy to audit in one place — mirrors
//! how `sql/logical.rs` (the "shape" types) is kept separate from
//! `sql/executor.rs` (the "behavior" using them).
//!
//! **Why `Literal`/`ExecResult` don't just derive `Serialize`:** `Literal`
//! already derives `Serialize`/`Deserialize` unconditionally (`sql/
//! logical.rs`) — but that derive is load-bearing for the catalog's
//! on-disk format (`Expr::Literal` is embedded in the serde_json blob
//! `Catalog::persist` writes for RLS policies). Its default enum
//! representation serializes `Literal::Json(s)` as `{"Json": "<raw json
//! text as a string>"}`, which is correct and stable for that internal
//! use but is exactly the "JSON-encoded-as-a-string" shape the REST wire
//! format should *not* have (a client reading `payload.data.status`
//! shouldn't have to parse a string-within-a-string). Rather than risk
//! changing `Literal`'s existing serialization (a breaking change to the
//! catalog's on-disk format) or forking a second `Literal`-shaped type,
//! `literal_to_json` below does the REST-facing conversion explicitly,
//! reusing exactly the same per-variant mapping `queue::payload::
//! row_to_json` already established in M4 for the same reason.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value as Json};

use crate::{
    catalog::IndexKind,
    heap::RowId,
    sql::{executor::ExecResult, logical::Literal},
};

/// `Literal` -> REST-facing JSON, re-parsing `Literal::Json`'s raw text
/// into a real nested `serde_json::Value` rather than leaving it as a
/// JSON-encoded string.
pub fn literal_to_json(lit: &Literal) -> Json {
    match lit {
        Literal::Null => Json::Null,
        Literal::Int(n) => Json::Number(Number::from(*n)),
        Literal::Text(s) => Json::String(s.clone()),
        Literal::Bool(b) => Json::Bool(*b),
        Literal::Json(s) => serde_json::from_str(s).unwrap_or(Json::Null),
        Literal::Vector(v) => Json::Array(
            v.iter()
                .map(|f| {
                    Number::from_f64(*f as f64)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                })
                .collect(),
        ),
        // Exact types serialize as strings so JSON's f64 numbers never lose
        // precision (P2.a) — decimal text and canonical UTC timestamp text.
        Literal::Decimal(value, scale) => {
            Json::String(crate::sql::logical::format_decimal(*value, *scale))
        }
        Literal::Timestamp(micros) => Json::String(crate::sql::datetime::format_timestamp(*micros)),
        // P2.b scalar types: floats as JSON numbers; uuid/bytea/date/time as
        // canonical strings.
        Literal::Float(f) => Number::from_f64(*f).map(Json::Number).unwrap_or(Json::Null),
        Literal::Uuid(b) => Json::String(crate::sql::executor::format_uuid(b)),
        Literal::Bytea(b) => Json::String(crate::sql::executor::format_bytea(b)),
        Literal::Date(d) => Json::String(crate::sql::datetime::format_date(*d)),
        Literal::Time(t) => Json::String(crate::sql::datetime::format_time(*t)),
    }
}

/// `ExecResult` -> a tagged JSON object (`{"type": "...", ...}`), the
/// response body shape for `/sql` and `/cypher`.
pub fn exec_result_to_json(result: &ExecResult) -> Json {
    let mut obj = Map::new();
    match result {
        ExecResult::CreatedTable => {
            obj.insert("type".into(), Json::String("created_table".into()));
        }
        ExecResult::CreatedIndex => {
            obj.insert("type".into(), Json::String("created_index".into()));
        }
        ExecResult::Inserted { count } => {
            obj.insert("type".into(), Json::String("inserted".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Updated { count } => {
            obj.insert("type".into(), Json::String("updated".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Deleted { count } => {
            obj.insert("type".into(), Json::String("deleted".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Rows(rows) => {
            obj.insert("type".into(), Json::String("rows".into()));
            let json_rows: Vec<Json> = rows
                .iter()
                .map(|row| Json::Array(row.iter().map(literal_to_json).collect()))
                .collect();
            obj.insert("rows".into(), Json::Array(json_rows));
        }
        ExecResult::AlteredTable => {
            obj.insert("type".into(), Json::String("altered_table".into()));
        }
        ExecResult::DroppedTable => {
            obj.insert("type".into(), Json::String("dropped_table".into()));
        }
        ExecResult::Truncated { count } => {
            obj.insert("type".into(), Json::String("truncated".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
    }
    Json::Object(obj)
}

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
}

#[derive(Debug, Deserialize)]
pub struct CypherRequest {
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct RowIdResponse {
    pub row_id: RowId,
}

#[derive(Debug, Deserialize)]
pub struct CreateEdgeRequest {
    pub from_id: i64,
    pub to_id: i64,
    pub edge_type: String,
    /// Raw JSON, re-serialized to text for `Engine::create_edge`'s `&str`
    /// signature — defaults to `{}` if omitted, matching `create_edge`'s
    /// own tests' convention.
    #[serde(default = "default_props")]
    pub props: Json,
}

fn default_props() -> Json {
    Json::Object(Map::new())
}

#[derive(Debug, Deserialize)]
pub struct DeleteEdgeRequest {
    pub from_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetIndexRequest {
    pub table: String,
    pub column: String,
    pub kind: Option<IndexKind>,
}

#[derive(Debug, Deserialize)]
pub struct AckEventsRequest {
    pub consumer: String,
    pub up_to_seq: i64,
}
