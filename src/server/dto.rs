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
    catalog::{ColumnType, IndexKind, TableDef},
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
        // Bind placeholders are substituted before execution (P2.e); a result
        // row can never contain one.
        Literal::Param(_) => Json::Null,
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
        ExecResult::Rows { columns, rows } => {
            obj.insert("type".into(), Json::String("rows".into()));
            obj.insert(
                "columns".into(),
                Json::Array(columns.iter().map(|c| Json::String(c.clone())).collect()),
            );
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
    /// Optional positional bind parameters for `$1`, `$2`, ... in `sql`
    /// (P2.e). Absent/empty means the SQL is executed as-is. Supplying params
    /// is the injection-safe way to pass user data.
    #[serde(default)]
    pub params: Vec<Json>,
    /// Optional isolation level for a **one-shot** statement (R2): the
    /// request runs as a single transaction at this level without opening a
    /// session. Rejected on a request that also carries `X-Txn-Id` —
    /// isolation is fixed at `POST /txn/begin` for sessions.
    #[serde(default)]
    pub isolation: Option<IsolationDto>,
    /// `true` (R4) buffers the query's `rows` result server-side and returns
    /// a `cursor_id` instead of the rows, for paging via
    /// `GET /sql/cursor/{id}?limit=N`. Requires the request to produce
    /// exactly one `rows`-shaped result.
    #[serde(default)]
    pub cursor: bool,
}

/// Wire form of [`IsolationLevel`] (R1/R2). Snake-case strings on the wire:
/// `"read_committed"`, `"repeatable_read"`, `"serializable"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationDto {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationDto {
    pub fn to_engine(self) -> crate::txn::IsolationLevel {
        use crate::txn::IsolationLevel as I;
        match self {
            IsolationDto::ReadCommitted => I::ReadCommitted,
            IsolationDto::RepeatableRead => I::RepeatableRead,
            IsolationDto::Serializable => I::Serializable,
        }
    }

    pub fn from_engine(level: crate::txn::IsolationLevel) -> Self {
        use crate::txn::IsolationLevel as I;
        match level {
            I::ReadCommitted => IsolationDto::ReadCommitted,
            I::RepeatableRead => IsolationDto::RepeatableRead,
            I::Serializable => IsolationDto::Serializable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IsolationDto::ReadCommitted => "read_committed",
            IsolationDto::RepeatableRead => "repeatable_read",
            IsolationDto::Serializable => "serializable",
        }
    }
}

/// Body of `POST /txn/begin` (R1). The body itself is optional — an empty
/// body opens a `read_committed` session.
#[derive(Debug, Default, Deserialize)]
pub struct BeginTxnRequest {
    #[serde(default)]
    pub isolation: Option<IsolationDto>,
}

/// Body of `PUT /tables/{table}/rls` (R3): the policy as a SQL predicate
/// string (the same AND-only comparison subset `WHERE` accepts), e.g.
/// `"tenant_id = 7"`. Chosen over a JSON policy DSL so the existing SQL
/// parser is the single grammar — see the backlog spec's blocker note.
#[derive(Debug, Deserialize)]
pub struct RlsRequest {
    pub predicate: String,
}

/// Body of `PUT /config/slow_query_threshold_ms` (item 34, Part A).
/// `threshold_ms: 0` disables slow-query logging; positive values enable it.
#[derive(Debug, Deserialize)]
pub struct SlowQueryThresholdRequest {
    pub threshold_ms: u64,
}

/// Query params for `GET /stats/history` (item 34, Part B).
#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    /// Number of points to return; default 60, max 300.
    pub points: Option<u32>,
    /// Resolution hint echoed back in the response (ms); default 5000.
    pub interval_ms: Option<u64>,
}

/// Body of `POST /rows/batch` (R4): raw row payloads, base64-encoded (rows
/// are opaque bytes and JSON cannot carry them verbatim).
#[derive(Debug, Deserialize)]
pub struct BatchInsertRequest {
    pub rows: Vec<String>,
}

/// Query string of `GET /sql/cursor/{id}` (R4).
#[derive(Debug, Deserialize)]
pub struct CursorQuery {
    /// Rows per page; default 1000, capped at 10 000.
    #[serde(default = "default_cursor_limit")]
    pub limit: usize,
}

fn default_cursor_limit() -> usize {
    1000
}

/// Convert a REST JSON bind-parameter value to a [`Literal`] (P2.e). The value
/// is always treated as *data*: a JSON string becomes `Literal::Text` (later
/// coerced to the target column's type — UUID, TIMESTAMP, etc.), a number
/// becomes `Int` or `Float`, and so on. Objects/arrays are passed through as
/// `Json`/`Vector` for JSON and vector columns respectively.
pub fn json_to_literal(v: &Json) -> Literal {
    match v {
        Json::Null => Literal::Null,
        Json::Bool(b) => Literal::Bool(*b),
        Json::String(s) => Literal::Text(s.clone()),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Literal::Int(i)
            } else {
                Literal::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        // A JSON array of numbers is a vector literal; anything else round-trips
        // as a JSON-typed value.
        Json::Array(items) if items.iter().all(|x| x.is_number()) => Literal::Vector(
            items
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect(),
        ),
        other => Literal::Json(other.to_string()),
    }
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

// ── table introspection (S1, `GET /tables`) ────────────────────────────────

/// One table's schema in the `GET /tables` response. Internal `__…__` tables
/// (`__events__`/`__consumers__`/`__edges__`/`__lobs__`) are omitted entirely
/// by the handler — this shape describes only user tables.
#[derive(Debug, Serialize, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

/// One column's schema within [`TableInfo`].
#[derive(Debug, Serialize, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    /// Human-readable type name (`"int"`, `"text"`, `"vector(384)"`, …).
    #[serde(rename = "type")]
    pub ty: String,
    /// `false` iff the column is `NOT NULL` or `PRIMARY KEY`.
    pub nullable: bool,
    /// The column's secondary-index kind, or `null` if unindexed.
    pub index: Option<&'static str>,
}

