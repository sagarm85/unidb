// Row -> JSON payload conversion (M4.a). New code: `encode_row`/`decode_row`
// (sql/executor.rs) go to/from the hand-rolled on-disk binary format, not
// JSON — this is the only place a `Vec<Literal>` becomes a JSON string, for
// `__events__`'s `payload` column.
//
// Item 60: replaced `serde_json::json!` + `serde_json::Map` with a manual
// string builder (`write_row_json` + `build_event_envelope_str`) to
// eliminate the heap allocation of a `serde_json::Value` AST. The on-wire
// format is identical; the only change is we never allocate an intermediate
// `Value` tree.

use serde_json::{Map, Number, Value as JsonValue};

use crate::{catalog::ColumnDef, sql::logical::Literal};

// ──────────────────────────────────────────────────────────────────────────────
// Zero-allocation path (item 60): build the CDC envelope as a raw String.
// ──────────────────────────────────────────────────────────────────────────────

/// Push `s` into `out` as a JSON string literal, properly escaping
/// `"`, `\`, and control characters (0x00–0x1f).  The surrounding `"…"` are
/// included in the output — this is the complete JSON string token.
fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Other control characters: \uXXXX
                let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Write a JSON object `{col1:val1, col2:val2, …}` for `row` directly
/// into `out`, with no intermediate `serde_json::Value` allocation.
///
/// Mirrors the `serde_json` output of the old `row_to_json` exactly:
/// * `Literal::Json(s)` is embedded as-is (already valid JSON).
/// * `Literal::Float(f)` with a non-finite value becomes `null` (same as
///   `Number::from_f64(f).map(…).unwrap_or(JsonValue::Null)`).
pub fn write_row_json(out: &mut String, row: &[Literal], columns: &[ColumnDef]) {
    out.push('{');
    let mut first = true;
    for (col, val) in columns.iter().zip(row) {
        if !first {
            out.push(',');
        }
        first = false;
        push_json_str(out, &col.name);
        out.push(':');
        match val {
            Literal::Null => out.push_str("null"),
            Literal::Int(n) => {
                let _ = std::fmt::Write::write_fmt(out, format_args!("{n}"));
            }
            Literal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Literal::Text(s) => push_json_str(out, s),
            Literal::Json(s) => out.push_str(s),
            Literal::Vector(v) => {
                out.push('[');
                for (i, f) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    // NaN / ±Inf → null, matching serde_json's
                    // `Number::from_f64` behaviour.
                    if f.is_finite() {
                        let _ = std::fmt::Write::write_fmt(out, format_args!("{}", f));
                    } else {
                        out.push_str("null");
                    }
                }
                out.push(']');
            }
            Literal::Decimal(value, scale) => {
                push_json_str(out, &crate::sql::logical::format_decimal(*value, *scale));
            }
            Literal::Timestamp(micros) => {
                push_json_str(out, &crate::sql::datetime::format_timestamp(*micros));
            }
            Literal::Float(f) => {
                if f.is_finite() {
                    let _ = std::fmt::Write::write_fmt(out, format_args!("{}", f));
                } else {
                    out.push_str("null");
                }
            }
            Literal::Uuid(b) => {
                push_json_str(out, &crate::sql::executor::format_uuid(b));
            }
            Literal::Bytea(b) => {
                push_json_str(out, &crate::sql::executor::format_bytea(b));
            }
            Literal::Date(d) => {
                push_json_str(out, &crate::sql::datetime::format_date(*d));
            }
            Literal::Time(t) => {
                push_json_str(out, &crate::sql::datetime::format_time(*t));
            }
            Literal::Param(_) => out.push_str("null"),
        }
    }
    out.push('}');
}

