# Supabase parity — remaining follow-ups

**Type:** Milestone
**Status:** SUPERSEDED (2026-08-01) by [`137_supabase_parity_free_roadmap.md`](137_supabase_parity_free_roadmap.md)
— the authoritative, free/self-hostable-filtered roadmap (excludes paid-service items
per the user). This file is kept for history; use 137 for planning.

> Living checklist of everything still open toward Supabase-class parity **after**
> the 120–133 build (auth, RLS↔token, auto REST + GraphQL read *and write*,
> realtime changes + broadcast + presence, storage authz, MFA, OAuth, vault,
> CAPTCHA, migrations, rate-limiting, JS SDK). This is a map + priority list, like
> item 120; **each item below gets its own numbered `NN_<slug>.md` when work
> starts** (next free ID is in `backlog_index.md`). Nothing here is in progress.
>
> **Session note (2026-08-01):** filed to hand off to a fresh session. The last
> session shipped items 132 (realtime broadcast/presence) + 133 (GraphQL
> mutations); the A/B/C sections below are what remained unbuilt or deferred.

---

## A. Small / correctness follow-ups from items 132–133 (do these first — cheap, contained)

1. **Named `SUPERUSER` not exempted from `WITH CHECK` on the per-row INSERT path**
   *(correctness, from item 133).* `sql/executor.rs::exec_insert` only bypasses a
   table's `WITH CHECK` policy for the embedded `None` caller and `service_role`,
   **not** for a named superuser principal — unlike the plan-level `apply_rls`
   skip, which *does* cover named superusers. Result: a named superuser can be
   blocked by a `WITH CHECK` policy on INSERT where the query path would let them
   through. Pre-existing; surfaced (with a test workaround) while writing item
   133's parity tests. Fix: make `exec_insert`'s bypass consistent with the
   plan-level superuser skip. Add a regression test proving named-superuser INSERT
   parity between `/sql` and the executor path. **Must keep crash 54/54.**

2. **GraphQL bulk insert `insert_<t>(values: [JSON!]!)`** *(item 133 deferred
   nice-to-have).* v1 shipped single-row insert only. Add the array variant,
   reusing the REST bulk-insert statement builder; requested-projection-only
   RETURNING as in the single-row path.

3. **Presence `track` orphan-entry gap** *(item 132 accepted v1 gap).* A
   `POST /realtime/presence/track` with **zero** live `presence/subscribe`
   connections for that `(topic, identity)` creates an entry that persists until a
   matching connection later opens and closes. Options: reject `track` with no
   live connection, or TTL-expire orphaned entries. Decide + implement.

## B. Realtime + GraphQL next tier

4. **Realtime channel-authorization policies** *(from item 132).* Today any
   authenticated principal may use any broadcast/presence topic. Add an RLS-style
   per-topic allow/deny (e.g. a `realtime.channels` policy relation, or reuse the
   policy engine keyed by topic pattern). Fail-closed. This is the main thing
   standing between v1 realtime and Supabase's channel-authorization model.

5. **GraphQL subscriptions** *(from item 133).* A `Subscription` root over the
   realtime SSE/broadcast work (item 132) so GraphQL clients get live updates.
   Must inherit RLS the same way the E1 realtime filter does (per-subscriber row
   filtering), not a parallel path.

## C. Larger Supabase gaps (each a milestone; prioritize by demand)

6. **Email transport + email auth flows** *(unblocks the biggest held cluster).*
   A pluggable email transport (SMTP + a dev log-transport) then: **magic-link /
   email OTP** (D2/D5, currently held), **password reset via email**, and **email
   confirmation** on signup. One transport unlocks all three. `ALTER USER …
   PASSWORD` already exists for the admin path; this is the self-service path.
7. **SMS / phone OTP** (D3, held) — needs an SMS transport; lower priority.
8. **More OAuth providers** — generalize item 128's provider-agnostic core beyond
   Google/GitHub (Apple, Azure, GitLab, Discord, …); mostly config + userinfo
   mapping. **SAML / enterprise SSO** is a separate, larger effort.
9. **Storage: image transformations + resumable (TUS) uploads + CDN semantics** —
   item 125 shipped per-object authz + presign + public/private; these are the
   remaining Supabase Storage features.
10. **Database webhooks (outbound HTTP on row change)** — unidb has the WAL-derived
    event queue (M4) + realtime; add an outbound HTTP sink that POSTs change
    events to a configured URL with retries. Distinct from the SSE subscribe path.
11. **Scheduled jobs (`pg_cron` analog)** — run SQL on a schedule inside the engine.
12. **Backups / PITR polish** — R1/R2 replication + `restore_to_time` shipped
    (item 28); a user-facing backup/restore + retention story is still thin.
13. **SDK breadth** — `unidb-js` shipped (see cross-repo below); Python/Dart/Swift/
    Kotlin clients are unbuilt.

## D. Cross-repo pending (not in this engine repo — pointers for the fresh session)

- **`sagarm85/unidb-studio`** — Workstream G panels branch
  `claude/studio-supabase-panels-6y1755` has a tip commit (`4efdb573`) **not yet
  merged to studio `main`** (main is at PR #17). Newly-unblocked studio work after
  this session's engine merges: **GraphQL explorer panel** (C4/#232 + mutations
  #235), **MFA UI** (D4/#229), **OAuth UI** (D1/#230), and finishing the deferred
  **C2 embedded-resources example** (#227). A ready-to-paste studio prompt was
  drafted in the last session — regenerate against this list.
- **`sagarm85/unidb-js`** — SDK v0.1.0 shipped; its own `MEMORY.md` lists: live
  integration tests (env-gated), realtime before/after mapping verification, a
  **storage client module** (F1), and **npm publish + CI**. Also now worth adding:
  a **GraphQL client** helper (queries + the new mutations) and **broadcast/
  presence** channel helpers (items 132/133).

---

## Explicitly out of scope (locked; do NOT pull forward without sign-off)

- Multi-project / hosted control plane (I6) — user-excluded.
- Edge Functions / serverless (I7) — parked by user.
- Distributed consensus / multi-node realtime bus — out of scope per `CLAUDE.md` §1.
