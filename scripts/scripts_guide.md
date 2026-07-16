# `scripts/`

Utility scripts for unidb. **If you just want the benchmark report, run
`scripts/report.sh` — nothing else.** The rest of this file explains that command
and catalogs every other script so none of them is a mystery.

---

## Generate the benchmark report (start here)

One command, self-contained output (you never need to open another doc to read it):

```bash
scripts/report.sh
```

It **auto-selects the environment**:

- **Docker running** → fair-fsync comparison on Linux, where unidb *and* Postgres
  both use plain `fsync()` (the honest apples-to-apples plane). Recommended, and
  the default when Docker is available.
- **No Docker** → native run on this host (still valid; on macOS the report notes
  unidb uses `F_FULLFSYNC` while Postgres-default does not).

Force a mode with `scripts/report.sh --docker` or `--native`; `--help` prints usage.

**Every report also gets a “Concurrency correctness matrix” section appended** —
a pass/fail table of production-shaped concurrent read/write border cases
(cross-row UPDATE churn = the backlog-item-16 anomaly shape, readers-during-writes
at RC/RR/SERIALIZABLE, same-row contention, mixed CRUD, balance-transfer sum
invariance, vacuum interleaved with churn, delete+reinsert slot reuse), swept
across the `UNIDB_CONCURRENT_SQL_WRITES` toggle and indexed/unindexed tables,
under CPU contention with repeats. It always runs natively on the host (it
checks correctness invariants, not fsync-fair timing). Run it standalone with:

```bash
scripts/report.sh --conc            # matrix only → docs/performance/conc_matrix_<ts>.md
CONC_REPEATS=10 scripts/report.sh --conc   # tighten the intermittency net
```

### Tuning the workload (env vars)

| Var | Default | Controls |
|---|---|---|
| `MM_SIZES` | `1000,10000,100000` | Row counts for the W0→W4 ladder (Tables 1–2). |
| `MM_SAMPLE` | `200` | Marginal commits averaged per ladder point. |
| `MM_CRUD_ROWS` | `100000` | Rows pre-loaded for the CRUD suite (Table 3). |
| `MM_BULK_SIZES` | `10000,1000000,2000000` | Row counts for the bulk insert+scan stress sweep (Table 3.1). Default tops out at 2M; push to `5000000`/`10000000` for a heavier run (5M ≈ 2.7 min insert/engine). |
| `MM_TX_SWEEP` | `1000,10000,100000,1000000` | Tx counts for the multi-model-vs-Postgres sweep (Table 4). |
| `MM_REPLACED_STACK` | _(unset)_ | `1` → Table 4 adds the §6 replaced-stack column (row + pgvector + graph + queue as four independent commits, no shared txn) + a crash-consistency verdict. Needs a pgvector-enabled Postgres (`CREATE EXTENSION vector`); the Docker image already has it. |
| `CONC_REPEATS` | `3` | Repeats per concurrency-matrix cell (a cell FAILs if any repeat violates its oracle). |
| `CONC_SPIN` | `= cores` | CPU-contention spinner threads during the matrix (`0` disables). |
| `CONC_ROUNDS` | `1` | Concurrency-matrix workload-size multiplier. |
| `CONC_ONLY` | _(unset)_ | Substring filter on matrix scenario ids (e.g. `churn`). |
| `CONC_SKIP` | _(unset)_ | `1` skips the matrix (perf-only report). |
| `CONC_STRICT` | _(unset)_ | `1` makes the script exit nonzero if any matrix cell fails (CI). |

```bash
# Push the multi-model sweep to millions (slow — synchronous HNSW at scale):
MM_TX_SWEEP=10000,100000,1000000 scripts/report.sh

# Quick smoke:
MM_SIZES=1000 MM_SAMPLE=30 MM_CRUD_ROWS=5000 MM_BULK_SIZES=10000,100000 MM_TX_SWEEP=1000,10000 scripts/report.sh
```

