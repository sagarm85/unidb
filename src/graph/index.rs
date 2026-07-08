// Edge-list index (M3.a): `from_id -> [RowId]`, maintained synchronously
// inline — unlike M2's `VectorIndex`, this does NOT use a background
// worker. A `HashMap`/`Vec` insert is O(1) amortized; M2's worker exists
// specifically because HNSW graph construction is O(n log n) per upsert,
// a cost this index simply doesn't have. No channel, no thread, no
// `IndexStatus::Building`/`Ready`.
//
// Like `VectorIndex`, this is a candidate-fetcher, not a source of truth:
// every traversal must re-resolve candidates through `Heap::get` + the
// caller's MVCC snapshot (see `Engine::edges_from` in `lib.rs`), which is
// what keeps an aborted edge creation from ever leaking into a query
// result even though the index may already reference it. Unlike
// `VectorIndex` (M2's known tech debt: no cleanup on update, since
// `instant-distance` made it awkward), this index cleanly removes entries
// on delete — a plain `Vec::retain` — so M3 does not repeat that gap.

use std::collections::HashMap;

use crate::{
    bufferpool::BufferPool,
    catalog::ColumnDef,
    error::Result,
    format::{PageId, Xid},
    heap::RowId,
    mvcc::{is_visible, Snapshot},
    sql::{executor::decode_row, logical::Literal},
};

#[derive(Default)]
pub struct EdgeIndex {
    by_from: HashMap<i64, Vec<RowId>>,
}

impl EdgeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, from_id: i64, row_id: RowId) {
        self.by_from.entry(from_id).or_default().push(row_id);
    }

    pub fn remove(&mut self, from_id: i64, row_id: RowId) {
        if let Some(list) = self.by_from.get_mut(&from_id) {
            list.retain(|&existing| existing != row_id);
            if list.is_empty() {
                self.by_from.remove(&from_id);
            }
        }
    }

    /// Remove every entry pointing at `row_id`, regardless of which `from_id`
    /// bucket holds it (M10.c index vacuum). Unlike `remove`, the caller need
    /// not know the edge's `from_id` — vacuum only has the physical `RowId` of
    /// a reclaimed tuple. This is the load-bearing fix for the vacuum aliasing
    /// hazard: an aborted `create_edge` leaves a stale `from_id -> RowId` entry
    /// (abort has no index hook), which becomes a *wrong-but-visible* result
    /// the moment that slot is reused, unless it is scrubbed here first.
    /// Returns `true` if any entry was removed.
    pub fn remove_rowid(&mut self, row_id: RowId) -> bool {
        let mut removed = false;
        self.by_from.retain(|_, list| {
            let before = list.len();
            list.retain(|&existing| existing != row_id);
            removed |= list.len() != before;
            !list.is_empty()
        });
        removed
    }

    pub fn candidates(&self, from_id: i64) -> &[RowId] {
        self.by_from.get(&from_id).map_or(&[], |v| v.as_slice())
    }

    pub fn len(&self) -> usize {
        self.by_from.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_from.is_empty()
    }
}

// M7 originally added `graph_candidates`, preferring the CSR graph index
// once `IndexStatus::Ready` and falling back to `EdgeIndex` otherwise. That
// was a real correctness bug, found during M8 merge verification, not
// shipped as-is: `Ready` only means "the initial backfill completed" (for
// a fresh/empty table that's true almost instantly) — it does NOT mean
// "every edge written since then has been incorporated into a rebuild,"
// since CSR's rebuild is debounced/async (see `index_worker.rs`'s module
// doc). A query immediately after `create_edge` could observe `Ready` with
// a CSR structure that hasn't yet staged *that very edge*, silently
// returning fewer edges than actually exist — a real regression against
// the pre-M7 guarantee that `edges_from` sees a just-created edge
// immediately (`tests/graph_mvcc.rs::
// aborted_edge_creation_never_surfaces_in_traversal`, an M3 test with no
// retry loop, failed consistently once this was wired in and tested in
// isolation from the rest of the suite). `EdgeIndex` is synchronous and
// always current, so `edges_from`/the Cypher executor now use it directly
// again, unconditionally. `CsrIndex` itself remains correct, tested, and
// benchmarked (`src/csr_index.rs`, `benches/graph.rs`) and is still kept
// warm by every live edge write (`Engine::create_edge`) and rebuilt on
// open (`rebuild_csr_index` in `lib.rs`) — it just isn't safely
// consultable from this path yet. A future fix would need a staleness/
// generation marker proving CSR has incorporated every write up to a
// specific point before it can be trusted here.

