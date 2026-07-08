// On-disk vector index — durable IVF-Flat (P3.c production).
//
// This began as the P3.c spike (`docs/design/p3c_vector_spike.md`): an on-disk
// IVF-Flat index chosen over DiskANN/Vamana because it pages trivially onto
// machinery Phase 3 already built and its recall/latency is directly tunable.
// The spike validated recall@10 = 1.000 at nprobe=4 vs. brute force. This module
// is the **production promotion**: centroids and config are now persisted in a
// WAL-logged meta/centroid page chain, so the index is crash-recovered and
// **never rebuilt on open** — the same O(1)-open contract the durable `DiskBTree`
// (P3.a) already gives. The in-RAM HNSW (`vector.rs`) and its async rebuild
// worker are retired; `CREATE INDEX ... USING HNSW`/`USING IVF` builds this.
//
// ## Why IVF-Flat reuses everything we built
//
// IVF partitions the vector space into `nlist` Voronoi cells around trained
// centroids. The only per-cell state is a **posting list: cell_id -> [RowId]** —
// which is *exactly* what `DiskBTree` (P3.a) already stores durably, WAL-logged,
// crash-recovered, and buffer-pool-managed. So the postings are a `DiskBTree`
// keyed by `OrderedValue::Int(cell_id)`; the only other on-disk state is the
// centroid table (`nlist * dim` f32s — bounded, independent of corpus size).
// Vectors themselves stay in the heap (IVF-Flat re-ranks candidates with exact
// distances fetched at query time), so nothing is duplicated.
//
//   * **insert**: assign the vector to its nearest centroid, insert
//     `(cell_id, RowId)` into the postings `DiskBTree` (durable, one WAL mini-txn).
//   * **search**: score the query against every centroid, probe the `nprobe`
//     nearest cells (`search_eq` each), gather candidate `RowId`s, and let the
//     caller fetch the exact vectors from the heap for an exact re-rank. Recall
//     rises with `nprobe`.
//
// ## On-disk layout (all pages carry the standard header + CRC, WAL-logged via
// `WAL_INDEX` full-page images — recovered identically to `DiskBTree` pages, no
// new record kind or page type, no `FORMAT_VERSION` bump)
//
//   * **meta page** (stable id, stored in the catalog as `ColumnDef.index_root`,
//     never changes — mirrors `DiskBTree`'s meta page):
//       body[0]      = IVF_META_MAGIC
//       body[1]      = metric (0 = Euclidean, 1 = Cosine)
//       body[2..6]   = dim (u32)
//       body[6..10]  = nlist (u32)
//       body[10..14] = nprobe (u32) — default probe count for NEAR
//       body[14..18] = postings meta page id (u32) — the cell posting-list tree
//       body[18..22] = centroid chain head page id (u32)
//   * **centroid data page(s)** (a right-linked chain holding the flat
//     `nlist * dim` f32 centroid table, split by page capacity):
//       body[0..4]   = next page id (u32; INVALID_PAGE_ID = end of chain)
//       body[4..6]   = float count in this page (u16)
//       body[6..]    = count × f32 (little-endian)
//
// ## v1 simplifications (documented, not silent)
//
// * Centroids are trained once at `CREATE INDEX` from the committed rows (mini-
//   batch Lloyd's k-means) and then fixed. An index created on an *empty* table
//   trains a single origin cell (nlist=1), so it degrades to correct-but-flat
//   brute force until re-created — re-training as a maintenance op (like vacuum)
//   is a documented follow-up.
// * No PQ/OPQ compression of vectors for in-RAM routing (that is the DiskANN
//   angle) — IVF-Flat fetches exact vectors, which keeps recall a pure function
//   of `nprobe` at the single-node target.

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    bufferpool::BufferPool,
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, Lsn, PageId, INVALID_PAGE_ID,
        PAGE_TYPE_BTREE,
    },
    heap::RowId,
    page::{SlottedPage, PAGE_HEADER_SIZE},
    vector::Metric,
    wal::Wal,
};

const IVF_META_MAGIC: u8 = 0xF1;

fn metric_tag(m: Metric) -> u8 {
    match m {
        Metric::Euclidean => 0,
        Metric::Cosine => 1,
    }
}

fn metric_from_tag(t: u8) -> Metric {
    match t {
        1 => Metric::Cosine,
        _ => Metric::Euclidean,
    }
}

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

/// Number of f32 centroid values that fit in one centroid data page.
fn floats_per_page(page_size: usize) -> usize {
    (page_size - PAGE_HEADER_SIZE - 6) / 4
}

