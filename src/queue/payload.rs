// Row -> JSON payload conversion (M4.a). New code: `encode_row`/`decode_row`
// (sql/executor.rs) go to/from the hand-rolled on-disk binary format, not
// JSON — this is the only place a `Vec<Literal>` becomes a `serde_json::
// Value`, for `__events__`'s `payload` column.

use serde_json::{Map, Number, Value as JsonValue};

use crate::{catalog::ColumnDef, sql::logical::Literal};

/// Build a JSON object keyed by column name from a decoded row. `Json`
/// columns are parsed and embedded as a nested value, not double-encoded as
/// a string — a consumer reading `payload.data.status` shouldn't have to
/// parse a string-within-a-string.
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
            constraints: Default::default(),
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
}
