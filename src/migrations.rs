//! Workstream I4 — SQL schema-migrations tooling (item 120's roadmap, filed
//! as `docs/backlog/126_sql_schema_migrations.md`).
//!
//! Supabase-style **forward-only** SQL migrations: plain `.sql` files in a
//! directory, applied in ascending version order, tracked in a
//! [`MIGRATIONS_TABLE`] table this module creates and maintains itself.
//!
//! ## File naming
//!
//! `<version>_<name>.sql` — `version` sorts **lexicographically** (so either
//! zero-padded sequence numbers, `0001_init.sql`, `0002_add_users.sql`, or a
//! sortable timestamp prefix, `20260731120000_add_users.sql`, work; an
//! unpadded `9_x.sql`/`10_y.sql` pair would NOT sort the way you want, so
//! don't do that). A file may contain multiple `;`-separated statements —
//! executed exactly like any other multi-statement string passed to
//! [`crate::Engine::execute_sql`].
//!
//! ## Apply algorithm
//!
//! For each migration file, sorted by version ascending:
//! 1. If its version is already recorded in [`MIGRATIONS_TABLE`], compare
//!    the recorded checksum (SHA-256 of the file's exact bytes at the time
//!    it was applied) against the current file's checksum. Mismatch =
//!    **drift** — someone edited a shipped migration — and this STOPS with
//!    [`DbError::Migration`] rather than silently re-applying a changed
//!    file. Match = skip (idempotent re-run).
//! 2. Otherwise, execute the file's SQL and record a
//!    [`MIGRATIONS_TABLE`] row (version, name, checksum, `applied_at`) in
//!    the **same** mini-transaction, then commit.
//! 3. On any error (bad SQL, constraint violation, checksum drift), STOP
//!    immediately. Earlier migrations in this run stay applied; the failing
//!    migration is **not** recorded, so a subsequent run retries it.
//!
//! ## The non-transactional-DDL caveat — read this before writing a migration
//!
//! unidb's catalog DDL (`CREATE TABLE`, `ALTER TABLE`, …) is **not**
//! MVCC-versioned/transactional in general (see `MEMORY.md`'s "Catalog DDL
//! is not transactional" known issue: a DDL statement is *not* undone by
//! [`crate::Engine::abort`] once it has succeeded). This module's actual
//! exposure to that gap is narrower than "any statement failure loses
//! atomicity," and it is worth being precise about exactly where the line
//! is (verified empirically, not assumed — CLAUDE.md §0.6 item 4):
//!
//! - **Within one migration file**, unidb's request-scoped DDL rollback
//!   (item P2.c: [`crate::Engine::execute_sql`]/`execute_sql_params` snapshot
//!   `catalog_root` before their first statement and restore it if **any**
//!   statement in that same call fails) means a multi-statement file *is*
//!   atomic with respect to itself: `CREATE TABLE foo (...); CREATE TABLE
//!   foo (...);` (a duplicate, so the second statement errors) leaves `foo`
//!   **not** created — the whole file's effect (DDL and DML alike) is
//!   undone together, before `apply_migrations` ever returns.
//! - **The one real gap**: this module records the migration in
//!   [`MIGRATIONS_TABLE`] via a **second**, separate SQL call, after the
//!   file's own SQL has already succeeded. If — and only if — that second,
//!   tool-internal `INSERT` itself fails (a narrow case: it is a fixed,
//!   well-formed statement against a table this module fully owns), the
//!   file's already-succeeded DDL is **not** undone (crossing an
//!   `execute_sql`/`execute_sql_params` call boundary is exactly where the
//!   general "DDL survives `abort`" limitation applies), while the
//!   migration is also **not** recorded as applied — a subsequent run will
//!   retry the file's SQL and may hit "already exists"-style errors on any
//!   DDL it contains.
//!
//! This module does **not** attempt to build a rollback/savepoint engine to
//! paper over that last narrow gap (explicitly out of scope — see the
//! item-126 design; it is not worth the complexity for one internal
//! `INSERT`). Instead:
//! - **Keep each migration file to one logical change** (the standard
//!   Supabase/Rails/Flyway convention anyway) — this keeps the intra-file
//!   atomicity guarantee doing all the work, and minimizes what there is to
//!   clean up in the rare case the recording step itself fails.
//! - **If a migration reports "applied but failed to record," manual
//!   inspection may be needed** before retrying (e.g. `DROP` an object the
//!   file's DDL already created). The error message returned by
//!   [`crate::Engine::apply_migrations`] says so inline, distinctly from a
//!   plain "migration failed" (which needs no cleanup — see above).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{DbError, Result};
use crate::sql::executor::ExecResult;
use crate::sql::logical::Literal;
use crate::Engine;

