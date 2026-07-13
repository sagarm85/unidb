// SQL statement builder for logical replication apply (item 28, R2).
//
// Translates an event's JSON payload into one or more SQL statements that
// reproduce the change on the target engine. The engine's SQL layer is the
// entry point (no direct heap/WAL manipulation), so this module is entirely
// at the app layer — no engine internals.
//
// JSON-to-SQL value mapping:
//   null        → NULL
//   number (int) → bare integer literal
//   number (float) → decimal literal
//   string      → single-quoted, with ' escaped as ''
//   bool        → TRUE / FALSE
//   object/array → JSON string literal (single-quoted)

use serde_json::Value as JsonValue;
use unidb::queue::Event;

use crate::TableSpec;

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("UPDATE/DELETE on table '{table}' requires a key column in the payload, but '{key}' was not found")]
    MissingKey { table: String, key: String },
    #[error("empty payload for op '{op}' on table '{table}'")]
    EmptyPayload { table: String, op: String },
}

/// Build the SQL statements to apply `event` to the target.
///
/// Returns a `Vec<String>` (usually 1 statement, 2 for UPDATE) or an empty
/// vec if the event should be skipped. Returns `Err` only for unrecoverable
/// payload defects.
pub fn build_apply_sql(event: &Event, spec: &TableSpec) -> Result<Vec<String>, ApplyError> {
    let table = &event.table_name;
    let payload = match &event.payload {
        JsonValue::Object(m) if !m.is_empty() => m,
        _ => {
            return Err(ApplyError::EmptyPayload {
                table: table.clone(),
                op: event.op.clone(),
            });
        }
    };

    match event.op.as_str() {
        "insert" => {
            let cols: Vec<&str> = payload.keys().map(String::as_str).collect();
            let vals: Vec<String> = payload.values().map(json_to_sql_literal).collect();
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                escape_ident(table),
                cols.iter()
                    .map(|c| escape_ident(c))
                    .collect::<Vec<_>>()
                    .join(", "),
                vals.join(", ")
            );
            Ok(vec![sql])
        }

        "delete" => {
            let key_val = payload
                .get(&spec.key_column)
                .ok_or_else(|| ApplyError::MissingKey {
                    table: table.clone(),
                    key: spec.key_column.clone(),
                })?;
            let sql = format!(
                "DELETE FROM {} WHERE {} = {}",
                escape_ident(table),
                escape_ident(&spec.key_column),
                json_to_sql_literal(key_val)
            );
            Ok(vec![sql])
        }

        "update" => {
            // The UPDATE payload carries the new row image. We reconstruct via
            // DELETE + INSERT using the key column from the new image.
            // If the key itself was updated, the DELETE will find no row — the
            // INSERT still lands. This is the known gap (item-26 follow-up).
            let key_val = payload
                .get(&spec.key_column)
                .ok_or_else(|| ApplyError::MissingKey {
                    table: table.clone(),
                    key: spec.key_column.clone(),
                })?;
            let delete_sql = format!(
                "DELETE FROM {} WHERE {} = {}",
                escape_ident(table),
                escape_ident(&spec.key_column),
                json_to_sql_literal(key_val)
            );
            let cols: Vec<&str> = payload.keys().map(String::as_str).collect();
            let vals: Vec<String> = payload.values().map(json_to_sql_literal).collect();
            let insert_sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                escape_ident(table),
                cols.iter()
                    .map(|c| escape_ident(c))
                    .collect::<Vec<_>>()
                    .join(", "),
                vals.join(", ")
            );
            Ok(vec![delete_sql, insert_sql])
        }

        other => {
            tracing::debug!(op = other, table = %table, "logical: unknown op, skipping");
            Ok(vec![])
        }
    }
}

/// Convert a JSON value to an inline SQL literal.
pub fn json_to_sql_literal(v: &JsonValue) -> String {
    match v {
        JsonValue::Null => "NULL".to_string(),
        JsonValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                // Use repr that roundtrips through the engine's FLOAT type.
                format!("{f}")
            } else {
                // Fallback: use the JSON representation.
                n.to_string()
            }
        }
        JsonValue::String(s) => {
            // Escape single quotes by doubling them (standard SQL).
            format!("'{}'", s.replace('\'', "''"))
        }
        // Nested objects/arrays are serialized back to JSON and stored as TEXT.
        other => {
            let json_str = other.to_string().replace('\'', "''");
            format!("'{json_str}'")
        }
    }
}

/// Return the identifier as-is. The engine's parser accepts plain identifiers
/// for typical table/column names. If reserved-word conflicts arise, the
/// caller can switch to double-quoting here.
fn escape_ident(s: &str) -> &str {
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use unidb::queue::Event;

    fn make_event(table: &str, op: &str, payload: serde_json::Value) -> Event {
        Event {
            seq: 1,
            xid: 42,
            table_name: table.to_string(),
            op: op.to_string(),
            payload,
            before: None,
            after: None,
            ts_ms: 0,
        }
    }

    fn spec(key: &str) -> TableSpec {
        TableSpec {
            table: "t".to_string(),
            key_column: key.to_string(),
        }
    }

    #[test]
    fn insert_builds_correct_sql() {
        let event = make_event("t", "insert", json!({"id": 1, "name": "alice"}));
        let stmts = build_apply_sql(&event, &spec("id")).unwrap();
        assert_eq!(stmts.len(), 1);
        // SQL must contain column names and values (order may vary with HashMap).
        let sql = &stmts[0];
        assert!(sql.starts_with("INSERT INTO t"));
        assert!(sql.contains("id"));
        assert!(sql.contains("1"));
        assert!(sql.contains("name"));
        assert!(sql.contains("'alice'"));
    }

    #[test]
    fn delete_builds_correct_sql() {
        let event = make_event("t", "delete", json!({"id": 7, "name": "bob"}));
        let stmts = build_apply_sql(&event, &spec("id")).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "DELETE FROM t WHERE id = 7");
    }

    #[test]
    fn update_builds_delete_then_insert() {
        let event = make_event("t", "update", json!({"id": 3, "name": "carol"}));
        let stmts = build_apply_sql(&event, &spec("id")).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("DELETE FROM t WHERE id = 3"));
        assert!(stmts[1].starts_with("INSERT INTO t"));
    }

    #[test]
    fn delete_without_key_returns_error() {
        let event = make_event("t", "delete", json!({"name": "dave"}));
        let err = build_apply_sql(&event, &spec("id")).unwrap_err();
        assert!(matches!(err, ApplyError::MissingKey { .. }));
    }

    #[test]
    fn unknown_op_returns_empty_vec() {
        let event = make_event("t", "truncate", json!({"id": 1}));
        let stmts = build_apply_sql(&event, &spec("id")).unwrap();
        assert!(stmts.is_empty());
    }

    #[test]
    fn string_with_single_quote_is_escaped() {
        assert_eq!(json_to_sql_literal(&json!("it's")), "'it''s'");
    }

    #[test]
    fn null_renders_as_null() {
        assert_eq!(json_to_sql_literal(&JsonValue::Null), "NULL");
    }

    #[test]
    fn bool_renders_correctly() {
        assert_eq!(json_to_sql_literal(&json!(true)), "TRUE");
        assert_eq!(json_to_sql_literal(&json!(false)), "FALSE");
    }
}
