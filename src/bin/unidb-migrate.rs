//! `unidb-migrate`: CLI for the schema-migrations tooling (item 126,
//! Workstream I4 — `docs/backlog/126_sql_schema_migrations.md`). A thin
//! wrapper around [`unidb::Engine::apply_migrations`]; see that module's doc
//! comment (`src/migrations.rs`) for the algorithm, file-naming convention,
//! and the non-transactional-DDL caveat.
//!
//! No async runtime, no HTTP — this only opens the sync embedded `Engine`
//! and applies migrations as the trusted local operator (same posture as
//! any other embedded-API DDL call: no RLS/authz gate, matching
//! `Engine::execute_sql`'s implicit-superuser semantics).
//!
//! ## Usage
//!
//! ```text
//! unidb-migrate [DIR]
//! ```
//!
//! `DIR` defaults to `./migrations`. Config via environment:
//! - `UNIDB_DATA_DIR` (default `/tmp/unidb`): directory `Engine::open`s —
//!   same variable and default `unidb-server` uses, so a deployment that
//!   already sets it for the server needs no separate config for this tool.
//! - `UNIDB_PAGE_SIZE` (default `0`, meaning `Engine::open`'s own default).
//!
//! Exit code `0` on success (including "nothing to apply"), `1` on any
//! error (bad directory, unreadable engine, a failing/drifted migration).
//! Prints a one-line summary on success; prints the error (which, per
//! `src/migrations.rs`'s doc comment, already includes the non-transactional-
//! DDL cleanup guidance when relevant) to stderr on failure.

use std::path::PathBuf;

use unidb::Engine;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("migrations"));
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    let page_size: u32 = std::env::var("UNIDB_PAGE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let engine = match Engine::open(std::path::Path::new(&data_dir), page_size) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("unidb-migrate: failed to open engine at '{data_dir}': {e}");
            std::process::exit(1);
        }
    };

    println!(
        "unidb-migrate: applying migrations from '{}'...",
        dir.display()
    );
    match engine.apply_migrations(&dir) {
        Ok(report) => {
            println!("unidb-migrate: {}", report.summary());
        }
        Err(e) => {
            eprintln!("unidb-migrate: {e}");
            std::process::exit(1);
        }
    }
}