/// Build the canonical CDC envelope JSON string directly into a new `String`.
///
/// Output structure (identical to the old `serde_json::json!` macro call):
/// ```json
/// {
///   "payload": <compat: after ?? before ?? null>,
///   "before":  <before or null>,
///   "after":   <after or null>,
///   "ts_ms":   <ts_ms>,
///   "source":  {"seq":<seq>,"txId":<xid>,"table":"<table>","schema":"public"}
/// }
/// ```
///
/// No intermediate `serde_json::Value` is ever allocated.
pub fn build_event_envelope_str(
    op: &str,
    table: &str,
    before: Option<(&[Literal], &[ColumnDef])>,
    after: Option<(&[Literal], &[ColumnDef])>,
    ts_ms: i64,
    seq: u64,
    xid: u64,
) -> String {
    // Capacity heuristic: 512 bytes handles a typical single-row event;
    // the vector path will grow as needed.
    let mut out = String::with_capacity(512);
    out.push_str("{\"payload\":");
    // compat payload = after ?? before ?? null
    match (after, before) {
        (Some((ar, ac)), _) => write_row_json(&mut out, ar, ac),
        (None, Some((br, bc))) => write_row_json(&mut out, br, bc),
        (None, None) => out.push_str("null"),
    }
    out.push_str(",\"before\":");
    match before {
        Some((br, bc)) => write_row_json(&mut out, br, bc),
        None => out.push_str("null"),
    }
    out.push_str(",\"after\":");
    match after {
        Some((ar, ac)) => write_row_json(&mut out, ar, ac),
        None => out.push_str("null"),
    }
    out.push_str(",\"ts_ms\":");
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{ts_ms}"));
    // "source" object — same fields as the serde_json macro produced.
    out.push_str(",\"source\":{\"seq\":");
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{seq}"));
    out.push_str(",\"txId\":");
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{xid}"));
    out.push_str(",\"table\":");
    push_json_str(&mut out, table);
    out.push_str(",\"schema\":\"public\"}}");
    // Ignore `op` here — it is stored as a separate column in __events__,
    // not inside the envelope.  (The old code also did not embed `op` inside
    // the serde_json object — `op` was passed as a separate argument to
    // `event_row`.)
    let _ = op; // suppress unused-variable warning
    out
}

// ──────────────────────────────────────────────────────────────────────────────
// Legacy path kept for callers outside the hot CDC path (server/dto.rs etc.)
// ──────────────────────────────────────────────────────────────────────────────