/// Train `nlist` centroids from `sample` via a few Lloyd's iterations. An empty
/// sample (index created on an empty table) yields a single origin centroid —
/// correct-but-flat until re-created.
fn train_centroids(
    dim: usize,
    sample: &[Vec<f32>],
    nlist_req: usize,
    iters: usize,
    metric: Metric,
) -> Vec<Vec<f32>> {
    if sample.is_empty() {
        return vec![vec![0.0f32; dim]];
    }
    let nlist = nlist_req.min(sample.len()).max(1);
    // Deterministic evenly-spaced initialization (k-means++ is a refinement).
    let mut centroids: Vec<Vec<f32>> = (0..nlist)
        .map(|i| sample[i * sample.len() / nlist].clone())
        .collect();
    for _ in 0..iters {
        let mut sums = vec![vec![0.0f32; dim]; nlist];
        let mut counts = vec![0usize; nlist];
        for v in sample {
            let c = nearest_centroid(&centroids, metric, v);
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
    centroids
}

/// Everything the meta page records — loaded (bounded, O(nlist·dim)) per
/// operation, mirroring how `DiskBTree` reloads its root from its meta page.
#[derive(Debug, Clone)]
pub struct IvfHeader {
    pub metric: Metric,
    pub dim: usize,
    pub nlist: usize,
    pub nprobe: usize,
    pub postings_meta: PageId,
    pub centroid_head: PageId,
}

/// On-disk IVF-Flat vector index (P3.c production). A stateless handle over its
/// stable meta page id — like [`DiskBTree`], it holds no tree/centroid state in
/// the struct; every operation reloads the (bounded) centroid table from disk.
pub struct DiskIvfIndex {
    meta_page: PageId,
    page_size: usize,
}

impl DiskIvfIndex {
    /// Wrap an existing durable index by its stable meta page id (the value in
    /// `ColumnDef.index_root`). O(1) — nothing is read until an operation runs.
    pub fn open(meta_page: PageId, page_size: usize) -> Self {
        Self {
            meta_page,
            page_size,
        }
    }

    pub fn meta_page(&self) -> PageId {
        self.meta_page
    }

    /// Train centroids from `sample`, create the empty postings tree, and
    /// durably persist the centroid table + meta page (one WAL mini-txn for the
    /// centroid/meta pages; the postings tree's `create` is its own mini-txn).
    /// The caller records [`Self::meta_page`] in the catalog. `sample` may be
    /// empty (empty-table `CREATE INDEX`).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        dim: usize,
        sample: &[Vec<f32>],
        nlist_req: usize,
        nprobe_req: usize,
        iters: usize,
        metric: Metric,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<DiskIvfIndex> {
        let page_size = pool.page_size();
        let centroids = train_centroids(dim, sample, nlist_req, iters, metric);
        let nlist = centroids.len();
        let nprobe = nprobe_req.clamp(1, nlist);

        let postings = DiskBTree::create(pool, wal)?;

        // Flatten the centroid table and lay it out across a page chain.
        let mut flat: Vec<f32> = Vec::with_capacity(nlist * dim);
        for c in &centroids {
            flat.extend_from_slice(c);
        }
        let fpp = floats_per_page(page_size);
        let n_pages = flat.len().div_ceil(fpp).max(1);
        let mut cpages = Vec::with_capacity(n_pages);
        for _ in 0..n_pages {
            cpages.push(pool.alloc_page()?);
        }
        let meta_page = pool.alloc_page()?;

        let (txn, begin) = wal.begin_mini_txn()?;
        let mut prev = begin;
        for pi in 0..n_pages {
            let start = pi * fpp;
            let end = ((pi + 1) * fpp).min(flat.len());
            let next = if pi + 1 < n_pages {
                cpages[pi + 1]
            } else {
                INVALID_PAGE_ID
            };
            let img = centroid_page_bytes(cpages[pi], next, &flat[start..end], page_size);
            prev = write_image(pool, wal, txn, prev, cpages[pi], img)?;
        }
        let meta_img = meta_page_bytes(
            meta_page,
            metric,
            dim,
            nlist,
            nprobe,
            postings.meta_page(),
            cpages[0],
            page_size,
        );
        prev = write_image(pool, wal, txn, prev, meta_page, meta_img)?;
        wal.commit_mini_txn(txn, prev)?;

        Ok(DiskIvfIndex::open(meta_page, page_size))
    }

    /// Load the meta page's config (bounded; a single page read).
    pub fn load_header(&self, pool: &mut BufferPool) -> Result<IvfHeader> {
        let page = pool.fetch_page(self.meta_page)?;
        let body = &page.as_bytes()[PAGE_HEADER_SIZE..];
        if body.first().copied() != Some(IVF_META_MAGIC) {
            pool.unpin(self.meta_page);
            return Err(DbError::Recovery(format!(
                "IVF meta page {} is not an IVF meta node",
                self.meta_page
            )));
        }
        let hdr = IvfHeader {
            metric: metric_from_tag(body[1]),
            dim: u32_from_le(body[2..6].try_into().unwrap()) as usize,
            nlist: u32_from_le(body[6..10].try_into().unwrap()) as usize,
            nprobe: u32_from_le(body[10..14].try_into().unwrap()) as usize,
            postings_meta: u32_from_le(body[14..18].try_into().unwrap()),
            centroid_head: u32_from_le(body[18..22].try_into().unwrap()),
        };
        pool.unpin(self.meta_page);
        Ok(hdr)
    }

    /// Load the centroid table (bounded: `nlist * dim` floats) by walking the
    /// centroid page chain.
    fn load_centroids(&self, hdr: &IvfHeader, pool: &mut BufferPool) -> Result<Vec<Vec<f32>>> {
        let mut flat: Vec<f32> = Vec::with_capacity(hdr.nlist * hdr.dim);
        let mut pid = hdr.centroid_head;
        while pid != INVALID_PAGE_ID {
            let page = pool.fetch_page(pid)?;
            let body = &page.as_bytes()[PAGE_HEADER_SIZE..];
            let next = u32_from_le(body[0..4].try_into().unwrap());
            let count = u16_from_le(body[4..6].try_into().unwrap()) as usize;
            for i in 0..count {
                let o = 6 + i * 4;
                flat.push(f32::from_le_bytes(body[o..o + 4].try_into().unwrap()));
            }
            pool.unpin(pid);
            pid = next;
        }
        if flat.len() != hdr.nlist * hdr.dim {
            return Err(DbError::Recovery(format!(
                "IVF centroid table length {} != nlist*dim {}",
                flat.len(),
                hdr.nlist * hdr.dim
            )));
        }
        Ok((0..hdr.nlist)
            .map(|c| flat[c * hdr.dim..(c + 1) * hdr.dim].to_vec())
            .collect())
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
        let hdr = self.load_header(pool)?;
        let centroids = self.load_centroids(&hdr, pool)?;
        let cell = nearest_centroid(&centroids, hdr.metric, vector);
        DiskBTree::new(hdr.postings_meta, self.page_size).insert(
            OrderedValue::Int(cell as i64),
            rid,
            pool,
            wal,
        )
    }

    /// Remove `(rid, vector)` from its cell's posting list (used by vacuum's
    /// aliasing gate so a reused slot can't surface a stale candidate).
    pub fn remove(
        &self,
        rid: RowId,
        vector: &[f32],
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<()> {
        let hdr = self.load_header(pool)?;
        let centroids = self.load_centroids(&hdr, pool)?;
        let cell = nearest_centroid(&centroids, hdr.metric, vector);
        DiskBTree::new(hdr.postings_meta, self.page_size).remove(
            &OrderedValue::Int(cell as i64),
            rid,
            pool,
            wal,
        )
    }

    /// Gather candidate `RowId`s from the `nprobe` nearest cells' posting lists
    /// (nprobe from the meta page unless `nprobe_override` is given). Returns the
    /// index `Metric` too, so the caller can exact-re-rank fetched vectors.
    pub fn candidates(
        &self,
        query: &[f32],
        nprobe_override: Option<usize>,
        pool: &mut BufferPool,
    ) -> Result<(Metric, Vec<RowId>)> {
        let hdr = self.load_header(pool)?;
        let centroids = self.load_centroids(&hdr, pool)?;
        let nprobe = nprobe_override
            .unwrap_or(hdr.nprobe)
            .clamp(1, hdr.nlist.max(1));

        let mut cell_d: Vec<(usize, f32)> = centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, distance(hdr.metric, c, query)))
            .collect();
        cell_d.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        let postings = DiskBTree::new(hdr.postings_meta, self.page_size);
        let mut out = Vec::new();
        for &(cell, _) in cell_d.iter().take(nprobe) {
            out.extend(postings.search_eq(&OrderedValue::Int(cell as i64), pool)?);
        }
        Ok((hdr.metric, out))
    }

    /// Approximate top-`k` nearest neighbors to `query`. `fetch` returns the
    /// stored vector for a candidate `RowId` (the heap, in production). Convenience
    /// wrapper over [`Self::candidates`] for benches/tests where the fetch source
    /// is independent of the buffer pool; the SQL executor calls `candidates`
    /// directly so its heap fetch shares the same `&mut BufferPool` and re-checks
    /// MVCC visibility per row.
    pub fn search<F>(
        &self,
        query: &[f32],
        k: usize,
        nprobe_override: Option<usize>,
        pool: &mut BufferPool,
        fetch: F,
    ) -> Result<Vec<(RowId, f32)>>
    where
        F: Fn(RowId) -> Option<Vec<f32>>,
    {
        let (metric, candidates) = self.candidates(query, nprobe_override, pool)?;
        let mut scored: Vec<(RowId, f32)> = candidates
            .into_iter()
            .filter_map(|rid| fetch(rid).map(|v| (rid, distance(metric, query, &v))))
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        scored.truncate(k);
        Ok(scored)
    }

    /// Approximate in-RAM footprint of the centroid table in bytes — bounded by
    /// `nlist * dim`, independent of corpus size (the whole point vs. HNSW).
    pub fn ram_bytes(&self, pool: &mut BufferPool) -> Result<usize> {
        let hdr = self.load_header(pool)?;
        Ok(hdr.nlist * hdr.dim * std::mem::size_of::<f32>())
    }
}

/// Serialize an IVF meta page image (CRC/LSN filled by [`write_image`]).
#[allow(clippy::too_many_arguments)]
fn meta_page_bytes(
    meta_page: PageId,
    metric: Metric,
    dim: usize,
    nlist: usize,
    nprobe: usize,
    postings_meta: PageId,
    centroid_head: PageId,
    page_size: usize,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    buf[0..4].copy_from_slice(&u32_to_le(meta_page));
    buf[4] = PAGE_TYPE_BTREE;
    let body = &mut buf[PAGE_HEADER_SIZE..];
    body[0] = IVF_META_MAGIC;
    body[1] = metric_tag(metric);
    body[2..6].copy_from_slice(&u32_to_le(dim as u32));
    body[6..10].copy_from_slice(&u32_to_le(nlist as u32));
    body[10..14].copy_from_slice(&u32_to_le(nprobe as u32));
    body[14..18].copy_from_slice(&u32_to_le(postings_meta));
    body[18..22].copy_from_slice(&u32_to_le(centroid_head));
    buf
}

/// Serialize one centroid data page image holding `floats` (CRC/LSN filled by
/// [`write_image`]).
fn centroid_page_bytes(page_id: PageId, next: PageId, floats: &[f32], page_size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    buf[0..4].copy_from_slice(&u32_to_le(page_id));
    buf[4] = PAGE_TYPE_BTREE;
    let body = &mut buf[PAGE_HEADER_SIZE..];
    body[0..4].copy_from_slice(&u32_to_le(next));
    body[4..6].copy_from_slice(&u16_to_le(floats.len() as u16));
    let mut o = 6;
    for f in floats {
        body[o..o + 4].copy_from_slice(&f.to_le_bytes());
        o += 4;
    }
    buf
}

/// Pin the page for write, WAL-log the full image (`WAL_INDEX`, redo-only —
/// recovered exactly like a `DiskBTree` node), stamp the LSN, write it. Mirrors
/// `btree_index::write_raw`.
fn write_image(
    pool: &mut BufferPool,
    wal: &mut Wal,
    txn_id: u64,
    prev_lsn: Lsn,
    page_id: PageId,
    image: Vec<u8>,
) -> Result<Lsn> {
    let _ = pool.fetch_page_for_write(page_id, wal)?;
    let lsn = wal.log_index(txn_id, prev_lsn, page_id, &image)?;
    let mut sp = SlottedPage::from_bytes_unchecked(image);
    sp.set_lsn(lsn);
    pool.write_page(&sp)?;
    pool.unpin(page_id);
    Ok(lsn)
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
    /// the exact nearest neighbor.
    #[test]
    fn ivf_finds_nearest_on_separated_clusters() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        let centers = [[0.0, 0.0], [100.0, 0.0], [0.0, 100.0], [100.0, 100.0]];
        let mut vectors: HashMap<RowId, Vec<f32>> = HashMap::new();
        let mut sample = Vec::new();
        let mut idx_counter = 0u32;
        for c in centers.iter() {
            for j in 0..25 {
                let jitter = (j as f32) * 0.01;
                let v = vec![c[0] + jitter, c[1] + jitter];
                let r = rid(idx_counter);
                idx_counter += 1;
                vectors.insert(r, v.clone());
                sample.push(v);
            }
        }

        let ivf =
            DiskIvfIndex::create(2, &sample, 4, 2, 10, Metric::Euclidean, &mut pool, &mut wal)
                .unwrap();
        for (r, v) in &vectors {
            ivf.insert(*r, v, &mut pool, &mut wal).unwrap();
        }

        let query = vec![100.0, 0.0];
        let results = ivf
            .search(&query, 5, None, &mut pool, |r| vectors.get(&r).cloned())
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results[0].1 < 1.0,
            "nearest distance should be ~0, got {}",
            results[0].1
        );
        let bx = vectors.get(&results[0].0).unwrap()[0];
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
            DiskIvfIndex::create(2, &sample, 16, 4, 5, Metric::Euclidean, &mut pool, &mut wal)
                .unwrap();
        assert_eq!(ivf.ram_bytes(&mut pool).unwrap(), 16 * 2 * 4);
        assert_eq!(ivf.load_header(&mut pool).unwrap().nlist, 16);
    }

    /// The centroid table survives being reloaded from disk through a fresh
    /// handle (the durable-open path): reopen by meta page id, no rebuild.
    #[test]
    fn reopen_by_meta_page_preserves_search() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        let sample: Vec<Vec<f32>> = (0..200).map(|i| vec![i as f32, -(i as f32)]).collect();
        let mut vectors: HashMap<RowId, Vec<f32>> = HashMap::new();
        let meta = {
            let ivf =
                DiskIvfIndex::create(2, &sample, 8, 4, 8, Metric::Euclidean, &mut pool, &mut wal)
                    .unwrap();
            for (i, v) in sample.iter().enumerate() {
                let r = rid(i as u32);
                vectors.insert(r, v.clone());
                ivf.insert(r, v, &mut pool, &mut wal).unwrap();
            }
            ivf.meta_page()
        };

        // Fresh handle over the same meta page — nothing rebuilt.
        let reopened = DiskIvfIndex::open(meta, DEFAULT_PAGE_SIZE as usize);
        let query = vec![150.0, -150.0];
        let results = reopened
            .search(&query, 1, Some(8), &mut pool, |r| vectors.get(&r).cloned())
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, rid(150));
    }

    /// An index created on an empty table (nlist=1 origin cell) still returns
    /// exact results — inserts all land in the single cell, exact re-rank does
    /// the rest.
    #[test]
    fn empty_table_index_is_flat_but_correct() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        let ivf =
            DiskIvfIndex::create(2, &[], 16, 8, 5, Metric::Euclidean, &mut pool, &mut wal).unwrap();
        assert_eq!(ivf.load_header(&mut pool).unwrap().nlist, 1);

        let mut vectors: HashMap<RowId, Vec<f32>> = HashMap::new();
        for i in 0..50u32 {
            let v = vec![i as f32, i as f32];
            vectors.insert(rid(i), v.clone());
            ivf.insert(rid(i), &v, &mut pool, &mut wal).unwrap();
        }
        let results = ivf
            .search(&[0.0, 0.0], 3, None, &mut pool, |r| {
                vectors.get(&r).cloned()
            })
            .unwrap();
        let ids: Vec<u32> = results.iter().map(|(r, _)| r.page_id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    /// Remove drops a point from a cell's posting list.
    #[test]
    fn remove_drops_candidate() {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();

        let sample: Vec<Vec<f32>> = (0..40).map(|i| vec![i as f32, i as f32]).collect();
        let ivf = DiskIvfIndex::create(2, &sample, 4, 4, 5, Metric::Euclidean, &mut pool, &mut wal)
            .unwrap();
        let mut vectors: HashMap<RowId, Vec<f32>> = HashMap::new();
        for (i, v) in sample.iter().enumerate() {
            vectors.insert(rid(i as u32), v.clone());
            ivf.insert(rid(i as u32), v, &mut pool, &mut wal).unwrap();
        }
        let target = vec![10.0, 10.0];
        let before = ivf
            .search(&target, 1, None, &mut pool, |r| vectors.get(&r).cloned())
            .unwrap();
        assert_eq!(before[0].0, rid(10));

        ivf.remove(rid(10), &target, &mut pool, &mut wal).unwrap();
        vectors.remove(&rid(10));
        let after = ivf
            .search(&target, 1, None, &mut pool, |r| vectors.get(&r).cloned())
            .unwrap();
        assert_ne!(after[0].0, rid(10), "removed point must not resurface");
    }
}
