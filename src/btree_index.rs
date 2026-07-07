// B-Tree secondary index (M6): accelerates equality/range WHERE predicates
// on Int64/Text/Bool columns. API shape mirrors `VectorIndex`/`InvertedIndex`
// (new/upsert/remove/search.../len/is_empty) so it plugs into
// `index_worker.rs`'s existing generic async-worker machinery unchanged.
//
// Unlike HNSW, `BTreeMap` supports true incremental insert/remove — no
// rebuild-the-whole-structure tax the way `instant-distance`'s HNSW needs
// (see `vector.rs`'s module doc). `upsert` still needs to track each row's
// prior indexed value internally (`by_id`) so an UPDATE that changes the
// indexed column's value removes the stale bucket entry, not just adds a
// new one — the same "insert or overwrite by RowId" contract `VectorIndex`/
// `InvertedIndex` already provide, just implemented differently since a
// BTreeMap is keyed by value, not by RowId.

use std::collections::{BTreeMap, HashMap};

use crate::{
    error::{DbError, Result},
    heap::RowId,
    sql::logical::{CmpOp, Literal},
};

/// A `Literal` projected down to the subset that's `Ord` — `Vector`/`Json`/
/// `Null` have no meaningful total order for indexing purposes and are
/// rejected at `CREATE INDEX` validation time (`sql/executor.rs::
/// exec_create_index`), so this conversion failing here would indicate a
/// bug upstream, not a normal runtime condition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum OrderedValue {
    Int(i64),
    Text(String),
    Bool(bool),
}

impl TryFrom<&Literal> for OrderedValue {
    type Error = DbError;

    fn try_from(lit: &Literal) -> Result<Self> {
        match lit {
            Literal::Int(n) => Ok(OrderedValue::Int(*n)),
            Literal::Text(s) => Ok(OrderedValue::Text(s.clone())),
            Literal::Bool(b) => Ok(OrderedValue::Bool(*b)),
            other => Err(DbError::SqlUnsupported(format!(
                "{other:?} is not orderable for a BTree index"
            ))),
        }
    }
}

/// The four range comparators a `BTreeMap` can serve directly via
/// `range()`. `Eq` is handled separately by `search_eq` (a plain map
/// lookup, not a range scan); `Ne` has no compact range representation and
/// is intentionally not representable here — see `search`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOp {
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Default)]
pub struct BTreeIndex {
    by_value: BTreeMap<OrderedValue, Vec<RowId>>,
    by_id: HashMap<RowId, OrderedValue>,
}