### Where the output lands

| Mode | Output |
|---|---|
| **Docker** | `docker/out/report_<timestamp>.md` (+ `phases.csv`, `stats.csv`, run logs) |
| **Native** | `docs/performance/report_<timestamp>.md` |

Docker-run output (`docker/out/`) is **git-ignored** (run artifact). The dated
**native** reports under `docs/performance/` **are committed** — the durable
measurement record alongside `PROGRESS.md`.

### What the report contains

- **Table 1–2** — W0→W4 decomposition ladder: the per-commit cost of adding each
  model (btree, vector/HNSW, graph edge, event) to a relational write, and how it
  moves with table size.
- **Table 3** — CRUD stress: unidb (SQL) vs Postgres (relational) across bulk
  INSERT, filtered/grouped SELECT, bulk UPDATE, selected/full DELETE — with the
  record count each op touches and a **remark** naming the winner and its margin.
- **Table 3.1** — bulk stress: a fresh table loaded then full-scanned at 10k → 2M
  rows by default (`MM_BULK_SIZES`; 5M/10M available on request), reporting unidb-vs-Postgres insert and scan throughput
  (matched load method) plus the per-metric winner and margin.
- **Table 4** — unidb multi-model (one transaction: relational + vector + graph +
  event) vs Postgres relational, swept across tx counts to millions.
- **CPU / Memory** — per-phase `docker stats` for the unidb and Postgres
  containers (Docker mode), with the embedded-vs-server asymmetry stated plainly.
- **Concurrency correctness matrix** — the pass/fail border-case table described
  above (`benches/conc_matrix.rs`), appended to every report unless `CONC_SKIP=1`.

The fair-fsync rationale and its caveats live in [`../docker/fair_fsync_benchmark.md`](../docker/fair_fsync_benchmark.md).

---

## Every script in this folder

### Run these

| Script | What it does |
|---|---|
| **`report.sh`** | **The benchmark-report entry point** (above). Auto-picks Docker or native; drives everything else. |
| `pg_compare.sh` | Brings up a throwaway Postgres (native `initdb`, or `--docker`) and runs the unidb-vs-Postgres comparison standalone. `report.sh --docker` supersedes this for the full report; keep it for a focused PG-only comparison or to reuse an existing `PG_URL`. |
| `bench_server.sh` | Smoke-tests a **running** `unidb-server` over real HTTP (curl/openssl only, no Rust toolchain) — rough p50/p99 + concurrent throughput. Not a substitute for `cargo bench --bench server`. |
| `gen_jwt.sh` | Mints a verify-only HS256 JWT for `unidb-server`'s auth (pure bash + openssl, no PyJWT). Used with `bench_server.sh` or manual `curl`. |

### Machinery (driven by `report.sh` — you don't run these directly)

| Script | Role |
|---|---|
| `multi_model_report.sh` | The **native** report engine: builds the `decompose` bench, runs its `mmreport` mode, captures peak RSS, assembles the markdown. Invoked by `report.sh --native`. |
| `docker_report.sh` | The **Docker** report runner: builds the image, brings up Postgres + the bench on Linux, samples `docker stats` for CPU/mem, and post-processes. Invoked by `report.sh --docker`. |
| `mm_resource_report.py` | Correlates the bench's phase windows (`phases.csv`) with `docker stats` samples (`stats.csv`) into the per-phase CPU/memory table. Called by `docker_report.sh`. |

---

## Where local data goes

- The **benchmarks** use throwaway temp dirs (`tempdir()`), auto-cleaned — nothing
  is written into the working tree.
- The **`unidb-server`** binary defaults `UNIDB_DATA_DIR` to **`/tmp/unidb`** (not
  the repo), so a local/dev server never litters the tree with `control`/`data.db`/
  `db.wal`. `/tmp` is ephemeral across reboots — set `UNIDB_DATA_DIR` to a real
  volume for anything you want to keep.
