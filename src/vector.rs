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

/// Distance metric a `VectorIndex` uses to rank neighbors.
///
/// This is a **per-index** choice (Track D): a given `VectorIndex` computes
/// every distance — both while building the HNSW graph and while searching —
/// with a single metric. Switching the metric on an existing index is not a
/// cheap relabel: the graph's edges were chosen *by* the old metric, so a
/// change forces a full rebuild from the buffered point set (handled by
/// [`VectorIndex::set_metric`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Metric {
    /// Straight-line L2 distance. Matches `pgvector`'s `<->` operator, the
    /// default kept for backward compatibility with M2's index.
    #[default]
    Euclidean,
    /// Cosine *distance* (`1 - cosine_similarity`), in `[0, 2]`. Direction-only
    /// similarity — the natural metric for text embeddings, where magnitude
    /// carries little meaning. Matches `pgvector`'s `<=>` operator.
    Cosine,
}

fn euclidean(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

/// `1 - cos(a, b)`, guarding against a zero-length vector (undefined cosine),
/// which is treated as maximally distant so it never wins a nearest-neighbor
/// race.
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 1.0;
    }
    1.0 - dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[derive(Clone)]
struct VectorPoint {
    coords: Vec<f32>,
    metric: Metric,
}

impl instant_distance::Point for VectorPoint {
    /// Distance under this point's metric. Every point in one index carries the
    /// same metric (set at build time), so both operands always agree.
    fn distance(&self, other: &Self) -> f32 {
        match self.metric {
            Metric::Euclidean => euclidean(&self.coords, &other.coords),
            Metric::Cosine => cosine_distance(&self.coords, &other.coords),
        }
    }
}

pub struct VectorIndex {
    points: HashMap<RowId, Vec<f32>>,
    map: Option<HnswMap<VectorPoint, RowId>>,
    metric: Metric,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndex {
    /// A new, empty index using the default [`Metric::Euclidean`] — kept as the
    /// zero-argument constructor so existing M2 callers are unaffected.
    pub fn new() -> Self {
        Self::with_metric(Metric::Euclidean)
    }

    /// A new, empty index using an explicit metric.
    pub fn with_metric(metric: Metric) -> Self {
        Self {
            points: HashMap::new(),
            map: None,
            metric,
        }
    }

    /// The metric this index ranks with.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// Change the metric. Because the HNSW graph's edges were chosen by the old
    /// metric, this rebuilds the whole graph from the buffered points so future
    /// searches are consistent. A no-op (no rebuild) if the metric is unchanged.
    pub fn set_metric(&mut self, metric: Metric) {
        if self.metric == metric {
            return;
        }
        self.metric = metric;
        self.rebuild();
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
        let metric = self.metric;
        let (points, ids): (Vec<VectorPoint>, Vec<RowId>) = self
            .points
            .iter()
            .map(|(id, v)| {
                (
                    VectorPoint {
                        coords: v.clone(),
                        metric,
                    },
                    *id,
                )
            })
            .unzip();
        self.map = Some(Builder::default().build(points, ids));
    }

    /// Up to `k` nearest neighbors to `query`, nearest first. The returned
    /// distance is in this index's metric (L2 for Euclidean, `1 - cos` for
    /// Cosine).
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(RowId, f32)> {
        let Some(map) = &self.map else {
            return Vec::new();
        };
        let point = VectorPoint {
            coords: query.to_vec(),
            metric: self.metric,
        };
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

    #[test]
    fn default_metric_is_euclidean() {
        assert_eq!(VectorIndex::new().metric(), Metric::Euclidean);
        assert_eq!(
            VectorIndex::with_metric(Metric::Cosine).metric(),
            Metric::Cosine
        );
    }

    #[test]
    fn cosine_ranks_by_direction_not_magnitude() {
        // Two candidates: one points the same *direction* as the query but is
        // far away in magnitude; the other is close in Euclidean terms but
        // points elsewhere. Cosine must prefer the same-direction one.
        let mut idx = VectorIndex::with_metric(Metric::Cosine);
        idx.upsert(rid(1, 0), vec![100.0, 0.0]); // same direction as query, big
        idx.upsert(rid(2, 0), vec![1.0, 1.0]); // 45° off, but Euclidean-closer

        let results = idx.search(&[1.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, rid(1, 0));
        // Same-direction vector has cosine distance ~0.
        assert!(results[0].1 < 1e-4, "distance was {}", results[0].1);
    }

    #[test]
    fn euclidean_and_cosine_can_disagree() {
        let build = |metric| {
            let mut idx = VectorIndex::with_metric(metric);
            idx.upsert(rid(1, 0), vec![100.0, 0.0]);
            idx.upsert(rid(2, 0), vec![1.0, 1.0]);
            idx.search(&[1.0, 0.0], 1)[0].0
        };
        // Euclidean prefers the physically nearer point; cosine the aligned one.
        assert_eq!(build(Metric::Euclidean), rid(2, 0));
        assert_eq!(build(Metric::Cosine), rid(1, 0));
    }

    #[test]
    fn set_metric_rebuilds_and_changes_ranking() {
        let mut idx = VectorIndex::new(); // Euclidean
        idx.upsert(rid(1, 0), vec![100.0, 0.0]);
        idx.upsert(rid(2, 0), vec![1.0, 1.0]);
        assert_eq!(idx.search(&[1.0, 0.0], 1)[0].0, rid(2, 0));

        idx.set_metric(Metric::Cosine);
        assert_eq!(idx.metric(), Metric::Cosine);
        assert_eq!(idx.search(&[1.0, 0.0], 1)[0].0, rid(1, 0));
    }

    #[test]
    fn zero_vector_is_maximally_distant_under_cosine() {
        assert_eq!(cosine_distance(&[0.0, 0.0], &[1.0, 1.0]), 1.0);
        assert_eq!(cosine_distance(&[1.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
