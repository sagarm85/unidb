# Backlog conventions — naming & lifecycle standard

> How to name and run every planning/spec doc in `docs/backlog/`. Follow this for
> **all new work** and when you touch an existing backlog file. Referenced from
> `CLAUDE.md` §9; the roadmap of *numbered* phases lives in `roadmap.md`.

There are four kinds of work. Pick one before you create the file.

| Type | What it is | Filename | Tracked in |
|------|------------|----------|------------|
| **Phase** | A **numbered** item on the roadmap (large, sequenced) | `phase<N>_<slug>.md` | `roadmap.md` + `PROGRESS.md` |
| **Milestone** | A large, independently-shippable **named** unit *not* on the numbered roadmap (its own design doc + PR(s)) | `<slug>.md` | `PROGRESS.md` |
| **Improvement** | A targeted feature / behavior / correctness change | `<slug>.md` | `PROGRESS.md` |
| **Performance** | A perf-focused effort measured against a baseline | `<slug>.md` | `PROGRESS.md` |

## Filename rule (the thing that was inconsistent)

- **`phase<N>_` is RESERVED for numbered roadmap phases** (`phase1_acid_hardening.md`
  … `phase6_ops_ha.md`). `<N>` is the roadmap number; `<slug>` is a short
  `snake_case` description.
- **Every other file uses a plain descriptive `snake_case` slug — no `phase` in
  the name.** e.g. `parallel_scan.md`, `autovacuum.md`, `crud_performance.md`,
  `commit_time_fsync.md`.
- **Never encode internal sub-parts in the filename.** A Performance/Milestone
  effort with sub-stages names them *inside* the doc — "Phase A / Phase B",
  "checkpoints A1 / B2", "P-a / P-b" — not `..._phaseA_B.md`. (This is exactly why
  `crud_performance_phaseA_B.md` was renamed to `crud_performance.md`: it is
  *Performance* work with internal Phase A/B stages, not a numbered roadmap phase.)
- Meta docs keep bare names: `roadmap.md`, `CONVENTIONS.md`,
  `engine_internals_doc_prompt.md`.

## Header (every backlog file starts with this)

```markdown
# <Title>

**Type:** Phase | Milestone | Improvement | Performance
**Status:** NOT STARTED | IN PROGRESS | SHIPPED (→ PROGRESS.md "<entry name>")
```

Adopt the header when you **create** a file or **next touch** an existing one
(don't churn all files at once). `crud_performance.md` and `parallel_scan.md`
carry the header as the exemplars.

## Lifecycle (all types)

1. **Create** the file at `NOT STARTED` with the plan/spec.
2. Flip `Status` → `IN PROGRESS` when work starts, → `SHIPPED (→ PROGRESS.md "…")`
   when the PR merges. Point at the `PROGRESS.md` entry, don't duplicate metrics.
3. **Metrics/outcomes live in `PROGRESS.md`** (one entry per shipped unit), never
   in the backlog file.
4. **Corrections are inline, dated, and additive** — never a silent rewrite (the
   evidence-based ethos of `CLAUDE.md` §0.5/§6). If the plan's premise turns out
   wrong, say so with a dated note and keep the original visible.

## How to run each type (process)

- **Phase** — sequence per `roadmap.md`; one PR per phase (checkpoints as ordered
  commits); a **benchmark table is mandatory** in the `PROGRESS.md` entry (§6).
- **Milestone** — its own design doc; may span multiple PRs. If it carries a real
  risk or "landmine", **surface it in the doc and de-risk it first** (see how
  `parallel_scan.md` states the pool/mmap question up front).
- **Improvement** — a single focused PR; record the before/after or the
  correctness proof in `PROGRESS.md`.
- **Performance** — **measurement-first (§6): every claim is a number, never
  asserted.** Instrument before changing code, use a matched/honest baseline, and
  put before→after in `PROGRESS.md`. Revise the *acceptance target* honestly if
  it proves architecturally unreachable (as `crud_performance.md` did for the
  UPDATE and filtered-SELECT targets) rather than reporting a flattering number.

## Commit / PR mapping (conventional commits)

- Phase / Milestone / Improvement → `feat:`; Performance → `feat:` or `perf:`.
- Measurement/benchmark-only changes → `bench:`; docs/closeout → `docs:`.
- One PR per Phase; a Milestone may be several PRs; the PR description carries the
  §6 metrics table + a peak-memory note.

## Cross-references (don't duplicate)

- `roadmap.md` — the numbered-phase plan and their order.
- `PROGRESS.md` — the shipped ledger (metrics). One entry per shipped unit.
- `MEMORY.md` — current running state + session log.
- `CLAUDE.md` — rules & locked decisions; §9 points here for backlog naming.
