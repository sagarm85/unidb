//! `unidb-embed`: a small semantic-search CLI for UniDB (roadmap Track D).
//!
//! It turns text into a vector via a pluggable HTTP embedding endpoint
//! (OpenAI-compatible; key via env var), then stores or searches those vectors
//! through the running UniDB REST server using the `unidb-attach` client.
//!
//! Embedding *generation* lives entirely here on the client side — the engine
//! never gains a model or network dependency. See `README.md` for a worked
//! example.

#![forbid(unsafe_code)]

mod embed;
mod sql;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value as Json;
use unidb_attach::{AttachClient, ExecResult};

use embed::EmbeddingClient;

/// Client-side semantic search over UniDB.
#[derive(Parser, Debug)]
#[command(name = "unidb-embed", version, about)]
struct Cli {
    /// UniDB REST server base URL.
    #[arg(
        long,
        env = "UNIDB_SERVER",
        default_value = "http://localhost:7777",
        global = true
    )]
    server: String,

    /// JWT bearer token for the UniDB server.
    #[arg(long, env = "UNIDB_TOKEN", default_value = "", global = true)]
    token: String,

    /// Embedding endpoint URL (OpenAI-compatible `/embeddings` shape).
    #[arg(
        long,
        env = "UNIDB_EMBED_URL",
        default_value = "https://api.openai.com/v1/embeddings",
        global = true
    )]
    embed_url: String,

    /// Embedding model identifier.
    #[arg(
        long,
        env = "UNIDB_EMBED_MODEL",
        default_value = "text-embedding-3-small",
        global = true
    )]
    embed_model: String,

    /// API key for the embedding endpoint (key via env var — never a flag in
    /// practice). Leave empty for a keyless local server.
    #[arg(long, env = "UNIDB_EMBED_API_KEY", default_value = "", global = true)]
    embed_api_key: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Embed a piece of text and INSERT it as a new row.
    EmbedInsert {
        /// Target table (must already have the id/text/vector columns).
        #[arg(long)]
        table: String,
        /// Integer primary-key value for the new row.
        #[arg(long)]
        id: i64,
        /// The text to embed and store.
        #[arg(long)]
        text: String,
        /// Name of the integer id column.
        #[arg(long, default_value = "id")]
        id_col: String,
        /// Name of the text column that holds the original text.
        #[arg(long, default_value = "content")]
        text_col: String,
        /// Name of the `VECTOR(n)` column that holds the embedding.
        #[arg(long, default_value = "embedding")]
        vec_col: String,
    },
    /// Embed a query string and return the nearest stored rows.
    Search {
        /// Table to search.
        #[arg(long)]
        table: String,
        /// The query text to embed and search for.
        #[arg(long)]
        text: String,
        /// Number of neighbors to return.
        #[arg(short, long, default_value_t = 5)]
        k: usize,
        /// Name of the integer id column.
        #[arg(long, default_value = "id")]
        id_col: String,
        /// Name of the text column to return alongside the id.
        #[arg(long, default_value = "content")]
        text_col: String,
        /// Name of the `VECTOR(n)` column to search.
        #[arg(long, default_value = "embedding")]
        vec_col: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let embedder = EmbeddingClient {
        url: cli.embed_url.clone(),
        model: cli.embed_model.clone(),
        api_key: cli.embed_api_key.clone(),
    };
    let attach = AttachClient::new(&cli.server, &cli.token)
        .map_err(|e| anyhow!("connecting to UniDB server {}: {e}", cli.server))?;

    match cli.command {
        Command::EmbedInsert {
            table,
            id,
            text,
            id_col,
            text_col,
            vec_col,
        } => {
            let vector = embedder.embed(&text)?;
            let stmt = sql::insert_sql(&table, &id_col, &text_col, &vec_col, id, &text, &vector);
            attach
                .execute_sql(&stmt)
                .with_context(|| format!("inserting row id={id} into {table}"))?;
            println!(
                "inserted id={id} into {table} ({}-dim embedding)",
                vector.len()
            );
        }
        Command::Search {
            table,
            text,
            k,
            id_col,
            text_col,
            vec_col,
        } => {
            let vector = embedder.embed(&text)?;
            let stmt = sql::search_sql(&table, &id_col, &text_col, &vec_col, &vector, k);
            let results = attach
                .execute_sql(&stmt)
                .with_context(|| format!("searching {table}"))?;
            print_search_results(&results);
        }
    }
    Ok(())
}

/// Print the rows from a `SELECT` result set, one per line.
fn print_search_results(results: &[ExecResult]) {
    for result in results {
        if let ExecResult::Rows { columns, rows } = result {
            if !columns.is_empty() {
                println!("{}", columns.join(" | "));
            }
            if rows.is_empty() {
                println!("(no matches)");
            }
            for row in rows {
                println!("{}", format_row(row));
            }
        }
    }
}

/// Render one row's JSON cells as `col | col | ...`.
fn format_row(row: &[Json]) -> String {
    row.iter()
        .map(|cell| match cell {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_row_unquotes_strings() {
        let row = vec![json!(7), json!("hello world")];
        assert_eq!(format_row(&row), "7 | hello world");
    }

    #[test]
    fn cli_parses_search_defaults() {
        let cli = Cli::try_parse_from(["unidb-embed", "search", "--table", "docs", "--text", "hi"])
            .unwrap();
        match cli.command {
            Command::Search {
                k, id_col, vec_col, ..
            } => {
                assert_eq!(k, 5);
                assert_eq!(id_col, "id");
                assert_eq!(vec_col, "embedding");
            }
            _ => panic!("expected search"),
        }
    }
}
