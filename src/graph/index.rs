// Edge adjacency resolution (M3.a; the index itself became durable in P3.b).
//
// Since P3.b the `from_id -> [RowId]` adjacency index is a **durable on-disk
// B+tree** (`DiskBTree`) over `__edges__.from_id` — no longer an in-memory
// `HashMap` rebuilt on open. Its meta page is stored in that column's
// `ColumnDef.index_root` (created by `graph::edges::ensure_edge_index`), so it
// is crash-recovered and never rebuilt; `Engine::create_edge`/`delete_edge`
// maintain it, and `edges_from`/the Cypher executor read it via
// `DiskBTree::search_eq(OrderedValue::Int(from_id))`. It remains a
// candidate-fetcher, not a source of truth: every traversal re-resolves
// candidates through the caller's MVCC snapshot below, so an aborted edge
// creation never surfaces even though the index may still reference it.
//
// What survives here is `resolve_candidates_batched` — the batch-latch
// adjacency resolver (M3.b) that turns a list of candidate `RowId`s into
// decoded, MVCC-visible rows, grouping by page so each page is fetched once.

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
