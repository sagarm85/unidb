# Schema migrations

> Item 126 (Workstream I4 — `docs/backlog/126_sql_schema_migrations.md`).
> Supabase-style, forward-only SQL migrations applied in version order,
> tracked in a table. Implementation: `src/migrations.rs`; CLI:
> `src/bin/unidb-migrate.rs`.

## What it is

Plain `.sql` files in a directory, applied in ascending version order,
tracked in a `schema_migrations` table the tool creates and maintains
itself. No down-migrations, no generated rollback SQL — forward-only, the
same model Supabase's own migration tooling uses.

## File naming

```
<version>_<name>.sql
```

`version` sorts **lexicographically**, not numerically — so use either
zero-padded sequence numbers (`0001_init.sql`, `0002_add_users.sql`) or a
sortable timestamp prefix (`20260731120000_add_users.sql`). An unpadded
`9_x.sql` / `10_y.sql` pair would **not** sort the way you want (`"10" <
"9"` lexicographically) — don't do that.

A file may contain multiple `;`-separated statements, executed exactly like
any other multi-statement string passed to `Engine::execute_sql`.

## Tracking table

Created with `CREATE TABLE` semantics equivalent to `IF NOT EXISTS` (unidb's
SQL grammar has no literal `IF NOT EXISTS`, so the tool emulates it by
treating "table already exists" as success) on first use:

```sql
CREATE TABLE schema_migrations (
    version    TEXT PRIMARY KEY,
    name       TEXT,
    checksum   TEXT,
    applied_at TIMESTAMP
);
```

`checksum` is the SHA-256 (lower-hex) of the migration file's **exact
bytes** at the time it was applied.

## Apply algorithm

For each `*.sql` file in the directory, sorted by version ascending:

1. **Already applied?** If `version` is already a row in
   `schema_migrations`, compare its recorded `checksum` against the current
   file's checksum.
   - **Match** → skip (this run is a no-op for that file — idempotent
     re-runs).
   - **Mismatch** → **drift**: someone edited a shipped migration. Stop
     immediately with an error naming the version and both checksums.
     Refuses to silently re-apply a changed file.
2. **Not yet applied** → execute the file's SQL, then insert a
   `schema_migrations` row (version, name, checksum, `applied_at`).
3. **Any error** (bad SQL, constraint violation, checksum drift) **stops the
   whole run immediately**. Migrations applied earlier in this run (or a
   previous run) stay applied; the failing migration is **not** recorded, so
   a subsequent run retries it.

## The non-transactional-DDL caveat

unidb's catalog DDL (`CREATE TABLE`, `ALTER TABLE`, …) is not
MVCC-versioned/transactional in general — a DDL statement is not undone by
aborting its enclosing transaction once it has succeeded (see `MEMORY.md`'s
"Catalog DDL is not transactional" known issue). This module's actual
exposure to that gap is narrower than it might sound, and it's worth being
precise (verified empirically, not assumed):

- **Within one migration file**, unidb's request-scoped DDL rollback (item
  P2.c) rolls back **every** statement executed earlier in that same
  `execute_sql` call if a later statement in it fails. A multi-statement
  migration file is therefore atomic with respect to itself — e.g.
  `CREATE TABLE foo (...); CREATE TABLE foo (...);` (duplicate, so the
  second statement errors) leaves `foo` **not** created; nothing from that
  file is left half-applied.
- **The one real gap**: the tool records the migration via a **second**,
  separate SQL call, after the file's own SQL has already succeeded. If —
  and only if — that second, internal `INSERT` itself fails (a narrow case:
  it's a fixed, well-formed statement against a table this module fully
  owns), the file's already-succeeded DDL is *not* undone, while the
  migration is also *not* recorded as applied. A subsequent run will then
  retry the file's SQL and may hit "already exists"-style errors on any DDL
  it contains.

The tool deliberately does **not** build a rollback/savepoint engine to
paper over that last narrow gap — not worth the complexity for one internal
`INSERT`. Instead:

- **Keep each migration file to one logical change** (the standard
  Supabase/Rails/Flyway convention anyway). This keeps the intra-file
  atomicity guarantee above doing all the work, and minimizes what there is
  to clean up in the rare case the recording step itself fails.
- **If a migration reports "applied but failed to record," manual cleanup
  may be needed** before retrying — the returned error says so explicitly,
  distinct from a plain "migration failed" (which needs no cleanup, since
  the file's own statements were rolled back together with the failure).

## CLI usage

```bash
# migrations/0001_init.sql, migrations/0002_add_users.sql, …
UNIDB_DATA_DIR=/var/lib/unidb cargo run --bin unidb-migrate -- migrations
```

Environment:

| Variable | Default | Purpose |
|----------|---------|---------|
| `UNIDB_DATA_DIR` | `/tmp/unidb` | Directory `Engine::open`s (same variable `unidb-server` uses) |
| `UNIDB_PAGE_SIZE` | `0` (engine default) | Page size for a fresh data directory |

Positional argument: the migrations directory (default `./migrations`).

Exit code `0` on success (including "nothing to apply"), `1` on any error.
Prints a one-line summary (`applied N, skipped M (versions...)`) on success.

Not gated behind the `server` Cargo feature — it only opens the sync
embedded `Engine`, so a default `cargo build`/`cargo run` already builds it.

## Rust API

```rust
use std::path::Path;
use unidb::Engine;

let engine = Engine::open(Path::new("/var/lib/unidb"), 0)?;
let report = engine.apply_migrations(Path::new("migrations"))?;
println!("{}", report.summary()); // "applied 2, skipped 0 (0001, 0002)"
```

`Engine::apply_migrations(dir: &Path) -> Result<MigrationReport>` runs as
the trusted local operator (embedded superuser, like any other embedded-API
DDL call) — no RLS/authz gate, matching `Engine::execute_sql`'s implicit-
superuser semantics. `MigrationReport { applied: Vec<String>, skipped: usize
}`.

## Testing

`tests/item126_migrations.rs` covers: creating objects + populating the
tracking table; idempotent re-runs; checksum-drift detection on an edited
migration; ascending-version-order application (including a
dependency-ordering case); a failing migration stopping the run,
not being recorded, and not leaking its own partial DDL, while earlier
migrations stay applied; and malformed-filename/duplicate-version rejection.