/// Resolve a batch of candidate `RowId`s to their decoded rows, filtered by
/// MVCC visibility. Groups candidates by `page_id` so each distinct page is
/// fetched/decoded/unpinned once via `BufferPool::fetch_page` — not once
/// per candidate — since `fetch_page` copies the page out on every call,
/// even for an already-resident one (see MEMORY.md's M3.b design note for
/// the measured effect on hot-hub adjacency scans). Output order is by
/// page grouping, not candidate order — callers needing a specific order
/// must sort afterward.
pub fn resolve_candidates_batched(
    candidates: &[RowId],
    snapshot: &Snapshot,
    self_xid: Xid,
    pool: &mut BufferPool,
    columns: &[ColumnDef],
) -> Result<Vec<(RowId, Vec<Literal>)>> {
    let mut by_page: HashMap<PageId, Vec<u16>> = HashMap::new();
    for c in candidates {
        by_page.entry(c.page_id).or_default().push(c.slot);
    }

    let mut out = Vec::new();
    for (page_id, slots) in by_page {
        let page = pool.fetch_page(page_id)?;
        for slot in slots {
            // Skip line pointers a vacuum has reclaimed (DEAD/UNUSED, M10): a
            // stale index candidate pointing at one carries no tuple to
            // resolve. (Correctly reused slots are handled by the index-vacuum
            // pass having removed such stale entries — see M10.c.)
            if !matches!(page.slot_state(slot), Ok(crate::page::SlotState::Live)) {
                continue;
            }
            let th = page.tuple_header(slot)?;
            if is_visible(th.xmin, th.xmax, snapshot, self_xid) {
                let bytes = page.get(slot)?.to_vec();
                let row = decode_row(&bytes, columns)?;
                out.push((RowId { page_id, slot }, row));
            }
        }
        pool.unpin(page_id);
    }
    Ok(out)
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
        let idx = EdgeIndex::new();
        assert!(idx.candidates(1).is_empty());
    }

    #[test]
    fn insert_and_lookup() {
        let mut idx = EdgeIndex::new();
        idx.insert(1, rid(0, 0));
        idx.insert(1, rid(0, 1));
        idx.insert(2, rid(0, 2));
        assert_eq!(idx.candidates(1), &[rid(0, 0), rid(0, 1)]);
        assert_eq!(idx.candidates(2), &[rid(0, 2)]);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn remove_drops_entry_and_cleans_up_empty_list() {
        let mut idx = EdgeIndex::new();
        idx.insert(1, rid(0, 0));
        idx.insert(1, rid(0, 1));
        idx.remove(1, rid(0, 0));
        assert_eq!(idx.candidates(1), &[rid(0, 1)]);
        idx.remove(1, rid(0, 1));
        assert!(idx.candidates(1).is_empty());
        assert!(idx.is_empty());
    }

    #[test]
    fn remove_on_unknown_from_id_is_a_no_op() {
        let mut idx = EdgeIndex::new();
        idx.remove(999, rid(0, 0));
        assert!(idx.is_empty());
    }

    #[test]
    fn remove_rowid_scrubs_entry_without_knowing_from_id() {
        let mut idx = EdgeIndex::new();
        idx.insert(100, rid(3, 0));
        idx.insert(200, rid(3, 1));
        // Vacuum only has the physical RowId of the reclaimed tuple.
        assert!(idx.remove_rowid(rid(3, 0)));
        assert!(idx.candidates(100).is_empty());
        assert_eq!(idx.candidates(200), &[rid(3, 1)]);
        // Removing a RowId that isn't present returns false.
        assert!(!idx.remove_rowid(rid(9, 9)));
    }
}
