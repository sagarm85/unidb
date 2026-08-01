# Realtime Broadcast + Presence

**Type:** Improvement
**Status:** IN PROGRESS

> Supabase-parity gap (item 120 follow-up). unidb's realtime today is
> **Postgres-Changes-equivalent only**: `GET /events/subscribe` (SSE) streams
> WAL-derived, RLS-filtered row change events (E1). Supabase Realtime has two
> more channel features that unidb lacks entirely — **Broadcast** (ephemeral
> client↔client pub/sub, not tied to the database) and **Presence** (who is
> currently subscribed to a channel, with per-client state). This adds both.

## Why these are safe by construction (ACID/perf)

Both features are **purely in-memory, server-side, ephemeral** — they never
touch the WAL, buffer pool, heap, catalog, or on-disk format. Nothing is
persisted; a server restart legitimately drops all broadcast/presence state
(same semantics as Supabase). Therefore the storage engine's ACID guarantees
are unaffected *by construction*, and the crash-injection harness stays 54/54
because no storage code path changes. The only shared-state is a per-topic
in-memory registry guarded by a `tokio`/`std` lock, entirely off the
commit/write path (mirrors E1's "delivery-side only, off the write path" rule).

## Scope

Keep the existing SSE transport (do **not** introduce WebSockets — the whole
realtime surface is SSE today; consistency beats matching Supabase's WS wire).
A named **channel/topic** is an opaque string.

### Broadcast
- `POST /realtime/broadcast/publish` — body `{ "topic": "...", "event": "...",
  "payload": <json> }`. Fans the message out to every current subscriber of
  `topic`. Returns the count of receivers (best-effort, at-most-once).
- `GET /realtime/broadcast/subscribe?topic=<t>` — SSE stream of
  `{ topic, event, payload, ts }` frames for that topic. Backed by a
  `tokio::sync::broadcast` channel per topic (bounded; lagged receivers get a
  documented drop, not backpressure onto publishers).
- In-memory hub: `HashMap<String, broadcast::Sender<BroadcastMsg>>` on
  `AppState`, senders reclaimed when the last receiver drops.

### Presence
- `GET /realtime/presence/subscribe?topic=<t>` — SSE stream that first emits a
  `sync` frame (the full current presence map for the topic) then `join`/`leave`
  deltas. The subscriber's own connection lifetime IS its presence: a drop
  guard removes it and broadcasts `leave` (this is why presence rides its own
  SSE connection).
- `POST /realtime/presence/track` — body `{ "topic": "...", "key": "...",
  "state": <json> }` associates/updates this caller's presence state under a
  presence key; pushes a `join`/`update` delta to the topic's subscribers.
- Registry: `HashMap<topic, HashMap<presence_key, (state, ref_count)>>`; a
  monotonic per-connection ref so two tabs of the same key both count.

## Authorization
- Both `subscribe` and `publish`/`track` are **JWT-gated** by the existing
  `require_jwt` middleware (same as every data-plane route). A caller must be
  authenticated; the `AuthPrincipal` is available.
- **v1 authorization model:** any authenticated principal may use any topic
  (Supabase's default before channel-authorization policies). **Channel
  authorization** (an RLS-style allow/deny per topic, e.g. a `realtime.channels`
  policy table) is a documented **follow-up**, explicitly out of v1 scope —
  flag it, don't half-build it. Note this clearly in `docs/REST_API.md`.
- Never log payloads or presence state at info level (may carry app data).

## Wire format
- Reuse the SSE framing already in `src/server/sse.rs` (axum `Sse` + `KeepAlive`).
- Broadcast frame `event:` = the user's `event` name; `data:` = the JSON.
- Presence frames use `event: sync|join|leave|update`.
- Add a Supabase-shaped variant note only if trivial; native shape is fine for v1.

## Acceptance
- Two subscribers on one topic both receive a published broadcast; a subscriber
  on a *different* topic does not (isolation test).
- A tracked presence key appears in a late subscriber's initial `sync`; closing
  the tracking connection emits `leave` to the others.
- No new `unsafe`; `#![cfg(feature = "server")]`-gated tests
  (`tests/item132_realtime.rs`) — put `#![cfg(feature = "server")]` at the top
  or it breaks plain `cargo test` (item-128 lesson).
- **Crash harness 54/54 unchanged** (proves the storage engine was untouched).
- `cargo test --no-run` (no features) + `clippy --all-features --all-targets
  -D warnings` + `fmt` all clean.
- `docs/REST_API.md` (new "Realtime Broadcast & Presence" section), `README.md`
  realtime bullet, and this file's Status flipped on merge.

## Non-goals (v1)
- No WebSocket transport. No persistence/replay of broadcast/presence. No
  channel-authorization policy engine (follow-up). No cross-node fan-out
  (single-primary; a multi-node realtime bus is out of scope like all
  distributed features per `CLAUDE.md` §1).
