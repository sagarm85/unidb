# Item 33 — CDC management API

**Type:** Improvement  
**Status:** ✅ SHIPPED — branch `33-cdc-management-api`, PR pending review  
**Priority:** High — Studio CDC tab and demo scripts work around every gap here

---

## Problem

The engine has `POST /tables/{name}/events` to enable CDC but nothing to inspect or
reverse that choice. Three concrete gaps:

| Gap | Studio workaround today |
|-----|------------------------|
| No way to ask "is CDC enabled on table X?" | `cdcTables` `Set<string>` in `localStorage` (lost on clear, wrong after engine restart) |
| No way to disable CDC once enabled | UI says "Once enabled it is permanent" |
| No way to get the current event log head seq without opening an SSE stream | `demo/events_demo.py::get_current_seq()` opens SSE for 1 s, drains events, records highest seq, closes |

---

## Proposed API surface

### `GET /tables/{name}/events`

Query CDC status for one table.

**Response `200 OK`:**
```json
{ "enabled": true }
```
Returns `{ "enabled": false }` when the table exists but CDC is off.  
Returns `404 TABLE_NOT_FOUND` if the table doesn't exist.

---

### `DELETE /tables/{name}/events`

Disable CDC on a table. Drains no pending events — any already in `__events__` remain
until consumed and vacuumed. Future writes on the table no longer emit events.

**Response:** `204 No Content`  
**Idempotency decision:** Returns `204` even when CDC was already off (idempotent). Rationale: simpler client code, matches standard REST disable semantics, avoids spurious errors on repeated calls.

---

### `GET /events/head`

Return the current highest committed event sequence number without opening a stream.
Useful for "start from now" positioning (avoid replaying full event history).

**Response `200 OK`:**
```json
{ "seq": 134937 }
```
Returns `{ "seq": 0 }` if no events have ever been written.

---

## Acceptance criteria

- [x] `GET /tables/{name}/events` returns `{ "enabled": bool }`
- [x] `DELETE /tables/{name}/events` disables CDC; subsequent writes on the table don't add to `__events__`
- [x] `GET /events/head` returns max committed `seq` from `__events__`, or 0 when empty
- [x] REST API doc (`docs/REST_API.md`) updated with all three routes
- [x] Integration tests for each route (6 new tests in `tests/server_events.rs`; P34 crash test added)

## Studio changes to make when this ships

- `src/lib/eventStore.js`: remove `LS_CDC_KEY` / `localStorage` CDC set; replace with
  `GET /tables/{name}/events` calls to check status
- `src/lib/EventsPanel.svelte`: remove "Once enabled it is permanent" note
- `demo/events_demo.py`: replace `get_current_seq()` peek with `GET /events/head`

## Engine touch points

- `src/server/router.rs` — add three routes
- `src/server/handlers.rs` — `get_table_events_status`, `delete_table_events`, `get_events_head`
- `src/server/engine_handle.rs` — wrap `Engine::is_events_enabled(table)`, `Engine::disable_events(xid, table)`, `Engine::events_head_seq()`
- `src/lib.rs` — implement the three engine methods
- `docs/REST_API.md` — document the routes
