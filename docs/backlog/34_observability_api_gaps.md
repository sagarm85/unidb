# Item 34 — Observability API gaps

**Type:** Improvement  
**Status:** 🚀 IN PROGRESS (branch `34-observability-api-gaps`)  
**Priority:** High — Studio Observability tab charts reset on reload; slow-query panel is always blank

---

## Problem

Two related gaps in the observability surface, both causing the Studio to paper over
missing engine behaviour:

| Gap | Studio workaround today |
|-----|------------------------|
| `slow_query_threshold_us` defaults to `0` (disabled); no env var or REST route to enable | "Recent slow queries" section in Observability tab is permanently empty |
| `GET /stats` returns a single snapshot | Studio accumulates up to 60 points (~5 min) in a `$state` array; all charts reset to "Collecting data…" on every page reload |

Both share the same engine surface (`GET /stats` / `src/lib.rs`) and the same Studio
file (`ObservabilityPanel.svelte`), so they belong in one item.

---

## Proposed changes

### A — Slow query threshold configuration

**Env var (startup):**
```
UNIDB_SLOW_QUERY_MS=100   # queries ≥ 100 ms land in recent_slow_queries
```
Read in server startup config, call `engine.set_slow_query_threshold(Duration::from_millis(n))`.
`0` or absent = disabled (preserves current default).

**Runtime route (optional — enables hot-reload without restart):**
```
PUT /config/slow_query_threshold_ms
Authorization: Bearer <token>
{ "threshold_ms": 100 }
```
Response: `204 No Content`. Already atomic (`AtomicU64`) — no lock needed.

---

### B — Metrics history endpoint

Engine maintains a fixed-size ring buffer of timestamped `Stats` snapshots (suggested:
300 points × 5 s = 25 min; ~72 KiB memory).

```
GET /stats/history
Authorization: Bearer <token>
```

Optional query params:

| param | default | meaning |
|-------|---------|---------|
| `points` | 60 | number of snapshots (max 300) |
| `interval_ms` | 5000 | resolution hint |

**Response `200 OK`:**
```json
{
  "interval_ms": 5000,
  "points": [
    {
      "t": 1752350400000,
      "commits": 42, "aborts": 3, "active_transactions": 0, "wal_bytes": 81920,
      "commits_per_sec": 1.4, "wal_bytes_per_sec": 2048.0,
      "bufferpool_hit_ratio": 0.96
    }
  ]
}
```

Engine computes rate fields (`commits_per_sec`, `wal_bytes_per_sec`) from
consecutive ring entries — removes client-side delta math.

---

## Acceptance criteria

- [ ] `UNIDB_SLOW_QUERY_MS` env var enables threshold at startup
- [ ] `PUT /config/slow_query_threshold_ms` changes threshold at runtime
- [ ] `GET /stats` `recent_slow_queries` populated for queries above threshold
- [ ] Engine maintains a ≥60-point ring buffer of `Stats` snapshots
- [ ] `GET /stats/history` returns points oldest-first with rate fields
- [ ] Empty `points: []` on fresh start (not an error)
- [ ] REST API doc updated for both routes
- [ ] Integration tests for slow-query capture and history endpoint

## Studio changes to make when this ships

- `src/lib/ObservabilityPanel.svelte`:
  - On mount: prefill `history` from `GET /stats/history` (charts survive reload)
  - Replace client-side delta math with server-computed `commits_per_sec` / `wal_bytes_per_sec`
  - Remove slow-queries "always hidden" guard; section shows whenever threshold is configured
- `demo/DEMO_GUIDE.md`: document `UNIDB_SLOW_QUERY_MS=50` in engine launch step

## Engine touch points

- `src/lib.rs` — `StatsPoint` struct; `history: Mutex<VecDeque<StatsPoint>>`; `Engine::stats_history(n)`
- Background ticker in `src/server/main.rs` — snapshot every 5 s into ring
- `src/server/router.rs` — `GET /stats/history`, `PUT /config/slow_query_threshold_ms`
- `src/server/handlers.rs` — two new handlers
- Server startup config — read `UNIDB_SLOW_QUERY_MS`
- `docs/REST_API.md` — document both routes
