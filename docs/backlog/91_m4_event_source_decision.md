**Type:** Improvement
**Status:** ⏳ NOT STARTED — **design decision required BEFORE M4 implementation starts**

# Item 91 — M4 event-source decision: slim WAL vs before-images

## Problem

M4's design (CLAUDE.md §5) describes the event queue as **WAL-derived**. The
2026-07-19 WAL slimming that made bulk DML fast also made the WAL
insufficient for that purpose as-is:

- DELETE selected now logs **5 B/row** (WAL_XMAX_BATCH: slot list only) —
  a WAL-derived consumer cannot reconstruct the deleted row's content.
- UPDATE HOT logs the new version but the *before* image is only reachable
  via the heap prev-pointer chain, which vacuum eventually removes.

Today this is masked because CDC events are captured at the **executor level**
(`send_event_capture`, before-image in hand) — but that mechanism only fires
for `events_enabled` tables, and M4's replay/offset semantics are specified
against the WAL.

## Decision to make (pick one, record sign-off in PROGRESS.md)

- **Option A — executor capture is the source of truth.** M4's durable queue
  is fed by `send_event_capture` writing event records into the same WAL/txn
  (as today), and "WAL-derived" is reinterpreted as "the event records live in
  the WAL", not "derived from physical redo records". Cheapest; keeps slim
  DML records; replay = scan event records. Cost: per-row event encoding on
  the commit path for events-enabled tables (item 60 already minimized this).
- **Option B — opt-in logical WAL level.** Tables (or the engine) can enable
  `wal_level=logical`-style before-images on DML records (PG REPLICA IDENTITY
  analog). Physical derivation stays possible; slim records remain the
  default. Cost: WAL volume regression on opted-in tables + format work.

Postgres precedent: fast physical WAL by default, logical decoding strictly
opt-in — i.e. Option A now, Option B later if external CDC consumers need it.

## Why now

Deciding after M4 implementation starts means either retrofitting before-
images into shipped WAL formats (FORMAT_VERSION churn) or rewriting M4's
replay path. The decision is cheap today, expensive later.

## Acceptance criteria

- Decision recorded with sign-off in PROGRESS.md; M4 section of
  `docs/design/engine_design.md` and this file updated to match.
- If Option A: M4 spec explicitly states event records are the replay source
  and slim DML records are non-goals for decoding.
