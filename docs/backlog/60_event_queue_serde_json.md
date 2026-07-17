# Item 60 — Event queue serde_json replacement

**Type:** Performance
**Status:** SHIPPED (→ PROGRESS.md "Item 60 — Event queue serde_json replacement")

## Problem

`send_event_capture` in `src/sql/executor.rs` built the CDC event envelope
using `serde_json::json!`.  For every INSERT/UPDATE/DELETE on an events-enabled
table this:

1. Called `row_to_json` twice (before + after images), each allocating a
   `serde_json::Value::Object` (a `HashMap<String, Value>` heap allocation).
2. Built a wrapping `serde_json::Value::Object` for the full envelope via the
   `json!` macro.
3. Serialised that `Value` back to a `String` via `.to_string()`.
4. Called `SystemTime::now()` (syscall) per event.

For a W4 commit on a 100-row pre-grown table the HNSW + graph costs dominate;
at 100k rows the event JSON allocation is the marginal cost that pushes W4/W0
above the 1.5× target.  The VECTOR(128) column contributes especially: each
f32 is boxed as a `serde_json::Number` inside a `Vec<Value>` before being
serialised back out as text.

## Baseline (from `benchmark_20260716_232744.md`)

| rows | W4/W0 | Δ event (W4−W3) |
|-----:|------:|----------------:|
| 1000 | 4.50× | +0.69 ms |
| 10000 | 1.98× | +0.06 ms |
| 100000 | 1.70× | +0.03 ms |

Target: W4/W0 at 100k ≤ 1.50× (1.33× stretch).

## Root cause

`send_event_capture` at `executor.rs:1068-1079` called `serde_json::json!` to
build the envelope.  Two `serde_json::Map` allocations (before + after) plus
one outer `Map` allocation, then a full serialise pass, for every captured row.
The `VECTOR(128)` path boxes 128 `f32` values into `JsonValue::Number` objects
(per `serde_json::Number::from_f64`), then serialises them back out.

## Fix (item 60)

Replace the `serde_json::json!` + `row_to_json` path with a **manual string
builder**:

- `queue::payload::write_row_json(out: &mut String, row, cols)` — writes a
  `{"col":val,...}` JSON object directly into a pre-allocated `String`.  No
  `Map` allocated; no intermediate `Value`.  Handles all `Literal` variants,
  including VECTOR (writes `[f1,f2,...]` directly) and Json (embedded verbatim).

- `queue::payload::build_event_envelope_str(op, table, before, after, ts_ms, seq, xid) -> String` —
  builds the full canonical envelope `{"payload":…,"before":…,"after":…,
  "ts_ms":…,"source":{…}}` directly as a `String`, calling `write_row_json`
  once per image.  The output is semantically identical to the old `json!` path
  (same fields, same JSON values after parse), so `resolve_event_candidates`
  (`lib.rs`) continues to work unchanged.

- `queue::event_row` signature changed from `payload: &serde_json::Value` to
  `payload_json: String` — eliminates the final `.to_string()` serialise step.

The legacy `row_to_json` function is kept for callers outside the hot CDC path
(e.g. `server/dto.rs`) that still need a `serde_json::Value`.

## Tests added

- `write_row_json_*` — unit tests for each `Literal` variant.
- `build_event_envelope_str_is_parseable_and_has_correct_fields` — parses the
  output and checks all envelope fields.
- `build_event_envelope_str_delete_has_before_only` — compat payload = before
  for DELETE.
- `envelope_str_matches_serde_json_macro_output` — parses both old and new
  paths and compares `serde_json::Value` equality.

## Files changed

- `src/queue/payload.rs` — new `write_row_json`, `build_event_envelope_str`,
  `push_json_str`; kept `row_to_json` for non-hot callers.
- `src/queue/mod.rs` — `event_row` signature: `&serde_json::Value` → `String`.
- `src/sql/executor.rs` — `send_event_capture`: removed `row_to_json` calls and
  `serde_json::json!` macro; replaced with `build_event_envelope_str`.

## Measurement

Run after PR merges; results go into `PROGRESS.md`.

```bash
MM_SIZES=1000,10000,100000 MM_SAMPLE=20 UNIDB_BENCH=mmreport \
  cargo bench --bench decompose 2>&1 | grep -E "W4/W0|^\| [0-9]"
```

Gate: W4/W0 at 100k must improve from ~1.70× toward ≤1.50×.
