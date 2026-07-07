// CSR (Compressed Sparse Row) graph adjacency index (M7): a read-optimized
// alternative to `EdgeIndex`'s `HashMap<i64, Vec<RowId>>` (`src/graph/
// index.rs`), built asynchronously on the background worker exactly like
// HNSW (`index_worker.rs`'s existing machinery) — CSR is a sorted,
// offset-array structure that cannot be incrementally patched (analogous
// to `instant-distance`'s HNSW having no incremental insert — see
// `vector.rs`'s module doc), so it's rebuilt from scratch, not updated in
// place. Unlike `VectorIndex` (which rebuilds once per upsert, a known,
// still-unfixed scaling limitation), the worker *debounces* CSR rebuilds —
// see `index_worker.rs`'s `worker_loop` — coalescing a burst of queued
// edge writes into one rebuild pass rather than one per edge.
//
// `EdgeIndex` remains the synchronous, always-current, always-available
// structure `create_edge`/`delete_edge` maintain inline; `CsrIndex` is a
// purely additional, async-built accelerant consulted only once `Ready`
// (see `graph/index.rs::graph_candidates`) — never a replacement, and
// losing it changes nothing but performance (rebuilt from `__edges__` on
// next open, same as every other secondary index).

use crate::heap::RowId;

#[derive(Default)]
pub struct CsrIndex {
    /// Raw, append-only accumulator of every edge staged so far — the
    /// source `rebuild` recomputes the queryable arrays from. Never
    /// queried directly.
    edges: Vec<(i64, RowId)>,
    from_ids_sorted: Vec<i64>,
    row_ptr: Vec<usize>,
    col_ind: Vec<RowId>,
    /// Test-only observability: how many times `rebuild` has actually run
    /// — lets `index_worker.rs`'s debounce test prove a burst of staged
    /// edges collapses into far fewer rebuild passes than messages sent,
    /// without needing to intercept the worker thread itself.
    rebuild_count: usize,
}

impl CsrIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one more edge without touching the queryable structure yet —
    /// see module doc: the worker coalesces a burst of `stage` calls into
    /// one `rebuild`.
    pub fn stage(&mut self, from_id: i64, row_id: RowId) {
        self.edges.push((from_id, row_id));
    }

    /// Recompute the CSR arrays from every staged edge so far. O(n log n)
    /// in the total edge count — not incrementally patchable (see module
    /// doc), same "rebuild the whole thing" tradeoff `VectorIndex` accepts
    /// for HNSW, but debounced rather than triggered per edge.
    pub fn rebuild(&mut self) {
        let mut sorted = self.edges.clone();
        sorted.sort_by_key(|(from_id, _)| *from_id);

        let mut from_ids_sorted = Vec::new();
        let mut row_ptr = vec![0usize];
        let mut col_ind = Vec::with_capacity(sorted.len());
        for (from_id, row_id) in sorted {
            if from_ids_sorted.last() != Some(&from_id) {
                from_ids_sorted.push(from_id);
                row_ptr.push(*row_ptr.last().expect("row_ptr always has >=1 element"));
            }
            col_ind.push(row_id);
            *row_ptr.last_mut().expect("row_ptr always has >=1 element") += 1;
        }
        self.from_ids_sorted = from_ids_sorted;
        self.row_ptr = row_ptr;
        self.col_ind = col_ind;
        self.rebuild_count += 1;
    }

    /// How many times `rebuild` has run — test-only observability, see
    /// the field's doc comment.
    pub fn rebuild_count(&self) -> usize {
        self.rebuild_count
    }

    /// Candidate `RowId`s for `from_id`, via binary search into the sorted
    /// `from_id` array — `&[]` if `from_id` has no edges (or none existed
    /// at the time of the last `rebuild`). A pure candidate-fetcher, same
    /// as `EdgeIndex`/`VectorIndex` — callers must re-validate every
    /// candidate against MVCC visibility (see `graph/index.rs::
    /// resolve_candidates_batched`).
    pub fn candidates(&self, from_id: i64) -> &[RowId] {
        match self.from_ids_sorted.binary_search(&from_id) {
            Ok(idx) => &self.col_ind[self.row_ptr[idx]..self.row_ptr[idx + 1]],
            Err(_) => &[],
        }
    }

    pub fn len(&self) -> usize {
        self.col_ind.len()
    }

    pub fn is_empty(&self) -> bool {
        self.col_ind.is_empty()
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
    fn empty_index_has_no_candidates() {
        let idx = CsrIndex::new();
        assert!(idx.candidates(1).is_empty());
    }

    #[test]
    fn candidates_before_any_rebuild_are_empty_even_if_staged() {
        let mut idx = CsrIndex::new();
        idx.stage(1, rid(0, 0));
        assert!(
            idx.candidates(1).is_empty(),
            "stage alone must not populate the queryable structure"
        );
    }

    #[test]
    fn rebuild_groups_by_from_id_correctly() {
        let mut idx = CsrIndex::new();
        idx.stage(2, rid(0, 2));
        idx.stage(1, rid(0, 0));
        idx.stage(1, rid(0, 1));
        idx.rebuild();

        let mut c1 = idx.candidates(1).to_vec();
        c1.sort_by_key(|r| r.slot);
        assert_eq!(c1, vec![rid(0, 0), rid(0, 1)]);
        assert_eq!(idx.candidates(2), &[rid(0, 2)]);
        assert!(idx.candidates(999).is_empty());
        assert_eq!(idx.len(), 3);
        assert!(!idx.is_empty());
    }

    #[test]
    fn rebuild_after_more_staging_reflects_new_edges() {
        let mut idx = CsrIndex::new();
        idx.stage(1, rid(0, 0));
        idx.rebuild();
        assert_eq!(idx.candidates(1), &[rid(0, 0)]);

        idx.stage(1, rid(0, 1));
        idx.stage(2, rid(0, 2));
        // Deliberately not rebuilt yet — the queryable structure must not
        // change until `rebuild` runs (this is the whole point of
        // debouncing: staged-but-not-yet-rebuilt edges are invisible).
        assert_eq!(idx.candidates(1), &[rid(0, 0)]);
        assert!(idx.candidates(2).is_empty());

        idx.rebuild();
        assert_eq!(idx.candidates(1), &[rid(0, 0), rid(0, 1)]);
        assert_eq!(idx.candidates(2), &[rid(0, 2)]);
    }
}
