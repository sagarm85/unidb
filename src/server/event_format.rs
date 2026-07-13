//! CDC envelope format adapters for `GET /events/subscribe?format=…` (item 29, C2).
//!
//! Three formats over the same canonical `Event` fields — pure serialization,
//! no storage interaction:
//!
//! - **native** (default): the full `Event` struct as JSON; `seq` is the SSE
//!   `id:` frame; downstream tools can use `before`/`after`/`ts_ms` directly.
//! - **debezium**: `{payload:{op,ts_ms,before,after,source:{seq,txId,table,schema}}}`
//!   with single-char op (`c`/`u`/`d`). Compatible with Kafka-Connect Debezium sinks.
//! - **supabase**: `{eventType,new,old,schema,table,commit_timestamp}` flat shape.
//!   Compatible with Supabase Realtime consumers.
//!
//! `seq` stays the offset/lag cursor in every format (the SSE `id:` frame).

use serde_json::json;

use crate::queue::Event;

/// Serialize `event` according to the requested format name.
/// Unknown format names fall back to "native".
pub fn format_event(event: &Event, format: &str) -> String {
    match format {
        "debezium" => format_debezium(event),
        "supabase" => format_supabase(event),
        _ => serde_json::to_string(event).unwrap_or_else(|_| "{}".into()),
    }
}

/// Debezium-compatible envelope.
///
/// ```json
/// {"payload":{"op":"u","ts_ms":…,"before":{…},"after":{…},
///             "source":{"seq":42,"txId":1017,"table":"orders","schema":"public"}}}
/// ```
fn format_debezium(event: &Event) -> String {
    let source = json!({
        "seq": event.seq,
        "txId": event.xid,
        "table": event.table_name,
        "schema": "public"
    });
    let value = json!({
        "payload": {
            "op": debezium_op(&event.op),
            "ts_ms": event.ts_ms,
            "before": event.before,
            "after": event.after,
            "source": source
        }
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
}

/// Supabase Realtime-compatible flat envelope.
///
/// ```json
/// {"eventType":"UPDATE","new":{…},"old":{…},
///  "schema":"public","table":"orders","commit_timestamp":"2026-07-13 12:00:00Z"}
/// ```
fn format_supabase(event: &Event) -> String {
    let commit_timestamp = if event.ts_ms > 0 {
        // ts_ms → microseconds → format_timestamp gives YYYY-MM-DD HH:MM:SS
        let ts_str = crate::sql::datetime::format_timestamp(event.ts_ms * 1000);
        // Replace the space separator with T and append Z for ISO 8601.
        ts_str.replacen(' ', "T", 1) + "Z"
    } else {
        String::new()
    };
    let value = json!({
        "eventType": supabase_event_type(&event.op),
        "new": event.after,
        "old": event.before,
        "schema": "public",
        "table": event.table_name,
        "commit_timestamp": commit_timestamp
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
}

fn debezium_op(op: &str) -> &'static str {
    match op {
        "insert" => "c",
        "update" => "u",
        "delete" => "d",
        _ => "u",
    }
}

fn supabase_event_type(op: &str) -> &'static str {
    match op {
        "insert" => "INSERT",
        "update" => "UPDATE",
        "delete" => "DELETE",
        _ => "UPDATE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::queue::Event;

    fn make_event(op: &str) -> Event {
        let before = if op == "insert" {
            None
        } else {
            Some(serde_json::json!({"id": 1, "name": "old"}))
        };
        let after = if op == "delete" {
            None
        } else {
            Some(serde_json::json!({"id": 1, "name": "new"}))
        };
        Event {
            seq: 42,
            xid: 1017,
            table_name: "orders".into(),
            op: op.into(),
            payload: after.clone().unwrap_or(before.clone().unwrap_or_default()),
            before,
            after,
            ts_ms: 1_752_000_000_000,
        }
    }

    #[test]
    fn native_serializes_full_event() {
        let ev = make_event("update");
        let s = format_event(&ev, "native");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["seq"], 42);
        assert_eq!(v["op"], "update");
        assert!(v.get("before").is_some());
        assert!(v.get("after").is_some());
        assert_eq!(v["ts_ms"], 1_752_000_000_000i64);
    }

    #[test]
    fn unknown_format_falls_back_to_native() {
        let ev = make_event("insert");
        let s = format_event(&ev, "kafka");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["seq"], 42);
    }

    #[test]
    fn debezium_insert_has_c_op_and_null_before() {
        let ev = make_event("insert");
        let s = format_event(&ev, "debezium");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["payload"]["op"], "c");
        assert!(v["payload"]["before"].is_null());
        assert_eq!(v["payload"]["after"]["name"], "new");
        assert_eq!(v["payload"]["source"]["seq"], 42);
        assert_eq!(v["payload"]["source"]["table"], "orders");
    }

    #[test]
    fn debezium_update_has_u_op_and_both_images() {
        let ev = make_event("update");
        let s = format_event(&ev, "debezium");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["payload"]["op"], "u");
        assert_eq!(v["payload"]["before"]["name"], "old");
        assert_eq!(v["payload"]["after"]["name"], "new");
    }

    #[test]
    fn debezium_delete_has_d_op_and_null_after() {
        let ev = make_event("delete");
        let s = format_event(&ev, "debezium");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["payload"]["op"], "d");
        assert_eq!(v["payload"]["before"]["name"], "old");
        assert!(v["payload"]["after"].is_null());
    }

    #[test]
    fn supabase_insert_has_insert_event_type() {
        let ev = make_event("insert");
        let s = format_event(&ev, "supabase");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["eventType"], "INSERT");
        assert_eq!(v["new"]["name"], "new");
        assert!(v["old"].is_null());
        assert_eq!(v["schema"], "public");
        assert_eq!(v["table"], "orders");
        assert!(!v["commit_timestamp"].as_str().unwrap().is_empty());
    }

    #[test]
    fn supabase_update_carries_both_images() {
        let ev = make_event("update");
        let s = format_event(&ev, "supabase");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["eventType"], "UPDATE");
        assert_eq!(v["new"]["name"], "new");
        assert_eq!(v["old"]["name"], "old");
    }

    #[test]
    fn supabase_delete_has_null_new() {
        let ev = make_event("delete");
        let s = format_event(&ev, "supabase");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["eventType"], "DELETE");
        assert!(v["new"].is_null());
        assert_eq!(v["old"]["name"], "old");
    }
}