/// Build a JSON object keyed by column name from a decoded row. `Json`
/// columns are parsed and embedded as a nested value, not double-encoded as
/// a string — a consumer reading `payload.data.status` shouldn't have to
/// parse a string-within-a-string.
///
/// **Prefer `write_row_json` on the hot path** (item 60) — this function
/// still allocates a `serde_json::Value` tree and is kept only for callers
/// that need a `JsonValue` (e.g. REST DTO serialisation).
pub fn row_to_json(row: &[Literal], columns: &[ColumnDef]) -> JsonValue {
    let mut map = Map::with_capacity(columns.len());
    for (col, val) in columns.iter().zip(row) {
        let json_val = match val {
            Literal::Null => JsonValue::Null,
            Literal::Int(n) => JsonValue::Number(Number::from(*n)),
            Literal::Text(s) => JsonValue::String(s.clone()),
            Literal::Bool(b) => JsonValue::Bool(*b),
            Literal::Json(s) => serde_json::from_str(s).unwrap_or(JsonValue::Null),
            Literal::Vector(v) => JsonValue::Array(
                v.iter()
                    .map(|f| {
                        Number::from_f64(*f as f64)
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            ),
            // Exact types render as strings so no precision is lost crossing
            // into JSON (P2.a): a `DECIMAL` as its canonical decimal text, a
            // `TIMESTAMP` as canonical UTC `YYYY-MM-DD HH:MM:SS[.ffffff]`.
            Literal::Decimal(value, scale) => {
                JsonValue::String(crate::sql::logical::format_decimal(*value, *scale))
            }
            Literal::Timestamp(micros) => {
                JsonValue::String(crate::sql::datetime::format_timestamp(*micros))
            }
            // P2.b scalar types.
            Literal::Float(f) => Number::from_f64(*f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            Literal::Uuid(b) => JsonValue::String(crate::sql::executor::format_uuid(b)),
            Literal::Bytea(b) => JsonValue::String(crate::sql::executor::format_bytea(b)),
            Literal::Date(d) => JsonValue::String(crate::sql::datetime::format_date(*d)),
            Literal::Time(t) => JsonValue::String(crate::sql::datetime::format_time(*t)),
            // Bind placeholders are substituted before execution (P2.e); a
            // stored row can never contain one.
            Literal::Param(_) => JsonValue::Null,
        };
        map.insert(col.name.clone(), json_val);
    }
    JsonValue::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnType;

    fn col(name: &str, ty: ColumnType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty,
            index: None,
            index_root: None,
            unique_index_root: None,
            dropped: false,
            constraints: Default::default(),
            include_cols: Vec::new(),
        }
    }

    #[test]
    fn null_becomes_json_null() {
        let columns = vec![col("a", ColumnType::Int64)];
        let row = vec![Literal::Null];
        assert_eq!(row_to_json(&row, &columns)["a"], JsonValue::Null);
    }

    #[test]
    fn int_becomes_json_number() {
        let columns = vec![col("a", ColumnType::Int64)];
        let row = vec![Literal::Int(42)];
        assert_eq!(row_to_json(&row, &columns)["a"], serde_json::json!(42));
    }

    #[test]
    fn text_becomes_json_string() {
        let columns = vec![col("a", ColumnType::Text)];
        let row = vec![Literal::Text("hi".to_string())];
        assert_eq!(row_to_json(&row, &columns)["a"], serde_json::json!("hi"));
    }

    #[test]
    fn bool_becomes_json_bool() {
        let columns = vec![col("a", ColumnType::Bool)];
        let row = vec![Literal::Bool(true)];
        assert_eq!(row_to_json(&row, &columns)["a"], serde_json::json!(true));
    }

    #[test]
    fn json_is_embedded_not_double_encoded() {
        let columns = vec![col("a", ColumnType::Json)];
        let row = vec![Literal::Json(r#"{"status":"active"}"#.to_string())];
        let out = row_to_json(&row, &columns);
        assert_eq!(out["a"]["status"], serde_json::json!("active"));
    }

    #[test]
    fn vector_becomes_json_array() {
        let columns = vec![col("a", ColumnType::Vector(3))];
        let row = vec![Literal::Vector(vec![1.0, 2.5, -3.0])];
        let out = row_to_json(&row, &columns);
        assert_eq!(out["a"], serde_json::json!([1.0, 2.5, -3.0]));
    }

    #[test]
    fn mixed_column_row() {
        let columns = vec![
            col("id", ColumnType::Int64),
            col("name", ColumnType::Text),
            col("active", ColumnType::Bool),
        ];
        let row = vec![
            Literal::Int(1),
            Literal::Text("alice".to_string()),
            Literal::Bool(false),
        ];
        let out = row_to_json(&row, &columns);
        assert_eq!(out["id"], serde_json::json!(1));
        assert_eq!(out["name"], serde_json::json!("alice"));
        assert_eq!(out["active"], serde_json::json!(false));
    }

    // ── item 60: write_row_json tests ────────────────────────────────────────

    #[test]
    fn write_row_json_null() {
        let columns = vec![col("a", ColumnType::Int64)];
        let row = vec![Literal::Null];
        let mut out = String::new();
        write_row_json(&mut out, &row, &columns);
        let v: JsonValue = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], JsonValue::Null);
    }

    #[test]
    fn write_row_json_int() {
        let columns = vec![col("id", ColumnType::Int64)];
        let row = vec![Literal::Int(42)];
        let mut out = String::new();
        write_row_json(&mut out, &row, &columns);
        let v: JsonValue = serde_json::from_str(&out).unwrap();
        assert_eq!(v["id"], serde_json::json!(42));
    }

    #[test]
    fn write_row_json_text_with_special_chars() {
        let columns = vec![col("s", ColumnType::Text)];
        let row = vec![Literal::Text("say \"hello\"\nworld".to_string())];
        let mut out = String::new();
        write_row_json(&mut out, &row, &columns);
        let v: JsonValue = serde_json::from_str(&out).unwrap();
        assert_eq!(v["s"], serde_json::json!("say \"hello\"\nworld"));
    }

    #[test]
    fn write_row_json_bool() {
        let columns = vec![col("flag", ColumnType::Bool)];
        for b in [true, false] {
            let row = vec![Literal::Bool(b)];
            let mut out = String::new();
            write_row_json(&mut out, &row, &columns);
            let v: JsonValue = serde_json::from_str(&out).unwrap();
            assert_eq!(v["flag"], serde_json::json!(b));
        }
    }

    #[test]
    fn write_row_json_vector() {
        let columns = vec![col("v", ColumnType::Vector(3))];
        let row = vec![Literal::Vector(vec![1.0f32, 2.5, -3.0])];
        let mut out = String::new();
        write_row_json(&mut out, &row, &columns);
        let v: JsonValue = serde_json::from_str(&out).unwrap();
        // Parse back as f64 array and compare approximately.
        let arr = v["v"].as_array().unwrap();
        assert!((arr[0].as_f64().unwrap() - 1.0).abs() < 1e-5);
        assert!((arr[1].as_f64().unwrap() - 2.5).abs() < 1e-5);
        assert!((arr[2].as_f64().unwrap() - (-3.0)).abs() < 1e-5);
    }

    #[test]
    fn write_row_json_json_embedded() {
        let columns = vec![col("data", ColumnType::Json)];
        let row = vec![Literal::Json(r#"{"k":1}"#.to_string())];
        let mut out = String::new();
        write_row_json(&mut out, &row, &columns);
        let v: JsonValue = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data"]["k"], 1);
    }

    #[test]
    fn build_event_envelope_str_is_parseable_and_has_correct_fields() {
        let columns = vec![col("id", ColumnType::Int64), col("name", ColumnType::Text)];
        let row = vec![Literal::Int(7), Literal::Text("alice".into())];
        let envelope = build_event_envelope_str(
            "insert",
            "users",
            None,
            Some((&row, &columns)),
            1_700_000_000_000,
            42,
            99,
        );
        let v: JsonValue = serde_json::from_str(&envelope).unwrap();
        assert_eq!(v["payload"]["id"], 7);
        assert_eq!(v["after"]["name"], "alice");
        assert!(v["before"].is_null());
        assert_eq!(v["ts_ms"], 1_700_000_000_000i64);
        assert_eq!(v["source"]["seq"], 42);
        assert_eq!(v["source"]["txId"], 99);
        assert_eq!(v["source"]["table"], "users");
        assert_eq!(v["source"]["schema"], "public");
    }

    #[test]
    fn build_event_envelope_str_delete_has_before_only() {
        let columns = vec![col("id", ColumnType::Int64)];
        let row = vec![Literal::Int(5)];
        let envelope =
            build_event_envelope_str("delete", "t", Some((&row, &columns)), None, 0, 1, 1);
        let v: JsonValue = serde_json::from_str(&envelope).unwrap();
        assert_eq!(v["payload"]["id"], 5); // compat = before for DELETE
        assert_eq!(v["before"]["id"], 5);
        assert!(v["after"].is_null());
    }

    /// Item 60 correctness gate: verify the manual builder produces the same
    /// JSON *value* as the old `serde_json::json!` macro for a mixed row.
    #[test]
    fn envelope_str_matches_serde_json_macro_output() {
        use serde_json::json;
        let columns = vec![
            col("id", ColumnType::Int64),
            col("name", ColumnType::Text),
            col("active", ColumnType::Bool),
        ];
        let after_row = vec![
            Literal::Int(1),
            Literal::Text("bob".into()),
            Literal::Bool(true),
        ];
        let before_row = vec![
            Literal::Int(1),
            Literal::Text("alice".into()),
            Literal::Bool(false),
        ];

        // Old path
        let before_val = row_to_json(&before_row, &columns);
        let after_val = row_to_json(&after_row, &columns);
        let compat_val = after_val.clone();
        let ts_ms: i64 = 1_700_000_000_000;
        let seq: u64 = 7;
        let xid: u64 = 3;
        let old_envelope = json!({
            "payload": compat_val,
            "before": before_val,
            "after": after_val,
            "ts_ms": ts_ms,
            "source": {
                "seq": seq,
                "txId": xid,
                "table": "users",
                "schema": "public"
            }
        });

        // New path
        let new_str = build_event_envelope_str(
            "update",
            "users",
            Some((&before_row, &columns)),
            Some((&after_row, &columns)),
            ts_ms,
            seq,
            xid,
        );
        let new_val: JsonValue = serde_json::from_str(&new_str).unwrap();

        assert_eq!(
            old_envelope, new_val,
            "manual builder must produce identical JSON to serde_json::json!"
        );
    }
}
