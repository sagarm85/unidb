# Fair-fsync benchmark (Docker)

Runs the unidb-vs-Postgres **multi-model** comparison (`scripts/multi_model_report.sh`
→ `decompose` bench `mmreport` mode) inside Linux containers so both engines use
the **same `fsync()` durability primitive**.

## Why Docker for this

On macOS the two engines don't sync the same way:

| | macOS commit durability |
|---|---|
| unidb (Rust std `File::sync_all`) | `F_FULLFSYNC` → **platter-durable** |
| Postgres default (`fsync`/`open_datasync`) | `fsync(2)` → **cache only** |

So a naïve macOS "default vs default" comparison is *tilted against unidb* (it
pays for real durability; Postgres-default does not). On **Linux there is no
`F_FULLFSYNC`** — both engines use plain `fsync()`, which is the device-durable
primitive. Running both on Linux therefore makes the comparison apples-to-apples
by construction.

This also required a bench fix: `decompose`'s durable lens hard-coded
`wal_sync_method = 'fsync_writethrough'` (a macOS-only value). On Linux that
`ALTER SYSTEM` errored and the Postgres column was **silently skipped**. It now
picks the strongest method the server actually offers from `pg_settings.enumvals`
(`fsync_writethrough` on macOS, `fsync` on Linux).

## Run it

```bash
scripts/docker_report.sh                           # default sizes (1k,10k,100k)
MM_SIZES=100000,1000000 scripts/docker_report.sh   # push to millions
```

Output: `docker/out/report_<timestamp>.md`. Promoted reference runs are named `benchmark_<timestamp>.md`.

## What it measures

- **Tables 1–2:** unidb W0→W4 decomposition ladder at scale (the per-commit tax
  of adding vector + graph + event to a relational write, in one transaction).
- **Table 3:** single-model relational — **unidb (SQL) vs Postgres** — the honest
  peer workload, now both on Linux `fsync`.
- **Table 4:** the "one atomic transaction vs the replaced stack" framing. Set
  `MM_REPLACED_STACK=1` for the honest §6 headline — unidb's ONE atomic commit vs
  the **replaced stack** (Postgres row + pgvector + a graph table + an outbox
  queue) run as **four independent commits with no shared transaction** (4
  `fsync`s, no cross-system atomicity), plus a crash-consistency verdict (the
  stack recovers a torn record; unidb recovers 0 orphans — `tests/crash` item16
  proofs). The pgvector image ships the `vector` extension the vector role needs.
  Without the flag, Table 4 shows only the PG-relational single-model floor.

## Important caveat: absolute vs relative durability

- **Docker Desktop on macOS** runs the containers in a **Linux VM**. Both engines
  share that VM's virtual disk with identical fsync semantics, so the **unidb ÷ PG
  ratio is fair**. But whether an fsync reaches the Mac's *physical* platter is
  VM-dependent and unquantifiable — do **not** headline *absolute* durable
  throughput from a Mac.
- **Native Linux host / CI runner** gives both a fair ratio **and** honest
  absolute durability — that's the environment for publishable numbers.
