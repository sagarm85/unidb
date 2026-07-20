// Graph adjacency cache (item 95): per-engine, per-hub in-memory cache for
// 1-hop traversal.
//
// # Design
//
// Cold graph traversal hits the B-tree + heap on every `edges_from` /
// `execute_cypher` call — 2–4 page fetches for the index scan plus ~1 heap
// page per ~20 edges, totalling 2–10 µs warm and 50–200 µs cold.  For a hot
// hub (the same `from_id` queried repeatedly) this is pure overhead: the data
// does not change between reads but the disk path is paid every time.
//
// The cache holds a `Vec<EdgeRef>` per `(table, from_id)` key behind an
// `Arc`.  Callers receive a clone of the `Arc` and hold it independently
// of the map, so invalidation (remove on INSERT/DELETE) is O(1) under the
// DashMap shard lock and existing holders finish safely against a consistent
// snapshot of the old adjacency list — no stale reads, no torn entries.
//
// # Correctness contract
//
// The cache is populated **after** a commit (the cache entry is derived from
// the MVCC-resolved candidate set, not from raw heap tuples), and invalidated
// immediately before any mutation to the hub's adjacency list reaches the heap.
// Because mutations to `__edges__` are serialised by `write_serial` (see
// `Engine::create_edge`/`delete_edge`), the invalidate→write→cache-miss-on-next-
// read sequence is race-free: a concurrent reader that misses the now-gone
// cache entry falls back to the B-tree cold path and re-populates from the
// authoritative on-disk snapshot.
//
// Note: MVCC means the cache always stores the **committed** view of the hub.
// A read inside an open writer transaction that has not yet committed its own
// edge inserts will see a cache miss (the hub entry was invalidated on write)
// and fall back to the B-tree path, which correctly reflects the in-progress
// snapshot.  This is correct: the cache is an optimisation for the common
// read-mostly case; it does not affect isolation.
//
// # Memory budget
//
// `UNIDB_GRAPH_CACHE_HUBS` (default 50_000) caps the number of cached hubs.
// When the map reaches the cap, the LRU hub (oldest `last_used` timestamp) is
// evicted before inserting the new entry.  The timestamp is an `AtomicU64`
// monotonic counter derived from `EVICTION_CLOCK` — a process-wide atomic
// that increments on every cache hit and insert.  This is "approximate LRU":
// the eviction candidate is the minimum across a random sample of entries
// rather than a full scan, which keeps eviction O(1) and lock-free for the
// common case.  Setting `UNIDB_GRAPH_CACHE_HUBS=0` disables the cache
// entirely (all lookups return `None`; existing graph tests run unaffected).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

// ── Constants ────────────────────────────────────────────────────────────────

/// Default maximum number of hubs held in the cache.  Overridden by the
/// `UNIDB_GRAPH_CACHE_HUBS` environment variable (parsed once at engine open).
pub const DEFAULT_MAX_HUBS: usize = 50_000;

/// Number of random entries sampled during LRU eviction.
const EVICTION_SAMPLE: usize = 8;

// ── Eviction clock ───────────────────────────────────────────────────────────

/// Monotonic counter shared by all `AdjacencyCache` instances in a process.
/// Incremented on every cache hit and insert to give a cheap "access time".
static EVICTION_CLOCK: AtomicU64 = AtomicU64::new(1);

#[inline]
fn tick() -> u64 {
    EVICTION_CLOCK.fetch_add(1, Ordering::Relaxed)
}

// ── EdgeRef ──────────────────────────────────────────────────────────────────

/// A single cached outgoing edge.  Stores the minimum information needed to
/// answer a `to_id`-only projection without a heap fetch.
///
/// `props_inline` holds the raw props bytes when they are short enough to
/// cache (≤ 256 bytes after encoding); `None` signals "fall back to
/// `edge_row_id` for a heap fetch."  In practice almost all unidb props fit
/// within this budget, so the heap is rarely needed for a cached hub.
#[derive(Debug, Clone)]
pub struct EdgeRef {
    /// Destination node id (`to_id` column).
    pub to_id: i64,
    /// The heap row that backs this edge (used for heap re-fetch when props
    /// are absent or when the caller needs `edge_type`).
    pub edge_row_id: crate::heap::RowId,
    /// Edge type string, always inlined (typically short like "KNOWS").
    pub edge_type: String,
    /// Raw props payload, inlined when `props.len() <= PROPS_INLINE_LIMIT`.
    /// `None` when the props are large — the caller must heap-fetch via
    /// `edge_row_id`.
    pub props_inline: Option<Vec<u8>>,
}

/// Maximum props byte length to inline in the cache entry.  Props larger than
/// this are stored as `None` and require a heap fetch on every access; this
/// is a memory-safety cap, not a correctness gate.
pub const PROPS_INLINE_LIMIT: usize = 256;

// ── CacheEntry ───────────────────────────────────────────────────────────────

/// One hub's adjacency list plus its LRU access timestamp.
struct CacheEntry {
    edges: Arc<Vec<EdgeRef>>,
    last_used: AtomicU64,
}

impl CacheEntry {
    fn new(edges: Arc<Vec<EdgeRef>>) -> Self {
        Self {
            edges,
            last_used: AtomicU64::new(tick()),
        }
    }

    fn touch(&self) -> Arc<Vec<EdgeRef>> {
        self.last_used.store(tick(), Ordering::Relaxed);
        Arc::clone(&self.edges)
    }
}

// ── AdjacencyCache ───────────────────────────────────────────────────────────

/// Per-engine in-memory cache mapping `(table_name, from_id)` → a committed
/// adjacency list.
///
/// The cache is disabled when `max_hubs == 0`.  All public methods are safe to
/// call in that state — they return `None` / do nothing, preserving the
/// "existing tests run without cache" invariant.
pub struct AdjacencyCache {
    entries: DashMap<(String, i64), CacheEntry>,
    max_hubs: usize,
}

