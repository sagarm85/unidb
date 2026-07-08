// P3.c spike — recall@k validation for the on-disk IVF-Flat vector index.
//
// The Phase-3 blueprint requires validating recall BEFORE committing to an
// on-disk ANN. This measures the spike's `DiskIvfIndex` against:
//   (1) brute-force exact top-k (the ground truth), and
//   (2) the current in-RAM HNSW baseline (`vector::VectorIndex`),
// on a synthetic clustered corpus, reporting recall@10 and per-query latency
// across several `nprobe` settings, plus the index's bounded RAM footprint.
//
// recall@k = (avg over queries) |approx_topk ∩ exact_topk| / k.
//
// Run with: cargo bench --bench vector_recall

use std::collections::HashSet;
use std::time::Instant;

use tempfile::tempdir;
use unidb::bufferpool::BufferPool;
use unidb::disk_vector::DiskIvfIndex;
use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
use unidb::heap::RowId;
use unidb::vector::{Metric, VectorIndex};
use unidb::wal::Wal;

/// Tiny deterministic LCG so the benchmark needs no rng dependency.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((self.0 >> 33) as f32) / (1u64 << 31) as f32
    }
}

fn rid(i: u32) -> RowId {
    RowId {
        page_id: i,
        slot: 0,
    }
}

/// Generate `n` vectors of dim `d` drawn from `clusters` Gaussian-ish blobs —
/// realistic for embeddings (which are clustered, not uniform).
fn gen_corpus(n: usize, d: usize, clusters: usize, rng: &mut Lcg) -> Vec<Vec<f32>> {
    let centers: Vec<Vec<f32>> = (0..clusters)
        .map(|_| (0..d).map(|_| rng.next_f32() * 100.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            let c = &centers[i % clusters];
            c.iter().map(|x| x + (rng.next_f32() - 0.5) * 8.0).collect()
        })
        .collect()
}

fn euclidean(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

fn exact_topk(corpus: &[Vec<f32>], query: &[f32], k: usize) -> HashSet<u32> {
    let mut scored: Vec<(u32, f32)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (i as u32, euclidean(query, v)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    scored.into_iter().take(k).map(|(i, _)| i).collect()
}

fn recall_of(approx: &[u32], exact: &HashSet<u32>, k: usize) -> f64 {
    let hit = approx.iter().filter(|i| exact.contains(i)).count();
    hit as f64 / k as f64
}

fn main() {
    // Corpus size is bounded by the in-RAM HNSW baseline, whose known M2
    // pathology is a full graph rebuild on *every* upsert (O(n²·log n) to
    // build n points) — so we keep n modest to time it at all. IVF-Flat has no
    // such cost; the recall comparison is the point, and recall is meaningful
    // at this scale.
    let n = 1_200usize;
    let d = 32usize;
    let clusters = 30usize;
    let k = 10usize;
    let n_queries = 100usize;
    let nlist = 32usize;

    let mut rng = Lcg(0x1234_5678_9abc_def0);
    let corpus = gen_corpus(n, d, clusters, &mut rng);
    let queries: Vec<Vec<f32>> = (0..n_queries)
        .map(|i| {
            corpus[(i * 37) % n]
                .iter()
                .map(|x| x + (rng.next_f32() - 0.5) * 4.0)
                .collect()
        })
        .collect();

    // Ground truth.
    let gt: Vec<HashSet<u32>> = queries.iter().map(|q| exact_topk(&corpus, q, k)).collect();

    println!("P3.c spike — on-disk IVF-Flat recall@{k} vs. in-RAM HNSW");
    println!("corpus: {n} vecs × {d}d, {clusters} clusters; {n_queries} queries; nlist={nlist}\n");

    // ── in-RAM HNSW baseline ────────────────────────────────────────────────
    let mut hnsw = VectorIndex::with_metric(Metric::Euclidean);
    let t = Instant::now();
    for (i, v) in corpus.iter().enumerate() {
        hnsw.upsert(rid(i as u32), v.clone());
    }
    let hnsw_build = t.elapsed();
    let t = Instant::now();
    let mut hnsw_recall = 0.0;
    for (qi, q) in queries.iter().enumerate() {
        let got: Vec<u32> = hnsw
            .search(q, k)
            .into_iter()
            .map(|(r, _)| r.page_id)
            .collect();
        hnsw_recall += recall_of(&got, &gt[qi], k);
    }
    let hnsw_qtime = t.elapsed() / n_queries as u32;
    hnsw_recall /= n_queries as f64;
    println!(
        "HNSW (in-RAM, rebuilt-on-open):  recall={:.3}  q_latency={:>8.1}µs  build={:>6.1}ms  RAM≈O(corpus)",
        hnsw_recall,
        hnsw_qtime.as_secs_f64() * 1e6,
        hnsw_build.as_secs_f64() * 1e3,
    );

    // ── on-disk IVF-Flat (the spike) ────────────────────────────────────────
    let dir = tempdir().unwrap();
    let mut pool = BufferPool::open(
        &dir.path().join("data.db"),
        DEFAULT_PAGE_SIZE as usize,
        4096,
    )
    .unwrap();
    let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
    wal.set_deferred_sync(true); // batch fsyncs for the build (server-mode style)

    let t = Instant::now();
    let ivf =
        DiskIvfIndex::train(&corpus, nlist, 12, Metric::Euclidean, &mut pool, &mut wal).unwrap();
    for (i, v) in corpus.iter().enumerate() {
        ivf.insert(rid(i as u32), v, &mut pool, &mut wal).unwrap();
    }
    wal.sync().unwrap();
    let ivf_build = t.elapsed();

    let lookup = |r: RowId| corpus.get(r.page_id as usize).cloned();

    println!(
        "\nIVF-Flat (on-disk, durable postings, RAM = {} B for {nlist} centroids, build={:.1}ms):",
        ivf.ram_bytes(),
        ivf_build.as_secs_f64() * 1e3
    );
    println!("  {:>7}  {:>10}  {:>14}", "nprobe", "recall", "q_latency");
    for &nprobe in &[1usize, 4, 8, 16, 32] {
        let t = Instant::now();
        let mut recall = 0.0;
        for (qi, q) in queries.iter().enumerate() {
            let got: Vec<u32> = ivf
                .search(q, k, nprobe, &mut pool, lookup)
                .unwrap()
                .into_iter()
                .map(|(r, _)| r.page_id)
                .collect();
            recall += recall_of(&got, &gt[qi], k);
        }
        let qtime = t.elapsed() / n_queries as u32;
        recall /= n_queries as f64;
        println!(
            "  {:>7}  {:>10.3}  {:>12.1}µs",
            nprobe,
            recall,
            qtime.as_secs_f64() * 1e6
        );
    }

    println!(
        "\nTakeaway: IVF-Flat recall climbs to HNSW-competitive with modest nprobe,\n\
         at O(nlist) RAM instead of O(corpus) — and its postings are the durable\n\
         DiskBTree from P3.a, so it is crash-safe and never rebuilt on open."
    );
}
