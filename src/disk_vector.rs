// On-disk vector index — P3.c SPIKE (not yet wired into the Engine).
//
// The Phase-3 blueprint (`docs/backlog/phase3_durable_storage.md` §P3.c) mandates
// a spike to choose an on-disk ANN approach and **validate recall@k before
// committing** — HNSW (a RAM graph) does not page cleanly, so we need bounded
// RAM + no full rebuild + crash safety. This module is that spike: an
// **on-disk IVF-Flat** index, chosen over DiskANN/Vamana for the spike because
// it pages trivially onto machinery we already have and its recall/latency is
// directly tunable and measurable.
//
// ## Why IVF-Flat reuses everything we built
//
// IVF partitions the vector space into `nlist` Voronoi cells around trained
// centroids. The only per-cell state is a **posting list: cell_id -> [RowId]** —
// which is *exactly* what `DiskBTree` (P3.a) already stores durably, WAL-logged,
// crash-recovered, and buffer-pool-managed. So the on-disk part of this index is
// a `DiskBTree` keyed by `OrderedValue::Int(cell_id)`; the only new in-RAM state
// is the centroid table (`nlist * dim` f32s — bounded, independent of the corpus
// size). Vectors themselves stay in the heap (IVF-Flat re-ranks candidates with
// exact distances fetched at query time), so nothing is duplicated.
//
//   * **insert**: assign the vector to its nearest centroid, insert
//     `(cell_id, RowId)` into the `DiskBTree` (durable, one WAL mini-txn).
//   * **search**: score the query against every centroid, probe the `nprobe`
//     nearest cells (`search_eq` each), gather candidate `RowId`s, fetch their
//     vectors, and return the exact top-k. Recall rises with `nprobe`.
//
// ## What this spike deliberately does NOT do (production follow-up)
//
// * Centroids are trained once (mini-batch Lloyd's k-means) and kept in RAM;
//   persisting them in a meta page and re-training as a maintenance op (like
//   vacuum) is production work, not needed to validate recall.
// * No PQ/OPQ compression of vectors for in-RAM routing (that is the DiskANN
//   angle) — IVF-Flat fetches exact vectors, which is fine at the single-node
//   100s-of-GB target and keeps recall a pure function of `nprobe`.
// * Not wired into `CREATE INDEX ... USING HNSW` / `NEAR` yet — the recall
//   benchmark (`benches/vector_recall.rs`) drives it directly.
//
// The recommendation coming out of this spike is recorded in
// `docs/design/p3c_vector_spike.md`.

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    bufferpool::BufferPool,
    error::Result,
    format::PageId,
    heap::RowId,
    vector::Metric,
    wal::Wal,
};

fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Euclidean => a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt(),
        Metric::Cosine => {
            let mut dot = 0.0f32;
            let mut na = 0.0f32;
            let mut nb = 0.0f32;
            for (x, y) in a.iter().zip(b) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            if na == 0.0 || nb == 0.0 {
                return 1.0;
            }
            1.0 - dot / (na.sqrt() * nb.sqrt())
        }
    }
}

/// On-disk IVF-Flat index (P3.c spike). Centroids live in RAM (bounded); the
/// cell posting lists live in a durable `DiskBTree` on disk.
pub struct DiskIvfIndex {
    centroids: Vec<Vec<f32>>,
    postings: DiskBTree,
    metric: Metric,
    dim: usize,
}

