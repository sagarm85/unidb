# Events / Realtime dispatcher — consume the WAL-derived event stream downstream

**Type:** Milestone
**Status:** NOT STARTED

> Supabase-"Realtime" analog over a primitive Supabase does not have: unidb's M4
> event queue is CDC captured **atomically with the commit** (one WAL — no
> Debezium-style lag/split-brain). This milestone makes that stream consumable
> by downstream services and the studio, WITHOUT teaching the engine any
> application shape (Milestone-18 boundary: the engine stops at a generic
> surface; apps own their delivery semantics).

## What already exists (do not rebuild)

- M4: `enable_events(table)` — every committed write on an enabled table emits
  an event in the same WAL append/commit; durable consumer offsets; replay.
- Server: subscribe API (M5) + `POST /events/vacuum` all-consumers contract
  (item 12).

## Scope

- **E1 — Browser-friendly streaming (engine-server, small).** Verify/extend the
  subscribe route to SSE (and/or WebSocket) framing so a browser can consume
  without a proxy. AC: studio tails a table's events live over the documented
  route; heartbeats + resume-from-offset on reconnect.
- **E2 — Dispatcher service (app layer; own crate `unidb-dispatch` or studio
  backend module).** Subscribes from a durable offset and fans out:
  (a) WebSocket/SSE rooms (studio live-table), (b) webhooks with retry +
  dead-letter TABLE in unidb (dogfood), (c) optional Kafka bridge — only if a
  real consumer demands it. Per-subscription filter/projection (table, op kind,
  column subset). **No transformation in the engine** — events stay raw
  row-level facts; transformation is consumer-side.
- **E3 — Event schema contract (docs).** Document the event payload (table, op,
  row image/id, xid, LSN/offset, timestamp) in `engine_access_guide.md` as a
  stable surface; state the replay + vacuum-horizon contract for consumers.
- **E4 — Studio "Events" tab.** Live stream viewer, offset scrubbing,
  replay-from-offset, per-table enable/disable (existing engine call).

## Acceptance

- [ ] A downstream demo service consumes INSERT/UPDATE/DELETE events for an
      enabled table with at-least-once delivery + resume after restart
      (offset-durable), zero events lost across an engine crash (replay proof).
- [ ] Webhook fan-out retries into the dead-letter table on a failing endpoint.
- [ ] Engine surface unchanged beyond E1 framing; no app REST in the engine.

## Notes / landmines

- Slow-consumer vs vacuum: the M4 durability contract (all-consumers vacuum)
  bounds retention — dispatcher must surface "consumer too far behind" loudly.
- This unblocks item 23 (storage service outbox).
