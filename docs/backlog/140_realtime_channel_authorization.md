# Realtime channel authorization

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #243) — role-based topic-glob channel policies in the control-plane store; enforced at all four realtime routes at connect/publish time (403 before stream opens); service_role/superuser bypass audited; fail-closed. Opt-in `UNIDB_REALTIME_REQUIRE_AUTHZ` (default OFF = item-132 open; ON = no-policy denied). Superuser `PUT/DELETE/GET /realtime/policies` + Engine methods. Crash 54/54.

> Wave-1 free-roadmap item (`137`), the item-132 follow-up. Today any
> authenticated principal (any JWT passing `require_jwt`) may publish/subscribe/
> track on ANY broadcast/presence topic. This adds an **RLS-style per-topic
> allow/deny** so operators can restrict channels by role. Control-plane only
> (in-memory policy store, plan-time check) — no storage-engine change, crash
> harness stays 54/54.

## Design (locked for v1 — a reversible, back-compat-safe choice)

**Policy model — role-based, topic-glob.** A channel policy is
`(topic_pattern, operation, allowed_roles)` where:
- `topic_pattern` is an exact topic or a `*`-suffix glob (`room:*`) — most-
  specific match wins (longest non-wildcard prefix).
- `operation` ∈ `{publish, subscribe, presence, all}`.
- `allowed_roles` is a set of role names (reusing the existing role system +
  `effective_roles`, incl. built-in `anon`/`authenticated`/`service_role`).
Stored in the existing control-plane store (`roles.json`-style, like RLS
policies / column grants) — `#[serde(default)]`, no FORMAT_VERSION bump.

**Default posture — OPT-IN, fail-closed once on (preserves item-132 behavior).**
- `UNIDB_REALTIME_REQUIRE_AUTHZ` (default **off**): a topic with **no matching
  policy** stays open to any authenticated principal — item-132 back-compat.
- When **on**: a topic with no matching policy is **denied** (fail-closed),
  exactly like RLS-enabled-no-policy.
- Either way, when a policy DOES match a topic, it is enforced: allow only if the
  caller has an allowed role for that operation; otherwise `403`.
- **`service_role` / superuser bypass**, audited (same `service_role_rls_bypass`
  posture as the query/realtime-E1 path, item-103) — do NOT add a second
  unaudited bypass.
- This "default reversible" choice is documented so a future maintainer can flip
  the default to closed with sign-off if desired (§0.6 escalation ethos).

## Admin surface
Topics are not SQL objects, so add a small **superuser-only** management surface
mirroring how other control-plane policy is set:
- `PUT /realtime/policies` — body `{ topic_pattern, operation, roles: [...] }`
  (upsert); `DELETE /realtime/policies` — `{ topic_pattern, operation }`;
  `GET /realtime/policies` — list (superuser only). All superuser-gated.
- Plus `Engine`/`EngineHandle` methods (`set_channel_policy`/`remove_channel_
  policy`/`list_channel_policies`) so the embedded crate can manage them too.

## Enforcement points (all four item-132 routes)
- `POST /realtime/broadcast/publish` → `publish` op on the body's topic.
- `GET  /realtime/broadcast/subscribe` → `subscribe` op on `?topic=`.
- `GET  /realtime/presence/subscribe` → `presence` (or `subscribe`) op.
- `POST /realtime/presence/track` → `presence` op.
Resolve the caller's effective roles once (existing `effective_roles`), check
before opening the stream / accepting the publish. Deny = `403 PERMISSION_DENIED`
at request time (not a silently empty stream — mirrors E1's subscribe-time gate).

## Correctness / security
- **Fail-closed everywhere:** a policy-store lookup error, an ambiguous match, or
  (enforce-mode) no-match → deny, never allow.
- Never log topic policy internals or tokens at info.
- The check is plan-time/in-memory — no per-frame cost beyond the one-time
  gate at publish/subscribe (subscribe checks once at connect; a policy change
  mid-stream does not retroactively kill an open stream — documented, matches
  E1's connect-time grant gate).

## Acceptance
- With `REQUIRE_AUTHZ` off + no policy: item-132 behavior unchanged (any authed
  principal) — existing `item132_realtime` tests pass untouched.
- A policy `("room:*", subscribe, ["member"])`: a caller with `member` subscribes;
  one without gets `403`; `service_role` bypasses (audited).
- With `REQUIRE_AUTHZ` on + a topic with no policy → `403` (fail-closed).
- Most-specific match wins (`room:42` policy overrides `room:*`).
- Superuser-only management routes (non-superuser → `403`).
- New `tests/item140_realtime_authz.rs` (`#![cfg(feature = "server")]` first line).
- Every pre-existing `item132_realtime` test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (policy routes + env var + enforcement semantics), `137`
  Wave-1 line, this Status flipped on merge.

## Non-goals (v1)
- Predicate/row-level channel policies (Supabase's full `realtime.messages` RLS) —
  role-based is the v1; a predicate model is a later item if needed.
- Retroactive kill of open streams on policy change.