impl DiskIvfIndex {
    /// Train `nlist` centroids from `sample` via a few Lloyd's iterations, and
    /// create the empty on-disk posting-list tree. `sample` should be a
    /// representative subset of the corpus (all of it, for the spike).
    pub fn train(
        sample: &[Vec<f32>],
        nlist: usize,
        iters: usize,
        metric: Metric,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<DiskIvfIndex> {
        assert!(!sample.is_empty(), "cannot train on an empty sample");
        let dim = sample[0].len();
        let nlist = nlist.min(sample.len()).max(1);

        // Initialize centroids as an evenly-spaced sample (deterministic, good
        // enough for the spike — k-means++ is a production refinement).
        let mut centroids: Vec<Vec<f32>> = (0..nlist)
            .map(|i| sample[i * sample.len() / nlist].clone())
            .collect();

        for _ in 0..iters {
            let mut sums = vec![vec![0.0f32; dim]; nlist];
            let mut counts = vec![0usize; nlist];
            for v in sample {
                let c = Self::nearest_centroid(&centroids, metric, v);
                for (s, x) in sums[c].iter_mut().zip(v) {
                    *s += x;
                }
                counts[c] += 1;
            }
            for (i, (sum, &count)) in sums.iter().zip(&counts).enumerate() {
                if count > 0 {
                    centroids[i] = sum.iter().map(|s| s / count as f32).collect();
                }
            }
        }

        let postings = DiskBTree::create(pool, wal)?;
        Ok(DiskIvfIndex {
            centroids,
            postings,
            metric,
            dim,
        })
    }

    /// The stable meta page of the on-disk posting-list tree (would be stored in
    /// the catalog by the production wiring).
    pub fn meta_page(&self) -> PageId {
        self.postings.meta_page()
    }

    pub fn nlist(&self) -> usize {
        self.centroids.len()
    }

    /// Approximate in-RAM footprint in bytes: just the centroid table, which is
    /// `nlist * dim * 4` — bounded and independent of the corpus size (the whole
    /// point vs. the in-RAM HNSW graph, which is O(corpus)).
    pub fn ram_bytes(&self) -> usize {
        self.centroids.len() * self.dim * std::mem::size_of::<f32>()
    }

    fn nearest_centroid(centroids: &[Vec<f32>], metric: Metric, v: &[f32]) -> usize {
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for (i, c) in centroids.iter().enumerate() {
            let d = distance(metric, c, v);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }

    /// Insert `(rid, vector)`: assign to the nearest cell and record it in the
    /// durable posting-list tree (one WAL mini-txn).
    pub fn insert(
        &self,
        rid: RowId,
        vector: &[f32],
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<()> {
        let cell = Self::nearest_centroid(&self.centroids, self.metric, vector);
        self.postings
            .insert(OrderedValue::Int(cell as i64), rid, pool, wal)
    }

    /// Approximate top-`k` nearest neighbors to `query`, probing the `nprobe`
    /// nearest cells. `fetch` returns the stored vector for a candidate `RowId`
    /// (the heap, in production). Returns `(RowId, exact_distance)` nearest-first.
    pub fn search<F>(
        &self,
        query: &[f32],
        k: usize,
        nprobe: usize,
        pool: &mut BufferPool,
        fetch: F,
    ) -> Result<Vec<(RowId, f32)>>
    where
        F: Fn(RowId) -> Option<Vec<f32>>,
    {
        // Rank cells by centroid distance, take the nearest `nprobe`.
        let mut cell_d: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, distance(self.metric, c, query)))
            .collect();
        cell_d.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // Gather candidate RowIds from the probed cells' posting lists.
        let mut candidates = Vec::new();
        for &(cell, _) in cell_d.iter().take(nprobe.max(1)) {
            candidates.extend(
                self.postings
                    .search_eq(&OrderedValue::Int(cell as i64), pool)?,
            );
        }

        // Exact re-rank against the fetched vectors.
        let mut scored: Vec<(RowId, f32)> = candidates
            .into_iter()
            .filter_map(|rid| fetch(rid).map(|v| (rid, distance(self.metric, query, &v))))
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        scored.truncate(k);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn rid(i: u32) -> RowId {
        RowId {
            page_id: i,
            slot: 0,
        }
    }

    /// On a set of clearly-separated clusters, IVF-Flat with enough probes finds
    /// the exact nearest neighbor — the spike's basic correctness bar.
    #[test]
    fn ivf_finds_nearest_on_separated_clusters() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        // 4 tight clusters around the corners of a 100-unit square.
        let centers = [[0.0, 0.0], [100.0, 0.0], [0.0, 100.0], [100.0, 100.0]];
        let mut vectors: HashMap<RowId, Vec<f32>> = HashMap::new();
        let mut sample = Vec::new();
        let mut idx_counter = 0u32;
        for (ci, c) in centers.iter().enumerate() {
            for j in 0..25 {
                let jitter = (j as f32) * 0.01;
                let v = vec![c[0] + jitter, c[1] + jitter];
                let r = rid(idx_counter);
                idx_counter += 1;
                vectors.insert(r, v.clone());
                sample.push(v);
                let _ = ci;
            }
        }

        let ivf =
            DiskIvfIndex::train(&sample, 4, 10, Metric::Euclidean, &mut pool, &mut wal).unwrap();
        for (r, v) in &vectors {
            ivf.insert(*r, v, &mut pool, &mut wal).unwrap();
        }

        // Query right next to cluster 1's center; nprobe=2 must find a point
        // essentially on top of it.
        let query = vec![100.0, 0.0];
        let results = ivf
            .search(&query, 5, 2, &mut pool, |r| vectors.get(&r).cloned())
            .unwrap();
        assert!(!results.is_empty());
        // The nearest result must be from cluster 1 (near [100,0]) — distance ~0.
        assert!(
            results[0].1 < 1.0,
            "nearest distance should be ~0, got {}",
            results[0].1
        );
        let (bx, _by) = {
            let v = vectors.get(&results[0].0).unwrap();
            (v[0], v[1])
        };
        assert!((bx - 100.0).abs() < 1.0, "nearest should be in cluster 1");
    }

    /// RAM footprint is the centroid table only — independent of corpus size.
    #[test]
    fn ram_footprint_is_bounded_by_nlist_not_corpus() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
        let sample: Vec<Vec<f32>> = (0..1000).map(|i| vec![i as f32, (i * 2) as f32]).collect();
        let ivf =
            DiskIvfIndex::train(&sample, 16, 5, Metric::Euclidean, &mut pool, &mut wal).unwrap();
        // 16 centroids * 2 dims * 4 bytes = 128 bytes, regardless of the 1000 pts.
        assert_eq!(ivf.ram_bytes(), 16 * 2 * 4);
        assert_eq!(ivf.nlist(), 16);
    }
}
