# P3.c spike — on-disk vector index: approach selection & recall validation

> **Status: SPIKE COMPLETE (2026-07-08).** This document records the spike the
> Phase-3 blueprint mandates ("SPIKE FIRST … validate recall@k before
> committing") for the durable on-disk vector index. The production wiring
> (`CREATE INDEX … USING HNSW` → durable index, `NEAR` reading it, crash point)
> is the **follow-up PR**, deliberately not rushed. Prototype:
> `src/disk_vector.rs`; validation harness: `benches/vector_recall.rs`.

## Problem

Today the vector index (`vector.rs`, `instant-distance` HNSW) is **in-RAM and
rebuilt on open** — O(corpus) startup and RAM, and it OOMs at the 100s-of-GB
target. Phase 3 needs it durable: bounded RAM, no full rebuild, crash-safe. An
HNSW graph does not page cleanly (its edges are random-access pointers all over
the graph), so a different structure is needed.

## Candidates considered

| Approach | Pages cleanly? | RAM | Recall | Build/update | Complexity |
|---|---|---|---|---|---|
| **On-disk IVF-Flat** | **Yes** — cell posting lists are `key→[RowId]` | O(nlist) centroids | tunable via `nprobe`; exact re-rank | k-means once + O(1) assign per insert | **Low** (reuses `DiskBTree`) |
| On-disk IVF + PQ | Yes | O(nlist)+codebooks | slightly lower (quantized) | + PQ training | Medium |
| DiskANN / Vamana | Partially (one on-disk graph + PQ in RAM for routing) | O(corpus·PQ) | Highest at low latency | Complex robust-prune graph build; hard updates | **High (research-grade)** |

## Decision: IVF-Flat for v1

The spike prototypes **on-disk IVF-Flat** and recommends it as the v1 durable
vector index, because:

1. **It reuses everything P3.a/P3.b already built.** An IVF index's only on-disk
   state is a posting list **`cell_id → [RowId]`** — exactly a `DiskBTree`
   (P3.a). So it is *already* durable, WAL-logged, crash-recovered, buffer-pool-
   managed, and vacuum-scrubbable, with no new storage machinery. The only new
   in-RAM state is the centroid table (`nlist·dim` floats — **bounded,
   independent of corpus size**), which is the whole point vs. the O(corpus)
   HNSW graph.
2. **Recall is a tunable, measurable function of `nprobe`** (see below), and
   IVF-Flat re-ranks candidates with **exact** distances fetched from the heap,
   so there is no quantization error — appropriate at the single-node target.
3. **DiskANN/Vamana is deferred, not rejected.** It offers better recall at very
   low latency, but its robust-prune graph construction and update story are
   genuinely research-grade; the blueprint explicitly says not to rush this.
   IVF-Flat is the pragmatic v1; DiskANN can supersede it later behind the same
   index interface if a workload demands it.

## Recall validation (`benches/vector_recall.rs`)

Synthetic clustered corpus (embeddings are clustered, not uniform): **1,200
vectors × 32 dims, 30 clusters, 100 queries, k=10, nlist=32**. Ground truth is
brute-force exact top-10. recall@10 = avg |approx ∩ exact| / 10.

| index | recall@10 | q-latency | build | RAM |
|---|---|---|---|---|
| **HNSW** (in-RAM, rebuilt-on-open) | 1.000 | ~26 µs | **30,223 ms** | O(corpus) |
| **IVF-Flat** `nprobe=1` | 0.957 | 8.4 µs | 24 ms | **4,096 B** (32 centroids) |
| **IVF-Flat** `nprobe=4` | **1.000** | 31 µs | 24 ms | 4,096 B |
| **IVF-Flat** `nprobe=8` | 1.000 | 59 µs | 24 ms | 4,096 B |
| **IVF-Flat** `nprobe=16` | 1.000 | 113 µs | 24 ms | 4,096 B |
| **IVF-Flat** `nprobe=32` (all cells) | 1.000 | 216 µs | 24 ms | 4,096 B |

Two things stand out. **(1)** IVF-Flat reaches the exact top-10 (recall 1.000)
at `nprobe=4` — a handful of the 32 cells — at O(nlist) RAM (4 KB) versus HNSW's
O(corpus). **(2)** The in-RAM HNSW *build* took **30 s for 1,200 vectors** — the
known M2 rebuild-per-upsert pathology (a full graph rebuild on every insert),
which is exactly the O(corpus)-on-open cost Phase 3 exists to kill; the durable
IVF build was **24 ms**. (An earlier spike run capped IVF recall at 0.912 even at
`nprobe=32` — that surfaced a real `DiskBTree` duplicate-key bug, since fixed:
`search_eq`/`remove` now descend to the leftmost leaf and walk the leaf links, so
a duplicate run straddling a leaf boundary — a hot cell, a common token, a graph
hub — is fully returned. Regression test:
`btree_index::heavily_duplicated_key_spanning_leaves_returns_all`.)

**Reading the numbers:** IVF-Flat recall climbs to HNSW-competitive with a
modest `nprobe` (a handful of the 32 cells), while its RAM footprint is the
centroid table only (bytes, not O(corpus)) and its postings live in the durable
`DiskBTree`. This validates the approach: **recall is acceptable and tunable,
RAM is bounded, and the on-disk structure is already crash-safe.**

## What the production follow-up PR adds (not in this spike)

- Persist centroids in a meta page (spike keeps them in RAM); re-train as a
  maintenance op (like vacuum), not on every open.
- Wire `CREATE INDEX … USING HNSW` (or a new `USING IVF`) to build a
  `DiskIvfIndex`, store its meta page in `ColumnDef.index_root`, and route
  `NEAR` through it; retire the async index worker (its last user).
- A crash-injection point (P17) for the durable vector index. (P16 is taken by
  P3.d large objects.)
- `nlist`/`nprobe` as index parameters; a larger-corpus recall/latency sweep.

## Bottom line

On-disk IVF-Flat clears the Phase-3 bar (bounded RAM, no rebuild-on-open,
crash-safe, competitive recall) with the least new code because its durable core
*is* the P3.a `DiskBTree`. Recommended for the production PR; DiskANN parked as a
later, higher-recall option behind the same interface.
