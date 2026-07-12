# MVCC visibility anomaly under concurrent SQL writes (root-cause + fix)

**Type:** Improvement
**Status:** NOT STARTED

> **Naming note (why "#16" appears in several places).** This is backlog item
> **16** in [`backlog_index.md`](backlog_index.md). It is **unrelated to PR
> #45** — that PR shipped item **17** (`17_mm_replaced_stack_headline.md`);
> its PR *body* and its `item16_*` crash-test names are stale labels from
> before that work was renumbered 16 → 17. Everything about the anomaly
> itself lives here and in the cross-references below.

## Symptom (what a user would see)

Under concurrent load, a SQL scan that overlaps a committing cross-row
UPDATE transaction can return a **wrong result set**:

- an **extra row version** — a superseded version visible alongside its
  successor (the original sighting: `SELECT` returning **3 rows instead
  of 2**; also observed as `COUNT(*)` = 9 on an 8-row table), or
- a **missing live row** — a short scan / torn read (`COUNT(*)` = 7 on an
  8-row table; a bank-transfer reader seeing 7 accounts summing 699 instead
  of 8 summing 800), and
- after vacuum races cross-row churn, **duplicate visible ids that persist
  at quiescence** (still there after writers stop and a final `vacuum()`) —
  corruption, not just a stale read.

## Discovery & evidence timeline

1. **2026-07-11** (item-12 verification): `tests/concurrent_writers.rs::
   cross_row_update_deadlock_resolves_no_hang` (toggle **on**) intermittently
   ended with 3 visible rows instead of 2 — only under CPU contention (6
   parallel test-binary instances), never in isolation. Reproduced on
   unmodified `main` @ `dc93931`. Initially believed confined to the
   `UNIDB_CONCURRENT_SQL_WRITES` toggle path.
2. **2026-07-12** (concurrency correctness matrix, PR #46,
   `benches/conc_matrix.rs`, run via `scripts/report.sh --conc`): the anomaly
   family is **NOT gated on the toggle** — the production default (off) is
   affected. Release build, native macOS (M5 Pro, 18 cores), `main` @
   `0c09a70`:

   | shape | toggle | observed |
   |---|---|---|
   | readers-during-churn, RC reader | **off** | FAIL 2/3 — `COUNT(*)`=7, expected 8 |
   | transfer-sum, RC reader | **off** | FAIL 1/3 (7/10 focused) — 7 accounts / sum 699 |
   | vacuum × cross-row churn | **off** | 3/10 focused — duplicates persist post-quiescence |
   | cross-row churn 8w×8rows | **off** | 1/6 focused — post-quiescence duplicate ids |
   | cross-row churn 2w×2rows (original geometry) | on | FAIL 2/3 with spinners (6/6 PASS without — why the shipped test looked reliable) |
   | cross-row churn 8w×8rows, indexed **and unindexed** | on | FAIL 3/3 — duplicate ids |
   | readers-during-churn RC/RR/SER | on | FAIL 2–3/3 — `COUNT(*)`=9 or a live row missing |
   | parallel-scan-path reader | on | FAIL 1/3 — 1 row missing from a 3000-row scan |
   | 8w indexed churn (one focused run) | on | `Recovery("D5 violation on flush: page LSN > durable WAL LSN")` at commit |
   | 8w indexed churn (one full-matrix run) | on | hang > 120 s (deadlock/livelock) |

   Official full-matrix run: **17 PASS · 11 FAIL of 28 cells** (3
   repeats/cell, 18 contention spinners).

## Three symptom classes to explain (may be 1–3 distinct bugs)

1. **MVCC visibility around commit-during-scan** (the family core): a scan
   concurrent with a multi-statement cross-row UPDATE commit either sees the
   superseded version too, or misses the live row. Fails **unindexed** as
   well ⇒ the Item-A crabbing B-tree is not the (only) culprit. The
   persistent post-vacuum duplicates suggest the wrong version can also
   survive reclamation.
2. **D5 flush-ordering violation** at commit (locked invariant, §3 D5) —
   possibly a separate bug in the eviction-forced-sync path under the
   concurrent-writes toggle.
3. **Deadlock/livelock hang** (> 120 s) under heavy contention with the
   toggle on — the wait-for-graph detector failed to break or a livelock
   formed.

## Repro (deterministic enough to bisect)

```bash
scripts/report.sh --conc                                   # full 28-cell matrix (~5–10 min)
CONC_ONLY=readers-during CONC_REPEATS=10 scripts/report.sh --conc   # fastest loop, toggle-on 3/3
CONC_ONLY=transfer      CONC_REPEATS=10 scripts/report.sh --conc    # toggle-OFF 7/10
CONC_ONLY=vacuum-churn  CONC_REPEATS=10 scripts/report.sh --conc    # persistent-corruption case
```

## Definition of done

- Root cause identified and written up here (dated, inline — per
  `CONVENTIONS.md` corrections rule) for **each** of the three symptom
  classes (or evidence they share one cause).
- Fix lands with the full concurrency matrix **green at
  `CONC_REPEATS≥10` with spinners**, toggle off AND on; crash harness green;
  no §3 decision reopened without recorded sign-off (D5 especially).
- `tests/concurrent_writers.rs` gains a geometry that fails pre-fix without
  external load (the matrix showed 8w×8rows + readers is that geometry).
- Only then may item 11's planned `UNIDB_CONCURRENT_SQL_WRITES` default-ON
  flip proceed (it stays **default-off** until this ships). The toggle-off
  findings mean the fix is a production-correctness item, not just a
  flip-blocker.

## Cross-references

- `index_write_concurrency.md` — "Known issue found post-ship" section
  (original sighting + matrix results tables; the toggle's design and
  validation strategy).
- `benches/conc_matrix.rs` + `scripts/report.sh --conc` (PR #46) — the
  reproducer/regression harness; report lands in
  `docs/performance/conc_matrix_<ts>.md` (git-ignored).
- `PROGRESS.md` "Index & heap write concurrency" — the item-11 unit whose
  toggle path first exposed the family.
