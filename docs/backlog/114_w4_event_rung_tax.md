# Item 114 — W4 residual multi-model tax: event rung + commit-path vector residue

**Type:** Performance
**Status:** 🔄 IN PROGRESS — Step-0 attribution A/B running 2026-07-24
(`UNIDB_BENCH=item114_step0` mode added to `benches/decompose.rs`: ladder
rungs W1–W4 at 100k in both configs — async worker [07-23/shipping] vs sync
fallback [07-21] — on one commit; Δevent and Δvector compared across configs).
Filed 2026-07-23 from the first official post-107 ladder record
(`docs/performance/report_20260723_124415.md`).

## Evidence (2026-07-23 bench, main `0324dc5`, canary quiet vs 07-21)

Item 107's async HNSW worker did what it promised: Δvector (W2−W1) at 100k
fell **+17.55 → +3.31 ms/commit**, W4/W0 at 100k **96.01× → 34.21×**, with the
background drain honestly accounted off the commit path (8.75–17.86 ms/commit
at 100k). But the ladder did not collapse to the ≈1.5× multi-model-tax target,
and the residue decomposes into two unexplained costs:

| Δ per commit @100k | 07-21 (sync HNSW) | 07-23 (async) | note |
|---|---:|---:|---|
| Δ vector (W2−W1) | +17.55 ms | **+3.31 ms** | async, but why not ≈ queue-append cost (~µs)? |
| Δ edge (W3−W2) | −0.07 ms | +0.75 ms | noise-band |
| Δ event (W4−W3) | +4.08 ms | **+9.93 ms** | 2.4× the 07-21 cost — now the dominant rung |

W4/W0 at 1000 improved (19.49→16.34×) and at 10000 regressed slightly
(17.61→20.55×); 100k is the signal point.

## Questions Step-0 must answer (attribution before optimization — §0.6)

1. **Why does the vector rung still cost +3.31 ms on the commit path when the
   worker is active?** Candidates: bounded-queue backpressure, cache/arena
   merge work on the foreground path, worker CPU contention with the
   foreground (the drain at 100k is 8.75–17.86 ms/commit of background work
   on the same cores — the M2.d lesson: "off the blocking path" ≠ free).
2. **Why did Δevent more than double at 100k?** The event rung existed
   unchanged before item 107; prime suspect is the same CPU contention — the
   W4 rung now runs while the worker drains vector backlog — but it could
   also be a real regression in the event append path. A/B: rerun the W3→W4
   rung with `UNIDB_HNSW_ASYNC` off (sync fallback) or after forcing a full
   drain, and compare Δevent.
3. Only after attribution: pick the lever (throttle/priority for the worker,
   batch the drain, or event-path fix) and set a measurable W4/W0 acceptance.

## Acceptance (provisional, revise after Step-0)

- Δevent at 100k back to ≤ its 07-21 cost (+4 ms) or attributed + accepted
  with sign-off.
- W4/W0 at 100k materially below 34× with recall/freshness contracts intact.
- Certified in a full Docker bench (item 105 carry-forward rules apply —
  this touches shared layers, so no stitching).