impl AdjacencyCache {
    /// Create a new cache.  `max_hubs = 0` disables caching entirely.
    pub fn new(max_hubs: usize) -> Self {
        Self {
            entries: DashMap::new(),
            max_hubs,
        }
    }

    /// Create from the `UNIDB_GRAPH_CACHE_HUBS` environment variable, falling
    /// back to `DEFAULT_MAX_HUBS` when the variable is absent or unparseable.
    pub fn from_env() -> Self {
        let max_hubs = std::env::var("UNIDB_GRAPH_CACHE_HUBS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_HUBS);
        Self::new(max_hubs)
    }

    /// Look up the adjacency list for `(table, from_id)`.
    ///
    /// Returns `Some(Arc<Vec<EdgeRef>>)` on a cache hit, updating the LRU
    /// timestamp.  Returns `None` on a miss or when the cache is disabled.
    pub fn get(&self, table: &str, from_id: i64) -> Option<Arc<Vec<EdgeRef>>> {
        if self.max_hubs == 0 {
            return None;
        }
        let entry = self.entries.get(&(table.to_string(), from_id))?;
        Some(entry.touch())
    }

    /// Insert or replace the adjacency list for `(table, from_id)`.
    ///
    /// When the map is at capacity, an approximate-LRU victim is evicted
    /// before inserting the new entry.  No-op when the cache is disabled.
    pub fn insert(&self, table: &str, from_id: i64, edges: Vec<EdgeRef>) {
        if self.max_hubs == 0 {
            return;
        }
        if self.entries.len() >= self.max_hubs {
            self.evict_one();
        }
        self.entries.insert(
            (table.to_string(), from_id),
            CacheEntry::new(Arc::new(edges)),
        );
    }

    /// Remove the cache entry for `(table, from_id)`, invalidating it.
    ///
    /// Called immediately before `create_edge` / `delete_edge` mutate the
    /// hub's adjacency list so that subsequent readers rebuild from storage.
    /// O(1) under the DashMap shard lock.  No-op when the cache is disabled
    /// or the key is absent.
    pub fn invalidate(&self, table: &str, from_id: i64) {
        if self.max_hubs == 0 {
            return;
        }
        self.entries.remove(&(table.to_string(), from_id));
    }

    /// Current number of cached hubs.  Exposed for tests and `/stats`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ── internal ─────────────────────────────────────────────────────────────

    /// Evict the entry with the smallest `last_used` among a random sample.
    /// This is O(sample_size) time and O(1) allocations — no full scan.
    fn evict_one(&self) {
        // Collect up to EVICTION_SAMPLE keys and their last_used timestamps.
        // DashMap's `iter()` yields shard-locked references in an unspecified
        // but deterministic order; taking the first N gives a pseudo-random
        // sample across shards.
        let mut candidates: Vec<((String, i64), u64)> = Vec::with_capacity(EVICTION_SAMPLE);
        for r in self.entries.iter().take(EVICTION_SAMPLE) {
            let ts = r.value().last_used.load(Ordering::Relaxed);
            candidates.push((r.key().clone(), ts));
        }
        if let Some((victim_key, _)) = candidates.into_iter().min_by_key(|(_, ts)| *ts) {
            self.entries.remove(&victim_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{format::DEFAULT_PAGE_SIZE, heap::RowId};

    fn make_edge(to_id: i64) -> EdgeRef {
        EdgeRef {
            to_id,
            edge_row_id: RowId {
                page_id: 1,
                slot: 0,
            },
            edge_type: "KNOWS".to_string(),
            props_inline: Some(b"{}".to_vec()),
        }
    }

    #[test]
    fn disabled_cache_always_misses() {
        let _ = DEFAULT_PAGE_SIZE; // suppress unused import lint
        let cache = AdjacencyCache::new(0);
        cache.insert("t", 1, vec![make_edge(2)]);
        assert!(cache.get("t", 1).is_none());
    }

    #[test]
    fn insert_then_get_returns_edges() {
        let cache = AdjacencyCache::new(10);
        cache.insert("t", 1, vec![make_edge(2), make_edge(3)]);
        let got = cache.get("t", 1).expect("cache hit");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].to_id, 2);
        assert_eq!(got[1].to_id, 3);
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = AdjacencyCache::new(10);
        cache.insert("t", 1, vec![make_edge(2)]);
        cache.invalidate("t", 1);
        assert!(cache.get("t", 1).is_none());
    }

    #[test]
    fn get_on_absent_key_returns_none() {
        let cache = AdjacencyCache::new(10);
        assert!(cache.get("t", 999).is_none());
    }

    #[test]
    fn cache_respects_max_hubs_cap() {
        let max = 5usize;
        let cache = AdjacencyCache::new(max);
        for i in 0..(max + 3) as i64 {
            cache.insert("t", i, vec![make_edge(i + 100)]);
        }
        // After inserting max+3 entries, eviction should have kept count <= max.
        // (Each insert that hits cap evicts one, so len == max after all inserts.)
        assert!(cache.len() <= max);
    }

    #[test]
    fn arc_clones_outlive_invalidation() {
        let cache = AdjacencyCache::new(10);
        cache.insert("t", 7, vec![make_edge(8), make_edge(9)]);
        let held = cache.get("t", 7).expect("hit");
        // invalidate after obtaining the Arc — existing holder must not be affected
        cache.invalidate("t", 7);
        assert!(cache.get("t", 7).is_none(), "entry gone from cache");
        // but the Arc clone still holds the old data
        assert_eq!(held.len(), 2, "old Arc still valid");
    }
}
