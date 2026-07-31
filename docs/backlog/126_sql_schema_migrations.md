# SQL schema-migrations tooling

**Type:** Improvement
**Status:** SHIPPED (→ PROGRESS.md not required — task scope excluded it; see this file for verification detail, per the item-124/125 precedent)

> Workstream I4 (item 120's roadmap — `docs/backlog/120_supabase_parity_roadmap.md`,
> row I). Decision-gated; approved by the user with the design below.

## Design (as approved and implemented)

Supabase-style, forward-only SQL migrations applied in version order, tracked
in a table.

- **Migration files:** plain `.sql` files in a `migrations/` directory (path
  configurable — passed to `apply_migrations`/the CLI). Named
  `<version>_<name>.sql` where `version` sorts **lexicographically** (zero-
  padded sequence numbers or a sortable timestamp prefix). A file may contain
  multiple `;`-separated statements.
- **Tracking table:** `schema_migrations` (`CREATE TABLE`-if-not-exists
  semantics, emulated — the SQL grammar has no literal `IF NOT EXISTS`) with
  `version TEXT PRIMARY KEY, name TEXT, checksum TEXT, applied_at TIMESTAMP`.
  Maintained by the migrator itself.
- **Apply algorithm:** list migration files sorted by version; for each, if
  its version is not already in `schema_migrations`, execute its SQL (via
  the engine's existing `execute_sql`/`execute_sql_params` statement path)
  then insert a tracking row with a SHA-256 checksum of the file's exact
  bytes and `applied_at`. Already-applied versions are skipped (idempotent
  re-runs). **Drift detection:** an already-applied version whose file
  checksum differs from the recorded one STOPS the run with a clear error.
  **Ordering/gap:** ascending version order; stop on the first error and do
  NOT record the failed migration.
- **Surface:** `Engine::apply_migrations(dir: &Path) -> Result<MigrationReport>`
  (Rust API) plus a dedicated `unidb-migrate` CLI binary (not gated behind
  the `server` feature — it only opens the sync embedded `Engine`).

## The non-transactional-DDL caveat — investigated, not assumed

The task brief flagged unidb's DDL as "not fully transactional" (a `CREATE
TABLE` inside a transaction that later aborts is not rolled back —
`MEMORY.md`) and asked this module to document rather than paper over it.
Before writing that documentation, its exact scope was verified empirically
(three probe tests, since removed after confirming the finding — see
`src/migrations.rs`'s module doc for the permanent record):

1. **Within one `execute_sql` call spanning multiple `;`-separated
   statements**, a later statement's failure **does** roll back every
   earlier statement in that same call — DDL included (item P2.c's
   request-scoped `catalog_root` snapshot/restore). A multi-statement
   migration file is therefore atomic with respect to itself.
2. **Across separate `execute_sql` calls under the same `xid`**, a DDL
   statement that already succeeded in an earlier call is **not** undone
   by later aborting the `xid` — this is the real, narrower gap.

`apply_one_migration` executes the file's own SQL as ONE `execute_sql` call
(so #1 covers it fully), then records the tracking row via a **second**,
separate `execute_sql_params` call. The residual gap is exactly there: if
step 2 (the tool's own `INSERT`) fails after step 1 succeeded, step 1's DDL
is not undone and the migration is not recorded. This is documented in the
module doc, `docs/SCHEMA_MIGRATIONS.md`, and inline in the error message
returned in that case (distinct wording from a plain mid-file failure, which
needs no manual cleanup).

No rollback/savepoint engine was built to close that last narrow gap —
explicitly out of scope per the approved design ("Do NOT pretend
per-migration atomicity you can't deliver... do not over-engineer a
rollback engine").

## Files changed

- `src/migrations.rs` (new) — `MigrationFile`, `MigrationReport`,
  `Engine::apply_migrations`/`ensure_migrations_table`/
  `load_applied_migrations`/`apply_one_migration`, `list_migration_files`,
  `checksum_hex`. Module-doc-documents the caveat above; 4 unit tests.
- `src/bin/unidb-migrate.rs` (new) — CLI wrapper.
- `src/lib.rs` — `pub mod migrations;` + re-export `MigrationFile`/
  `MigrationReport`.
- `src/error.rs` — new `DbError::Migration(String)` variant.
- `src/server/error.rs` — exhaustive `map_status` match arm for the new
  variant (mechanical; no route drives it yet — `/rest/v1` etc. untouched).
- `Cargo.toml` — new `[[bin]] unidb-migrate` (default features, no `server`
  requirement); reuses the existing `sha2` dependency (item 121 A4).
- `tests/item126_migrations.rs` (new) — 6 integration tests: object
  creation + tracking-table population; idempotent re-run; checksum-drift
  error; ascending-version-order application (including a genuine
  cross-migration data dependency); failing-migration stop/non-record/
  intra-file-non-leak with earlier migrations staying applied (plus a
  fix-and-retry follow-up); malformed-filename/duplicate-version rejection.
- `README.md` — new "Schema migrations" section + project-layout entries.
- `docs/SCHEMA_MIGRATIONS.md` (new) — full reference (naming, algorithm,
  the caveat, CLI/API usage).
- `docs/documentation_index.md` — registers the new doc under "For
  operators".

## Verification

- `cargo build --all-features`, `cargo clippy --all-features -- -D
  warnings`, `cargo fmt --all --check` — clean.
- `cargo test --test crash` — 54/54.
- `cargo test --lib` — 514/514 (includes 4 new migrations unit tests).
- `cargo test --test item126_migrations` — 6/6.
- `cargo test --features server --test server_crud --test server_auth` —
  9/9 (spot-check that the `server/error.rs` exhaustiveness fix didn't
  regress the server feature; full `cargo test`/`--all-features` cannot run
  wholesale due to a **pre-existing** broken test target,
  `tests/server_events_e1_rls.rs` — `unresolved import server_common` —
  confirmed via `git stash` to already fail identically on the branch tip
  before this work; unrelated to this change and out of this task's scope
  to fix).

No RLS/authz/executor semantics changed; no WAL/MVCC/storage-format change.