impl BTreeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite `id`'s indexed value. If `id` was previously
    /// indexed under a different value, that stale bucket entry is removed
    /// first — otherwise a row whose indexed column changes would leave a
    /// phantom candidate behind forever (harmless for correctness, since
    /// every candidate is re-validated against MVCC visibility downstream
    /// in `exec_select`, but wasteful and untidy).
    pub fn upsert(&mut self, id: RowId, value: OrderedValue) {
        if let Some(old) = self.by_id.get(&id) {
            if *old == value {
                return;
            }
            let old = old.clone();
            Self::remove_from_bucket(&mut self.by_value, &old, id);
        }
        self.by_value.entry(value.clone()).or_default().push(id);
        self.by_id.insert(id, value);
    }

    pub fn remove(&mut self, id: RowId) {
        if let Some(value) = self.by_id.remove(&id) {
            Self::remove_from_bucket(&mut self.by_value, &value, id);
        }
    }

    fn remove_from_bucket(
        map: &mut BTreeMap<OrderedValue, Vec<RowId>>,
        value: &OrderedValue,
        id: RowId,
    ) {
        if let Some(ids) = map.get_mut(value) {
            ids.retain(|&existing| existing != id);
            if ids.is_empty() {
                map.remove(value);
            }
        }
    }

    /// Exact-match candidates.
    pub fn search_eq(&self, value: &OrderedValue) -> Vec<RowId> {
        self.by_value.get(value).cloned().unwrap_or_default()
    }

    /// Range-match candidates.
    pub fn search_range(&self, op: RangeOp, value: &OrderedValue) -> Vec<RowId> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let range = match op {
            RangeOp::Lt => (Unbounded, Excluded(value)),
            RangeOp::Le => (Unbounded, Included(value)),
            RangeOp::Gt => (Excluded(value), Unbounded),
            RangeOp::Ge => (Included(value), Unbounded),
        };
        self.by_value
            .range(range)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect()
    }

    /// Dispatch a `sql::logical::CmpOp` to the right lookup. Returns `None`
    /// for `Ne` — there's no compact range representation for "not equal
    /// to," so the caller (`exec_select`) must treat `None` as "this index
    /// can't help, fall back to a full scan," never as "zero candidates."
    pub fn search(&self, op: CmpOp, value: &OrderedValue) -> Option<Vec<RowId>> {
        match op {
            CmpOp::Eq => Some(self.search_eq(value)),
            CmpOp::Lt => Some(self.search_range(RangeOp::Lt, value)),
            CmpOp::Le => Some(self.search_range(RangeOp::Le, value)),
            CmpOp::Gt => Some(self.search_range(RangeOp::Gt, value)),
            CmpOp::Ge => Some(self.search_range(RangeOp::Ge, value)),
            CmpOp::Ne => None,
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(page: u32, slot: u16) -> RowId {
        RowId {
            page_id: page,
            slot,
        }
    }

    #[test]
    fn ordered_value_rejects_non_orderable_literals() {
        assert!(OrderedValue::try_from(&Literal::Vector(vec![0.1])).is_err());
        assert!(OrderedValue::try_from(&Literal::Json("{}".into())).is_err());
        assert!(OrderedValue::try_from(&Literal::Null).is_err());
        assert_eq!(
            OrderedValue::try_from(&Literal::Int(5)).unwrap(),
            OrderedValue::Int(5)
        );
    }

    #[test]
    fn search_eq_finds_exact_matches_only() {
        let mut idx = BTreeIndex::new();
        idx.upsert(rid(1, 0), OrderedValue::Int(5));
        idx.upsert(rid(2, 0), OrderedValue::Int(7));
        idx.upsert(rid(3, 0), OrderedValue::Int(5));

        let mut got = idx.search_eq(&OrderedValue::Int(5));
        got.sort_by_key(|r| r.page_id);
        assert_eq!(got, vec![rid(1, 0), rid(3, 0)]);
        assert!(idx.search_eq(&OrderedValue::Int(99)).is_empty());
    }

    #[test]
    fn search_range_covers_lt_le_gt_ge() {
        let mut idx = BTreeIndex::new();
        for i in 1..=5 {
            idx.upsert(rid(i, 0), OrderedValue::Int(i as i64));
        }
        assert_eq!(
            idx.search_range(RangeOp::Lt, &OrderedValue::Int(3)).len(),
            2
        );
        assert_eq!(
            idx.search_range(RangeOp::Le, &OrderedValue::Int(3)).len(),
            3
        );
        assert_eq!(
            idx.search_range(RangeOp::Gt, &OrderedValue::Int(3)).len(),
            2
        );
        assert_eq!(
            idx.search_range(RangeOp::Ge, &OrderedValue::Int(3)).len(),
            3
        );
    }

    #[test]
    fn search_dispatches_by_cmpop_and_rejects_ne() {
        let mut idx = BTreeIndex::new();
        idx.upsert(rid(1, 0), OrderedValue::Int(1));
        assert!(idx.search(CmpOp::Eq, &OrderedValue::Int(1)).is_some());
        assert!(idx.search(CmpOp::Lt, &OrderedValue::Int(1)).is_some());
        assert!(idx.search(CmpOp::Ne, &OrderedValue::Int(1)).is_none());
    }

    #[test]
    fn upsert_changing_value_removes_stale_bucket_entry() {
        let mut idx = BTreeIndex::new();
        idx.upsert(rid(1, 0), OrderedValue::Int(1));
        idx.upsert(rid(1, 0), OrderedValue::Int(2));
        assert!(idx.search_eq(&OrderedValue::Int(1)).is_empty());
        assert_eq!(idx.search_eq(&OrderedValue::Int(2)), vec![rid(1, 0)]);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn remove_drops_id_from_its_bucket() {
        let mut idx = BTreeIndex::new();
        idx.upsert(rid(1, 0), OrderedValue::Int(1));
        idx.upsert(rid(2, 0), OrderedValue::Int(1));
        idx.remove(rid(1, 0));
        assert_eq!(idx.search_eq(&OrderedValue::Int(1)), vec![rid(2, 0)]);
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }
}
