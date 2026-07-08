# M9 — Python embedded bindings (PyO3, in-process)

## Status as of 2026-07-08: NOT STARTED (backlog).

Parked here as a durable reference. Do not begin implementing until
explicitly told to resume. This is a distinct, larger effort than the
Python *REST client* that M8's backlog note mentioned — see "Scope: what
this is and isn't" below.

## Context

Request: "use the engine as a library" from Python — i.e. embed
`unidb::Engine` **in-process**, no server, no HTTP. The Python analog of
`sqlite3`/`duckdb`: `import unidb; db = unidb.Engine("./mydata")`. This is
NOT the same as a Python client talking to a running `unidb-server` (that
is the REST-client item still parked from M8's backlog note, and is a much
smaller, non-native effort).

Why it's a clean fit (verified against the current `Engine`, not assumed):
- **`Engine` is already `Send`** — compiler-verified via a type assertion
  in `src/lib.rs` (the M5 writer-thread design needed it; the doc comment
  there explains the assertion turns "believed `Send`" into "checked every
  build"). So a PyO3 `#[pyclass]` wrapper works **without** needing
  `unsendable`.
- **`Engine` is deliberately NOT `Sync`, and is single-owner for its whole
  lifetime.** PyO3's model fits this exactly: a `#[pyclass]` is accessed
  only with the GIL held, and mutation goes through `PyRefMut` (runtime
  borrow-check, RefCell-like). One thread touches it at a time — the "one
  thread owns it" invariant is preserved, not violated.
- **The public API is small and synchronous** (`src/lib.rs`): `open`,
  `begin`/`begin_with_isolation`/`commit`/`abort`, `execute_sql`/
  `execute_cypher`, raw CRUD (`insert`/`get`/`update`/`delete`), graph
  (`create_edge`/`delete_edge`/`edges_from`), indexing (`set_column_index`/
  `index_status`), events (`enable_events`/`poll_events`/`ack_events`/
  `vacuum_events`), `set_rls_policy`, `checkpoint`/`flush`.
- **The background index worker is Rust-side and never touches Python**, so
  there is no GIL entanglement with it.

## Scope: what this is and isn't

- **IS**: an in-process native extension — Python loads the compiled Rust
  engine directly; a single `unidb.Engine` object owns real storage files,
  runs real transactions, and does everything the embedded Rust crate does.
- **IS NOT**: a Python REST client (that is a separate, smaller backlog
  item — thin HTTP wrapper over `docs/REST_API.md`, no native build). If
  someone wants "talk to a shared server from Python," that's the other
  item, not this one.
- **The ergonomic payoff over the M8 REST client**: embedded gives *real*
  transactions, not one-shot calls. Python gets a genuine
  `begin`→work→`commit`/`abort`, best surfaced as a context manager (see
  API shape below). This is the main reason embedded is worth doing at all
  rather than just shipping a REST client.

## Key design decisions (proposed — confirm before building)

- **Separate workspace crate `unidb-py`**, `crate-type = ["cdylib"]`,
  depending on `unidb` as a path dep — exactly the isolation pattern
  `unidb-attach` established. PyO3 and its build machinery never enter the
  `unidb` crate's own dependency graph; `cargo tree -p unidb
  --no-default-features --edges normal` must stay clean of pyo3, same
  invariant M8 verified for `reqwest`.
- **`Engine` wrapped as a plain `#[pyclass]`** (not `unsendable`, since
  `Engine: Send`). Store it directly; PyO3's `PyCell`/`PyRefMut` provides
  the borrow-checking.
- **Two transaction surfaces, both offered:**
  - Explicit: `db.begin() -> Txn`, `txn.commit()`, `txn.abort()`, with the
    real `Xid` held Rust-side (Python never sees a raw xid integer it could
    misuse across engines).
  - Ergonomic: `with db.transaction() as txn:` — commits on clean exit,
    aborts on exception. This is the headline API and what docs should lead
    with.
  - A convenience `db.execute(sql)` that wraps a single statement in its
    own begin→commit (matching how the REST routes behave), for the common
    one-shot case.
- **Error mapping — a Python exception hierarchy, not raw `DbError`
  strings.** Base `unidb.DbError(Exception)` with subclasses mirroring the
  meaningful variants (`TableNotFound`, `ColumnNotFound`, `NotFound`,
  `TableAlreadyExists`, `WriteConflict`, `SerializationFailure`,
  `SqlParse`, `SqlPlan`, `SqlUnsupported`). Storage-internal variants
  (`PageNotFound`, `ChecksumMismatch`, WAL/control corruption) collapse to
  a single `unidb.InternalError` — they are not actionable from Python, the
  same judgment M8's `AttachError` made.
