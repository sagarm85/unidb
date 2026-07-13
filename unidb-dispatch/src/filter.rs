//! Per-subscription filtering + projection (item 20, E2). **The engine emits
//! raw row-level facts and never transforms them** (Milestone-18 boundary);
//! all narrowing happens here, consumer-side. A `Filter` selects which events a
//! subscription cares about (by table and op kind) and optionally projects the
//! payload down to a column subset before the event reaches a sink.

use unidb::queue::Event;

/// Which events a subscription receives, and how much of each.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only events for these tables. Empty ⇒ every table.
    pub tables: Vec<String>,
    /// Only these op kinds (`"insert"`/`"update"`/`"delete"`). Empty ⇒ all.
    pub ops: Vec<String>,
    /// If set, project each event's `payload` object down to just these
    /// columns. `None` ⇒ deliver the full row image untouched.
    pub columns: Option<Vec<String>>,
}

impl Filter {
    /// A pass-through filter: every event, full payload.
    pub fn all() -> Self {
        Self::default()
    }

    /// Restrict to a single table.
    pub fn table(name: impl Into<String>) -> Self {
        Self {
            tables: vec![name.into()],
            ..Self::default()
        }
    }

    /// Chain an op-kind restriction.
    pub fn ops(mut self, ops: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.ops = ops.into_iter().map(Into::into).collect();
        self
    }

    /// Chain a column projection.
    pub fn project(mut self, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.columns = Some(columns.into_iter().map(Into::into).collect());
        self
    }

    /// Does this event pass the table/op predicate?
    pub fn matches(&self, event: &Event) -> bool {
        if !self.tables.is_empty() && !self.tables.iter().any(|t| t == &event.table_name) {
            return false;
        }
        if !self.ops.is_empty() && !self.ops.iter().any(|o| o == &event.op) {
            return false;
        }
        true
    }

    /// Apply the column projection (if any), returning the event a sink sees.
    /// Cloning is deliberate: one raw event fans out to many subscriptions,
    /// each of which may project differently, so the source event is never
    /// mutated.
    pub fn apply(&self, event: &Event) -> Event {
        let mut out = event.clone();
        if let (Some(cols), serde_json::Value::Object(map)) = (&self.columns, &event.payload) {
            let projected: serde_json::Map<String, serde_json::Value> = cols
                .iter()
                .filter_map(|c| map.get(c).map(|v| (c.clone(), v.clone())))
                .collect();
            out.payload = serde_json::Value::Object(projected);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(table: &str, op: &str, payload: serde_json::Value) -> Event {
        Event {
            seq: 1,
            xid: 1,
            table_name: table.to_string(),
            op: op.to_string(),
            payload,
            before: None,
            after: None,
            ts_ms: 0,
        }
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = Filter::all();
        assert!(f.matches(&ev("t", "insert", serde_json::json!({}))));
        assert!(f.matches(&ev("other", "delete", serde_json::json!({}))));
    }

    #[test]
    fn table_and_op_predicates() {
        let f = Filter::table("orders").ops(["insert", "update"]);
        assert!(f.matches(&ev("orders", "insert", serde_json::json!({}))));
        assert!(!f.matches(&ev("orders", "delete", serde_json::json!({}))));
        assert!(!f.matches(&ev("users", "insert", serde_json::json!({}))));
    }

    #[test]
    fn projection_narrows_payload() {
        let f = Filter::all().project(["id"]);
        let out = f.apply(&ev(
            "t",
            "insert",
            serde_json::json!({"id": 7, "secret": "x"}),
        ));
        assert_eq!(out.payload, serde_json::json!({"id": 7}));
    }

    #[test]
    fn projection_missing_column_is_dropped_not_null() {
        let f = Filter::all().project(["id", "absent"]);
        let out = f.apply(&ev("t", "insert", serde_json::json!({"id": 7})));
        assert_eq!(out.payload, serde_json::json!({"id": 7}));
    }
}