/// Render a [`ColumnType`] as the human-readable name used on the wire. Kept in
/// the REST layer (not `catalog.rs`) so the wire vocabulary is owned here — the
/// engine's on-disk enum can evolve without silently changing the API contract.
fn column_type_name(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Int64 => "int".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Bool => "bool".to_string(),
        ColumnType::Json => "json".to_string(),
        ColumnType::Vector(n) => format!("vector({n})"),
        ColumnType::Decimal(p, s) => format!("decimal({p},{s})"),
        ColumnType::Timestamp => "timestamp".to_string(),
        ColumnType::Float => "float".to_string(),
        ColumnType::Uuid => "uuid".to_string(),
        ColumnType::Bytea => "bytea".to_string(),
        ColumnType::Date => "date".to_string(),
        ColumnType::Time => "time".to_string(),
    }
}

/// Stable wire name for a secondary-index kind. `Hnsw` keeps its historical
/// name even though it is the durable IVF-Flat index since P3.c (matching the
/// catalog/SQL surface, see `catalog::IndexKind`).
fn index_kind_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Hnsw => "hnsw",
        IndexKind::FullText => "fulltext",
        IndexKind::BTree => "btree",
        IndexKind::Csr => "csr",
    }
}

/// Build the introspection view of one table. Dropped columns (P2.c logical
/// tombstones) are excluded — they are invisible to `SELECT *`, so they are
/// invisible here too.
pub fn table_def_to_info(def: &TableDef) -> TableInfo {
    let columns = def
        .columns
        .iter()
        .filter(|c| !c.dropped)
        .map(|c| ColumnInfo {
            name: c.name.clone(),
            ty: column_type_name(&c.ty),
            nullable: !(c.constraints.not_null || c.constraints.primary_key),
            index: c.index.map(index_kind_name),
        })
        .collect();
    TableInfo {
        name: def.name.clone(),
        columns,
    }
}

/// Whether a table is an internal engine table (`__events__`, `__edges__`,
/// `__lobs__`, `__consumers__`, …) hidden from `GET /tables`. All such tables
/// share the reserved `__…__` naming convention.
pub fn is_internal_table(name: &str) -> bool {
    name.starts_with("__")
}

#[derive(Debug, Deserialize)]
pub struct AckEventsRequest {
    pub consumer: String,
    pub up_to_seq: i64,
}

// ── replication (P6.b) ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSlotRequest {
    pub name: String,
    /// `true` for a synchronous slot (commit waits for the consumer, P6.c);
    /// defaults to async.
    #[serde(default)]
    pub sync: bool,
}

#[derive(Debug, Deserialize)]
pub struct AdvanceSlotRequest {
    /// The LSN the consumer has durably applied up to.
    pub lsn: u64,
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Ship records strictly after this LSN (default 0 = from the beginning of
    /// retained WAL).
    #[serde(default)]
    pub from_lsn: u64,
}

/// JSON view of a replication slot.
pub fn slot_to_json(info: &crate::replication::SlotInfo) -> serde_json::Value {
    serde_json::json!({
        "name": info.name,
        "restart_lsn": info.restart_lsn,
        "kind": match info.kind {
            crate::replication::SlotKind::Async => "async",
            crate::replication::SlotKind::Sync => "sync",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_to_literal_maps_scalars_as_data() {
        assert_eq!(json_to_literal(&Json::Null), Literal::Null);
        assert_eq!(json_to_literal(&json!(true)), Literal::Bool(true));
        assert_eq!(json_to_literal(&json!(42)), Literal::Int(42));
        assert_eq!(json_to_literal(&json!(1.5)), Literal::Float(1.5));
        // A string — including one that looks like SQL — is plain text data.
        assert_eq!(
            json_to_literal(&json!("'; DROP TABLE t; --")),
            Literal::Text("'; DROP TABLE t; --".to_string())
        );
        // A numeric array is a vector literal.
        assert_eq!(
            json_to_literal(&json!([0.1, 0.2])),
            Literal::Vector(vec![0.1, 0.2])
        );
    }

    #[test]
    fn sql_request_params_default_to_empty() {
        let req: SqlRequest = serde_json::from_value(json!({"sql": "SELECT 1"})).unwrap();
        assert!(req.params.is_empty());
        let req2: SqlRequest =
            serde_json::from_value(json!({"sql": "SELECT $1", "params": [7]})).unwrap();
        assert_eq!(req2.params.len(), 1);
    }

    #[test]
    fn rows_result_carries_column_names() {
        let result = ExecResult::Rows {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![vec![Literal::Int(1), Literal::Text("alice".to_string())]],
        };
        assert_eq!(
            exec_result_to_json(&result),
            json!({
                "type": "rows",
                "columns": ["id", "name"],
                "rows": [[1, "alice"]]
            })
        );
    }
}