- **Type marshaling is the bulk of the work, not the wiring.** Define the
  round-trip once, in one module:
  - `Literal::Int` ↔ `int`, `Text` ↔ `str`, `Bool` ↔ `bool`,
    `Vector` ↔ `list[float]`, `Json` ↔ `dict`/`list` (via `json.loads`
    on the Rust JSON string, or a direct serde_json→PyObject conversion),
    `Null` ↔ `None`.
  - `ExecResult::Rows` → `list[dict[str, value]]` (column-named dicts is
    the Pythonic shape; also expose column order).
  - Raw CRUD (`insert`/`get` take/return `&[u8]`/`Vec<u8>`) ↔ `bytes`.
  - `Edge`/`RowId` → small frozen dataclasses or named tuples.
- **GIL release for heavy calls**: because `Engine: Send`, a heavy
  `execute_sql` *can* legitimately run inside `py.allow_threads(...)` so
  other Python threads proceed during the fsync-dominated commit. Verify
  the `Ungil + Send` bounds hold for `&mut Engine` during implementation;
  if they do, release the GIL around the blocking engine call. If a
  wrinkle appears, holding the GIL is an acceptable v1 fallback (matches
  M8's "blocking is fine" decision) — document whichever ships.
- **Packaging: maturin.** `pyproject.toml` with `maturin` as build backend,
  `abi3` wheels (single wheel across CPython versions) if PyO3's abi3
  feature cooperates with the API used. Wheels are the deliverable; do not
  vendor a Python-side pure package.

## Checkpoints (proposed)

- **M9.a** — `unidb-py` crate scaffold (workspace member, cdylib, pyo3 +
  maturin), `pyproject.toml`, `unidb.Engine.__init__(path, page_size=0)`
  over `Engine::open`, the exception hierarchy, and the single simplest
  method end-to-end (`execute` one-shot SQL) with the type-marshaling
  module stubbed for scalar `Literal`s only. Get `maturin develop` +
  `import unidb` + a create/insert/select round-trip green before anything
  else.
- **M9.b** — Full method surface: explicit `begin`/`Txn`/`commit`/`abort`,
  the `with db.transaction()` context manager, `execute_cypher`, raw CRUD
  (`bytes`), graph (`create_edge`/`delete_edge`/`edges_from`), indexing,
  events. Complete the type marshaling (vectors, JSON, edges).
- **M9.c** — pytest suite (round-trip every method against a real temp-dir
  engine; prove `with db.transaction()` aborts on exception and the row is
  gone; prove an aborted edge never surfaces — the Python-level mirror of
  `tests/graph_mvcc.rs`), a benchmark comparing embedded-Python-call
  overhead vs. the direct Rust `Engine` call (isolate PyO3 + marshaling
  cost — expected: small, and far below the M8 REST/HTTP overhead), wheels
  built via maturin, and `PROGRESS.md`/`MEMORY.md`/`README.md`/`docs/`
  closeout (per CLAUDE.md §9 doc-upkeep rule).

## Known limitations to document (anticipated)

- **One engine per process on one directory** — same single-writer,
  single-directory model as embedded Rust; no connection pooling, no
  sharing an `Engine` across Python processes (use the REST server for
  that). Opening the same directory from two processes is unsupported,
  exactly as in Rust.
- **`Engine` is pinned to the thread that created it in practice** — even
  though it's `Send`, the GIL + `PyRefMut` model means concurrent access
  from multiple Python threads serializes; there is no real intra-process
  parallelism for writes (there isn't in the Rust engine either — D-series
  single-writer design).
- **`flush` is test-only** — expose it only if useful for tests, or omit
  (matching how M8 treated it).
- **No async/await API** — the engine is synchronous; an asyncio-friendly
  wrapper (run calls in a thread executor) is a future item, not v1.

## Backlog (explicitly deferred, not part of M9 v1)

- Node.js / other-language embedded bindings (napi-rs, etc.) — same
  approach, different FFI, whenever there's concrete demand.
- Python **REST** client (the still-open M8-era item) — orthogonal to this;
  ship independently if/when wanted.
- asyncio-compatible API layer over the sync bindings.
- Packaging to PyPI / CI wheel-build matrix (manylinux/macOS/Windows) —
  real distribution work beyond a locally-built wheel.