/// The tracking table this module creates (`CREATE TABLE IF NOT EXISTS`
/// semantics, emulated — see [`Engine::ensure_migrations_table`], since the
/// SQL grammar itself has no `IF NOT EXISTS`) and maintains. Columns:
/// `version TEXT PRIMARY KEY, name TEXT, checksum TEXT, applied_at
/// TIMESTAMP`.
pub const MIGRATIONS_TABLE: &str = "schema_migrations";

/// One migration file discovered on disk: its parsed `<version>_<name>.sql`
/// identity plus the path to read it from.
#[derive(Debug, Clone)]
pub struct MigrationFile {
    pub version: String,
    pub name: String,
    pub path: PathBuf,
}

/// Outcome of one [`Engine::apply_migrations`] call.
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    /// Versions newly applied this run, in the order they were applied
    /// (ascending version order).
    pub applied: Vec<String>,
    /// Count of already-applied versions skipped (checksum matched, so this
    /// run was a no-op for them).
    pub skipped: usize,
}

impl MigrationReport {
    /// One-line human summary, e.g. `"applied 2, skipped 3"` — what the CLI
    /// prints.
    pub fn summary(&self) -> String {
        format!(
            "applied {}, skipped {}{}",
            self.applied.len(),
            self.skipped,
            if self.applied.is_empty() {
                String::new()
            } else {
                format!(" ({})", self.applied.join(", "))
            }
        )
    }
}

/// SHA-256 of `bytes`, lower-hex encoded. Used as the migration-file
/// checksum: hashed over the file's **exact bytes** (not the parsed SQL), so
/// even a whitespace-only edit to an already-applied migration is caught as
/// drift.
fn checksum_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        // `write!` to a `String` never fails.
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// List `dir`'s `*.sql` files as [`MigrationFile`]s, sorted ascending by
/// `version` (lexicographic — see the module doc's naming section). Errors
/// on: an unreadable directory, a `.sql` file whose stem doesn't contain a
/// `_` separator (doesn't match `<version>_<name>.sql`), or two files
/// sharing the same `version`.
fn list_migration_files(dir: &Path) -> Result<Vec<MigrationFile>> {
    let entries = fs::read_dir(dir).map_err(|e| {
        DbError::Migration(format!(
            "cannot read migrations directory '{}': {e}",
            dir.display()
        ))
    })?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            DbError::Migration(format!(
                "cannot read an entry of migrations directory '{}': {e}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            DbError::Migration(format!(
                "migration filename '{}' is not valid UTF-8",
                path.display()
            ))
        })?;
        let (version, name) = stem.split_once('_').ok_or_else(|| {
            DbError::Migration(format!(
                "migration filename '{}' does not match '<version>_<name>.sql' \
                 (missing '_' separator)",
                path.display()
            ))
        })?;
        if version.is_empty() {
            return Err(DbError::Migration(format!(
                "migration filename '{}' has an empty version",
                path.display()
            )));
        }
        files.push(MigrationFile {
            version: version.to_string(),
            name: name.to_string(),
            path,
        });
    }

    files.sort_by(|a, b| a.version.cmp(&b.version));
    for pair in files.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(DbError::Migration(format!(
                "duplicate migration version '{}': '{}' and '{}'",
                pair[0].version,
                pair[0].path.display(),
                pair[1].path.display()
            )));
        }
    }
    Ok(files)
}

