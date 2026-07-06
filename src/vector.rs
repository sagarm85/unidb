// Vector similarity index (M2.b): a small wrapper around `instant-distance`
// so only this file's internals change if the crate needs swapping later.
//
// Design correction from the original M2 plan: `instant-distance` 0.6.1 has
// no incremental single-point insert in its public API (checked against the
// vendored source) — `Builder::build` only constructs an `HnswMap` from a
// full `Vec<P>`/`Vec<V>` at once. The plan assumed native incremental
// insertion; it doesn't exist. Corrected design: `VectorIndex` buffers every
// live point in a `HashMap<RowId, Vec<f32>>` and rebuilds the whole HNSW
// graph from scratch on every `upsert`/`remove`. This still satisfies "row
// write is the only synchronous cost" (CLAUDE.md's M2 goal) because the
// rebuild happens entirely on the background worker thread — the foreground
// write path only ever sends a channel message. See MEMORY.md's M2.b design
// note for the full reasoning; §6's benchmark table will show honestly if
// full-rebuild-per-upsert becomes a real bottleneck at larger row counts.

use std::collections::HashMap;

use instant_distance::{Builder, HnswMap, Search};

use crate::heap::RowId;

#[derive(Clone)]
struct VectorPoint(Vec<f32>);

impl instant_distance::Point for VectorPoint {
    /// Euclidean distance — the common default for embedding similarity and
    /// matches `pgvector`'s `<->` operator, keeping the later benchmark
    /// comparison apples-to-apples.
    fn distance(&self, other: &Self) -> f32 {
        self.0
            .iter()
            .zip(other.0.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt()
    }
}

pub struct VectorIndex {
    points: HashMap<RowId, Vec<f32>>,
    map: Option<HnswMap<VectorPoint, RowId>>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndex {
    pub fn new() -> Self {
        Self {
            points: HashMap::new(),
            map: None,
        }
    }

    /// Insert or overwrite the vector for `id`, then rebuild the HNSW graph
    /// from the full current point set (see module doc for why this is a
    /// rebuild, not a true incremental insert).
    pub fn upsert(&mut self, id: RowId, vector: Vec<f32>) {
        self.points.insert(id, vector);
        self.rebuild();
    }

    pub fn remove(&mut self, id: RowId) {
        self.points.remove(&id);
        self.rebuild();
    }

    fn rebuild(&mut self) {
        if self.points.is_empty() {
            self.map = None;
            return;
        }
        let (points, ids): (Vec<VectorPoint>, Vec<RowId>) = self
            .points
            .iter()
            .map(|(id, v)| (VectorPoint(v.clone()), *id))
            .unzip();
        self.map = Some(Builder::default().build(points, ids));
    }

    /// Up to `k` nearest neighbors to `query`, nearest first.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(RowId, f32)> {
        let Some(map) = &self.map else {
            return Vec::new();
        };
        let point = VectorPoint(query.to_vec());
        let mut search = Search::default();
        map.search(&point, &mut search)
            .take(k)
            .map(|item| (*item.value, item.distance))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
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
    fn empty_index_search_returns_nothing() {
        let idx = VectorIndex::new();
        assert!(idx.search(&[0.0, 0.0], 5).is_empty());
    }

    #[test]
    fn search_returns_nearest_neighbor_first() {
        let mut idx = VectorIndex::new();
        idx.upsert(rid(1, 0), vec![0.0, 0.0]);
        idx.upsert(rid(2, 0), vec![10.0, 10.0]);
        idx.upsert(rid(3, 0), vec![0.1, 0.1]);

        let results = idx.search(&[0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, rid(1, 0));
        assert_eq!(results[1].0, rid(3, 0));
    }

    #[test]
    fn upsert_overwrites_existing_vector() {
        let mut idx = VectorIndex::new();
        idx.upsert(rid(1, 0), vec![0.0, 0.0]);
        idx.upsert(rid(1, 0), vec![100.0, 100.0]);
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[100.0, 100.0], 1);
        assert_eq!(results[0].0, rid(1, 0));
    }

    #[test]
    fn remove_drops_point_from_results() {
        let mut idx = VectorIndex::new();
        idx.upsert(rid(1, 0), vec![0.0, 0.0]);
        idx.upsert(rid(2, 0), vec![1.0, 1.0]);
        idx.remove(rid(1, 0));
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[0.0, 0.0], 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, rid(2, 0));
    }
}
