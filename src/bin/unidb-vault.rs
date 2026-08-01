//! `unidb-vault`: CLI for the encrypt-at-rest secrets vault (item 129,
//! Workstream I3 — `docs/backlog/129_secrets_vault.md`). A thin wrapper
//! around [`unidb::Engine::set_secret`]/`has_secret`/`secret_names`; see
//! `src/vault.rs`'s module doc for the crypto (AES-256-GCM) and the
//! `UNIDB_MASTER_KEY` format.
//!
//! No async runtime, no HTTP — this only opens the sync embedded `Engine`
//! and calls its vault methods as the trusted local operator (same posture
//! as `unidb-migrate`: no RLS/authz gate, matching `Engine::execute_sql`'s
//! implicit-superuser semantics).
//!
//! ## Usage
//!
//! ```text
//! unidb-vault set <name>     Encrypt and store a secret under <name>.
//!                             The plaintext is read from stdin if piped/
//!                             redirected, otherwise from the
//!                             UNIDB_VAULT_SECRET_VALUE env var — NEVER
//!                             from a CLI argument, so it never ends up in
//!                             shell history or `ps`. Prints only
//!                             "<name>: stored", never the value.
//! unidb-vault has <name>      Print whether <name> is currently stored
//!                             ("stored" / "not stored"). Never decrypts,
//!                             never prints the value.
//! unidb-vault list            List every stored secret name, one per
//!                             line. Never prints a value.
//! ```
//!
//! Config via environment (same as `unidb-migrate`):
//! - `UNIDB_DATA_DIR` (default `/tmp/unidb`): directory `Engine::open`s.
//! - `UNIDB_PAGE_SIZE` (default `0`, meaning `Engine::open`'s own default).
//! - `UNIDB_MASTER_KEY` (required for `set` — see `src/vault.rs`): with no
//!   key configured, `Engine::open` succeeds (vault disabled, per D1's
//!   back-compat contract) but `set` fails clearly, since there is no path
//!   that stores a secret unencrypted. Malformed keys fail `Engine::open`
//!   itself with a clear error, same posture as `unidb-server`.
//!
//! Exit code `0` on success, `1` on any error (bad data dir, malformed
//! master key, no secret value supplied to `set`, vault disabled for
//! `set`), `2` on a usage error (missing/unknown subcommand or argument).

use std::io::{IsTerminal, Read};

use unidb::Engine;

fn usage() -> &'static str {
    "usage: unidb-vault <set|has|list> [name]\n\
     \n\
     \x20 set <name>   encrypt+store a secret (plaintext via stdin or\n\
     \x20               UNIDB_VAULT_SECRET_VALUE — never a CLI arg)\n\
     \x20 has <name>   print whether <name> is currently stored\n\
     \x20 list         list stored secret names"
}

/// Read the plaintext secret value to store: stdin if it's piped/redirected
/// (not an interactive terminal) and non-empty once trimmed of a trailing
/// newline, otherwise `UNIDB_VAULT_SECRET_VALUE`. Deliberately **no** CLI
/// argument form — a secret passed as `argv` ends up in shell history and
/// `ps`/`/proc/<pid>/cmdline` on any multi-user host.
fn read_secret_value() -> Result<String, String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut buf = String::new();
        stdin
            .lock()
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed reading stdin: {e}"))?;
        let trimmed = buf.trim_end_matches(['\n', '\r']);
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    std::env::var("UNIDB_VAULT_SECRET_VALUE")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "no secret value provided — pipe it via stdin (e.g. `echo -n '<value>' | \
             unidb-vault set <name>`) or set UNIDB_VAULT_SECRET_VALUE"
                .to_string()
        })
}

fn open_engine(data_dir: &str, page_size: u32) -> Engine {
    match Engine::open(std::path::Path::new(data_dir), page_size) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("unidb-vault: failed to open engine at '{data_dir}': {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    let page_size: u32 = std::env::var("UNIDB_PAGE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    match args.get(1).map(String::as_str) {
        Some("set") => {
            let Some(name) = args.get(2) else {
                eprintln!("{}", usage());
                std::process::exit(2);
            };
            let value = match read_secret_value() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("unidb-vault: {e}");
                    std::process::exit(1);
                }
            };
            let engine = open_engine(&data_dir, page_size);
            match engine.set_secret(name, &value) {
                Ok(()) => println!("{name}: stored"),
                Err(e) => {
                    eprintln!("unidb-vault: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("has") => {
            let Some(name) = args.get(2) else {
                eprintln!("{}", usage());
                std::process::exit(2);
            };
            let engine = open_engine(&data_dir, page_size);
            let state = if engine.has_secret(name) {
                "stored"
            } else {
                "not stored"
            };
            println!("{name}: {state}");
        }
        Some("list") => {
            let engine = open_engine(&data_dir, page_size);
            let names = engine.secret_names();
            if names.is_empty() {
                println!("(no secrets stored)");
            } else {
                for name in names {
                    println!("{name}");
                }
            }
        }
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}
