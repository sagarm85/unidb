# unidb-embed

A small **client-side** semantic-search CLI for UniDB (roadmap Track D).

It turns text into a vector by calling a **pluggable HTTP embedding endpoint**
(OpenAI-compatible by default), then stores or searches those vectors through a
running UniDB REST server using the [`unidb-attach`](../unidb-attach) client.

> **Embedding generation stays entirely on the client.** The `unidb` engine
> never gains a model or network dependency — this CLI is the only thing that
> talks to an embedding model. The engine just stores `VECTOR(n)` values and
> answers `NEAR(...)` queries, exactly as it already did.

## Install / build

```bash
cargo build -p unidb-embed --release
# binary: target/release/unidb-embed
```

## Configuration

Everything can come from flags or environment variables:

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--server` | `UNIDB_SERVER` | `http://localhost:7777` | UniDB REST server base URL |
| `--token` | `UNIDB_TOKEN` | *(empty)* | JWT bearer token for the server |
| `--embed-url` | `UNIDB_EMBED_URL` | `https://api.openai.com/v1/embeddings` | Embedding endpoint |
| `--embed-model` | `UNIDB_EMBED_MODEL` | `text-embedding-3-small` | Model id |
| `--embed-api-key` | `UNIDB_EMBED_API_KEY` | *(empty)* | Embedding API key (**via env var**) |

The endpoint is called with an OpenAI-style body
(`{"model": ..., "input": "<text>"}`) and the embedding is read from either
`data[0].embedding` or a flat `embedding` field, so self-hosted keyless servers
work too (leave `--embed-api-key` empty).

## Commands

```
unidb-embed embed-insert --table <T> --id <N> --text "<text>"
unidb-embed search       --table <T> --text "<query>" [-k <N>]
```

Column names default to `id` / `content` / `embedding` and are overridable with
`--id-col` / `--text-col` / `--vec-col`.

## Worked example

Assume the UniDB server is running and `UNIDB_TOKEN` / `UNIDB_EMBED_API_KEY`
are exported.

**1. Create the table + HNSW index once** (via the server — `text-embedding-3-small`
is 1536-dimensional):

```bash
curl -s -X POST "$UNIDB_SERVER/sql" -H "Authorization: Bearer $UNIDB_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"sql":"CREATE TABLE docs (id INT, content TEXT, embedding VECTOR(1536))"}'

curl -s -X POST "$UNIDB_SERVER/sql" -H "Authorization: Bearer $UNIDB_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"sql":"CREATE INDEX docs_emb ON docs USING HNSW (embedding)"}'
```

**2. Embed and insert a few rows:**

```bash
unidb-embed embed-insert --table docs --id 1 --text "The cat sat on the mat."
unidb-embed embed-insert --table docs --id 2 --text "A dog ran across the yard."
unidb-embed embed-insert --table docs --id 3 --text "Quarterly revenue grew 12%."
# → inserted id=1 into docs (1536-dim embedding)
```

**3. Search by meaning, not keywords:**

```bash
unidb-embed search --table docs --text "a pet indoors" -k 2
# → 1 | The cat sat on the mat.
#   2 | A dog ran across the yard.
```

The query text is embedded client-side, then the engine runs
`SELECT id, content FROM docs WHERE NEAR(embedding, [...], 2)` and returns the
nearest rows.

## Distance metric

The vector index supports two per-index metrics (`unidb::vector::Metric`):
**Euclidean** (default, `pgvector` `<->`) and **Cosine** (`1 - cos`, `pgvector`
`<=>`) — the natural metric for direction-only text-embedding similarity.
Cosine is available in the engine's `VectorIndex` API today; wiring a
`USING HNSW` metric choice through `CREATE INDEX` is a follow-up in the SQL
lane. For OpenAI-style embeddings (already ~unit-normalized) Euclidean and
cosine rankings largely agree, so this CLI works well against the default
index.