impl Engine {
    /// `CREATE TABLE IF NOT EXISTS schema_migrations (...)`, emulated:
    /// unidb's SQL grammar has no `IF NOT EXISTS` (item 126 verified this
    /// before relying on it), so this attempts the plain `CREATE TABLE` and
    /// treats [`DbError::TableAlreadyExists`] as success — idempotent across
    /// repeated `apply_migrations` calls.
    fn ensure_migrations_table(&self) -> Result<()> {
        let xid = self.begin()?;
        let ddl = format!(
            "CREATE TABLE {MIGRATIONS_TABLE} \
             (version TEXT PRIMARY KEY, name TEXT, checksum TEXT, applied_at TIMESTAMP)"
        );
        match self.execute_sql(xid, &ddl) {
            Ok(_) => self.commit(xid),
            Err(DbError::TableAlreadyExists(_)) => {
                // Nothing was actually mutated by the failed CREATE TABLE
                // (the catalog rejected it before any page write); abort
                // just to release the txn slot cleanly.
                let _ = self.abort(xid);
                Ok(())
            }
            Err(e) => {
                let _ = self.abort(xid);
                Err(e)
            }
        }
    }

    /// `version -> checksum` for every row currently in `schema_migrations`.
    /// Requires [`Engine::ensure_migrations_table`] to have already run (a
    /// missing table is a caller bug, not a legitimate "no migrations
    /// applied yet" state, since `apply_migrations` always creates it
    /// first).
    fn load_applied_migrations(&self) -> Result<BTreeMap<String, String>> {
        let xid = self.begin()?;
        let result = self.execute_sql(
            xid,
            &format!("SELECT version, checksum FROM {MIGRATIONS_TABLE}"),
        );
        let rows = match result {
            Ok(mut results) => {
                self.commit(xid)?;
                match results.pop() {
                    Some(ExecResult::Rows { rows, .. }) => rows,
                    _ => Vec::new(),
                }
            }
            Err(e) => {
                let _ = self.abort(xid);
                return Err(e);
            }
        };

        let mut applied = BTreeMap::new();
        for row in rows {
            if let [Literal::Text(version), checksum] = row.as_slice() {
                let checksum = match checksum {
                    Literal::Text(s) => s.clone(),
                    Literal::Null => String::new(),
                    other => format!("{other:?}"),
                };
                applied.insert(version.clone(), checksum);
            }
        }
        Ok(applied)
    }

    /// Execute one migration file's SQL, then record it in
    /// `schema_migrations` — see this module's doc comment for exactly
    /// which of the two calls below is (and isn't) rolled back on failure.
    fn apply_one_migration(&self, file: &MigrationFile) -> Result<()> {
        let bytes = fs::read(&file.path).map_err(|e| {
            DbError::Migration(format!(
                "cannot read migration '{}': {e}",
                file.path.display()
            ))
        })?;
        let checksum = checksum_hex(&bytes);
        let sql = String::from_utf8(bytes).map_err(|e| {
            DbError::Migration(format!(
                "migration '{}' is not valid UTF-8: {e}",
                file.path.display()
            ))
        })?;

        let applied_at_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);

        let xid = self.begin()?;

        // Step 1: the migration file's own SQL, as one `execute_sql` call.
        // unidb's request-scoped DDL rollback (item P2.c) rolls back every
        // statement executed earlier in THIS SAME call if a later one in it
        // fails — so a multi-statement file is atomic with respect to
        // itself; nothing from it is left half-applied.
        if let Err(e) = self.execute_sql(xid, &sql) {
            let _ = self.abort(xid);
            return Err(DbError::Migration(format!(
                "migration '{}_{}' failed: {e}. NOT recorded in {MIGRATIONS_TABLE}; earlier \
                 migrations in this run remain applied. This file's own statements were rolled \
                 back together with the failure (unidb rolls back every statement of one SQL \
                 call when any statement in it errors) — nothing from this file was left \
                 applied. Fix the file and re-run.",
                file.version, file.name
            )));
        }

        // Step 2: record it, in a SEPARATE call (the version/name/checksum
        // values are caller data bound via `$n`, never re-parsed as SQL).
        // This is the one place this module isn't fully atomic: if THIS
        // call fails, step 1's already-succeeded DDL/DML is not undone by
        // the `abort` below — crossing an `execute_sql`/`execute_sql_params`
        // call boundary is exactly where unidb's "DDL survives transaction
        // abort" limitation applies (see this module's doc comment).
        let record = self.execute_sql_params(
            xid,
            &format!(
                "INSERT INTO {MIGRATIONS_TABLE} (version, name, checksum, applied_at) \
                 VALUES ($1, $2, $3, $4)"
            ),
            &[
                Literal::Text(file.version.clone()),
                Literal::Text(file.name.clone()),
                Literal::Text(checksum),
                Literal::Timestamp(applied_at_us),
            ],
        );
        match record {
            Ok(_) => self.commit(xid),
            Err(e) => {
                let _ = self.abort(xid);
                Err(DbError::Migration(format!(
                    "migration '{}_{}' applied but FAILED TO RECORD in {MIGRATIONS_TABLE}: {e}. \
                     unidb does not roll back DDL across separate SQL calls (see this module's \
                     doc comment) — this migration's own statements may have already taken \
                     effect even though it is not marked applied, so a re-run will retry the \
                     file and may hit 'already exists'-style errors on any DDL it contains. \
                     Inspect and clean up manually before retrying.",
                    file.version, file.name
                )))
            }
        }
    }

    /// Apply every not-yet-applied `*.sql` file in `dir`, in ascending
    /// version order, tracking progress in the `schema_migrations` table
    /// (created on first use). See this module's doc comment for the full
    /// algorithm, naming convention, and the non-transactional-DDL caveat.
    ///
    /// Stops at the first failing migration (bad SQL, or checksum drift on
    /// an already-applied file whose bytes were edited since) and returns
    /// [`DbError::Migration`] describing it; migrations before it in
    /// version order stay applied and recorded.
    pub fn apply_migrations(&self, dir: &Path) -> Result<MigrationReport> {
        let files = list_migration_files(dir)?;
        self.ensure_migrations_table()?;
        let already_applied = self.load_applied_migrations()?;

        let mut report = MigrationReport::default();
        for file in &files {
            if let Some(recorded_checksum) = already_applied.get(&file.version) {
                let bytes = fs::read(&file.path).map_err(|e| {
                    DbError::Migration(format!(
                        "cannot read migration '{}': {e}",
                        file.path.display()
                    ))
                })?;
                let current_checksum = checksum_hex(&bytes);
                if &current_checksum != recorded_checksum {
                    return Err(DbError::Migration(format!(
                        "checksum drift on already-applied migration '{}_{}': recorded \
                         checksum {recorded_checksum}, file's current checksum \
                         {current_checksum}. An applied migration's file must never be edited \
                         after it ships — create a new migration with the fix instead. Refusing \
                         to re-apply.",
                        file.version, file.name
                    )));
                }
                report.skipped += 1;
                continue;
            }

            self.apply_one_migration(file)?;
            report.applied.push(file.version.clone());
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_hex_is_deterministic_and_64_hex_chars() {
        let a = checksum_hex(b"CREATE TABLE t (id INT);");
        let b = checksum_hex(b"CREATE TABLE t (id INT);");
        let c = checksum_hex(b"CREATE TABLE t (id INT PRIMARY KEY);");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn list_migration_files_sorts_and_parses_version_name() {
        let dir = std::env::temp_dir().join(format!("unidb_migtest_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0002_add_users.sql"), "SELECT 1;").unwrap();
        fs::write(dir.join("0001_init.sql"), "SELECT 1;").unwrap();
        fs::write(dir.join("readme.txt"), "not sql").unwrap();

        let files = list_migration_files(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].version, "0001");
        assert_eq!(files[0].name, "init");
        assert_eq!(files[1].version, "0002");
        assert_eq!(files[1].name, "add_users");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_migration_files_rejects_bad_name() {
        let dir = std::env::temp_dir().join(format!("unidb_migtest_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("noversion.sql"), "SELECT 1;").unwrap();

        let err = list_migration_files(&dir).unwrap_err();
        assert!(matches!(err, DbError::Migration(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_migration_files_rejects_duplicate_version() {
        let dir = std::env::temp_dir().join(format!("unidb_migtest_dup_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        fs::write(dir.join("0001_b.sql"), "SELECT 1;").unwrap();

        let err = list_migration_files(&dir).unwrap_err();
        assert!(matches!(err, DbError::Migration(_)));

        let _ = fs::remove_dir_all(&dir);
    }
}
