// Item 122 (Workstream B3/B4) acceptance tests — built-in roles + role-scoped
// policies.
//
// B3 — built-in roles: `anon` / `authenticated` / `service_role` are reserved
//   (never created/dropped via `CREATE`/`DROP ROLE`) and resolved
//   ENGINE-side (`RoleStore::effective_roles`) from the caller's verified
//   subject/claims — never trusted from `AuthPrincipal.roles` as supplied by
//   the HTTP layer, which never populates it.
// B4 — role-scoped policies: `CREATE POLICY ... TO <role,...>` only applies
//   to callers whose effective roles intersect the target set; a no-`TO`
//   policy still applies to everyone (back-compat, unchanged from pre-B4).
//
// Note on exercising `anon` end-to-end: `subject: None` is the pre-existing,
// protected "embedded API / no-`sub` token" superuser bypass (item 103) —
// it ALWAYS skips RLS entirely, by design, before role resolution even
// matters. So a literal "no-subject caller sees fewer rows" scenario can't
// be exercised through the public engine API (and must not be — changing
// that bypass would break dozens of existing tests across every workstream).
// Item 122 B3 additionally maps an explicit `claims["role"] == "anon"` token
// (the real Supabase anon-key JWT convention — the anon key carries a `role`
// claim, it is not inferred from `sub`'s absence) to the `anon` role, which
// *is* reachable: it carries a real `Some(subject)`, so it goes through the
// ordinary (non-bypass) RLS path exactly like any other named caller. That
// is what the tests below use to exercise `anon` concretely.

use std::collections::BTreeMap;

use serde_json::json;
use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, AuthPrincipal, DbError, Engine};

// ── helpers (mirrors tests/item122_auth_uid_jwt.rs) ─────────────────────────

/// Execute SQL as the embedded superuser (None identity).
fn exec(engine: &Engine, sql: &str) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql(x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    Ok(out)
}

/// Auth DDL (CREATE USER / GRANT / CREATE POLICY / CREATE ROLE) as the
/// embedded superuser. Returns the raw `Result` so callers can assert on
/// failure (e.g. reserved-role rejection) instead of unwrapping.
fn auth_ddl(engine: &Engine, sql: &str) -> unidb::error::Result<Vec<ExecResult>> {
    let x = engine.begin().unwrap();
    let r = engine.execute_sql_as(None, x, sql);
    if r.is_ok() {
        engine.commit(x).unwrap();
    } else {
        engine.abort(x).unwrap();
    }
    r
}

fn exec_as_principal(
    engine: &Engine,
    principal: &AuthPrincipal,
    sql: &str,
) -> unidb::error::Result<Vec<Vec<Literal>>> {
    let x = engine.begin()?;
    let rows = engine.execute_sql_as_principal(principal, x, sql)?;
    engine.commit(x)?;
    let mut out = Vec::new();
    for r in rows {
        if let ExecResult::Rows { rows: inner, .. } = r {
            out.extend(inner);
        }
    }
    Ok(out)
}

fn principal(subject: &str, claims: &[(&str, serde_json::Value)]) -> AuthPrincipal {
    let mut map = BTreeMap::new();
    for (k, v) in claims {
        map.insert((*k).to_string(), v.clone());
    }
    AuthPrincipal {
        subject: Some(subject.to_string()),
        claims: map,
        roles: Vec::new(),
    }
}

// ── (a) `TO authenticated` hides rows from an anon caller, shows them to an
//        authenticated one ──────────────────────────────────────────────────

#[test]
fn to_authenticated_policy_hides_from_anon_shows_to_authenticated() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE docs (id INT, body TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO docs (id, body) VALUES (1, 'a'), (2, 'b')",
    )
    .unwrap();

    // Both callers need a SELECT grant to even reach the RLS gate (grant
    // checks run first) — neither is a superuser or in bootstrap mode.
    auth_ddl(&engine, "CREATE USER anon_caller").unwrap();
    auth_ddl(&engine, "CREATE USER auth_caller").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON docs TO anon_caller").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON docs TO auth_caller").unwrap();

    // A `TO authenticated`-only policy: no no-TO fallback exists, so a
    // caller whose effective roles don't include `authenticated` must be
    // denied outright (item 122 B4 deny-by-default), not left unrestricted.
    auth_ddl(
        &engine,
        "CREATE POLICY p ON docs FOR SELECT TO authenticated USING (true)",
    )
    .unwrap();

    // anon: a real `Some(subject)` carrying `role: anon` — reaches the
    // ordinary RLS path (not the None-subject superuser bypass) and
    // resolves to the `anon` role, which the policy doesn't target.
    let anon = principal("anon_caller", &[("role", json!("anon"))]);
    let anon_rows = exec_as_principal(&engine, &anon, "SELECT id FROM docs").unwrap();
    assert_eq!(
        anon_rows.len(),
        0,
        "anon caller must see 0 rows under a TO-authenticated-only policy, got {anon_rows:?}"
    );

    // authenticated: an ordinary named subject with no special role claim
    // resolves to `authenticated` (item 122 B3 default mapping) and matches.
    let authed = principal("auth_caller", &[]);
    let authed_rows = exec_as_principal(&engine, &authed, "SELECT id FROM docs").unwrap();
    assert_eq!(
        authed_rows.len(),
        2,
        "authenticated caller must see both rows, got {authed_rows:?}"
    );
}

// ── (b) two users in different roles see role-appropriate rows ─────────────

#[test]
fn two_users_in_different_roles_see_role_appropriate_rows() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE reports (id INT, body TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO reports (id, body) VALUES (1, 'q1'), (2, 'q2'), (3, 'q3')",
    )
    .unwrap();

    auth_ddl(&engine, "CREATE USER alice").unwrap();
    auth_ddl(&engine, "CREATE USER bob").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON reports TO alice").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON reports TO bob").unwrap();
    auth_ddl(&engine, "CREATE ROLE manager").unwrap();
    auth_ddl(&engine, "GRANT manager TO alice").unwrap();

    auth_ddl(
        &engine,
        "CREATE POLICY p ON reports FOR SELECT TO manager USING (true)",
    )
    .unwrap();

    // Alice: authenticated + manager -> matches the policy's target set.
    let alice = principal("alice", &[]);
    let alice_rows = exec_as_principal(&engine, &alice, "SELECT id FROM reports").unwrap();
    assert_eq!(
        alice_rows.len(),
        3,
        "alice (manager) should see all 3 rows, got {alice_rows:?}"
    );

    // Bob: authenticated only, no manager role -> denied (deny-by-default).
    let bob = principal("bob", &[]);
    let bob_rows = exec_as_principal(&engine, &bob, "SELECT id FROM reports").unwrap();
    assert_eq!(
        bob_rows.len(),
        0,
        "bob (no manager role) should see 0 rows, got {bob_rows:?}"
    );
}

// ── (c) a no-`TO` policy still applies to everyone (back-compat) ───────────

#[test]
fn policy_without_to_clause_still_applies_to_everyone() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE notes (id INT, owner TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO notes (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .unwrap();

    auth_ddl(&engine, "CREATE USER carol").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON notes TO carol").unwrap();
    // No TO clause at all — pre-B4 grammar, must behave exactly as before.
    auth_ddl(&engine, "CREATE POLICY p ON notes FOR SELECT USING (true)").unwrap();

    let carol = principal("carol", &[]);
    let rows = exec_as_principal(&engine, &carol, "SELECT id FROM notes").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "a no-TO policy must apply to every caller unchanged, got {rows:?}"
    );

    // Even an anon-labeled caller sees the rows — a no-TO policy has an
    // empty target-role set, so it's not gated by role at all.
    let anon = principal("carol", &[("role", json!("anon"))]);
    let anon_rows = exec_as_principal(&engine, &anon, "SELECT id FROM notes").unwrap();
    assert_eq!(
        anon_rows.len(),
        2,
        "a no-TO policy must apply to an anon caller too, got {anon_rows:?}"
    );
}

// ── (d) service_role bypasses RLS and is audited ────────────────────────────

#[test]
fn service_role_bypasses_rls_and_is_audited() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE secrets (id INT, owner TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO secrets (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .unwrap();

    auth_ddl(&engine, "CREATE USER svc").unwrap();
    // Deliberately no GRANT for svc — service_role bypasses privilege checks
    // too (it "bypasses RLS like a superuser"), not just the RLS predicate.
    auth_ddl(
        &engine,
        "CREATE POLICY p ON secrets FOR SELECT TO authenticated USING (owner = auth.uid())",
    )
    .unwrap();

    let svc = principal("svc", &[("role", json!("service_role"))]);
    let rows = exec_as_principal(&engine, &svc, "SELECT id FROM secrets").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "service_role must bypass RLS entirely and see all rows, got {rows:?}"
    );

    // The bypass must be audited (item-103 lesson: distinct from the
    // unaudited implicit-superuser path).
    let audit_log = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    assert!(
        audit_log.contains("service_role_rls_bypass"),
        "service_role bypass must be recorded in the audit log, got: {audit_log}"
    );
    assert!(
        audit_log.contains("\"svc\""),
        "the audited event must name the acting subject, got: {audit_log}"
    );
}

// ── (e) reserved-role names can't be created/dropped ────────────────────────

#[test]
fn reserved_role_names_cannot_be_created_or_dropped_via_sql() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    for name in ["anon", "authenticated", "service_role"] {
        let create = auth_ddl(&engine, &format!("CREATE ROLE {name}"));
        assert!(
            matches!(create, Err(DbError::Authz(_))),
            "CREATE ROLE {name} must be rejected as reserved, got {create:?}"
        );
        let drop = auth_ddl(&engine, &format!("DROP ROLE {name}"));
        assert!(
            matches!(drop, Err(DbError::Authz(_))),
            "DROP ROLE {name} must be rejected as reserved, got {drop:?}"
        );
    }

    // A non-reserved role is unaffected.
    let ok = auth_ddl(&engine, "CREATE ROLE analyst");
    assert!(
        ok.is_ok(),
        "a non-reserved role must still be creatable: {ok:?}"
    );
}

// ── B4 grammar: multi-role TO clause + role-of-role transitive grant ───────

#[test]
fn to_clause_with_multiple_roles_and_transitive_grant() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE t (id INT)").unwrap();
    exec(&engine, "INSERT INTO t (id) VALUES (1), (2)").unwrap();

    auth_ddl(&engine, "CREATE USER dana").unwrap();
    auth_ddl(&engine, "GRANT SELECT ON t TO dana").unwrap();
    auth_ddl(&engine, "CREATE ROLE editor").unwrap();
    auth_ddl(&engine, "CREATE ROLE senior_editor").unwrap();
    // dana -> editor -> senior_editor (transitive).
    auth_ddl(&engine, "GRANT senior_editor TO editor").unwrap();
    auth_ddl(&engine, "GRANT editor TO dana").unwrap();

    auth_ddl(
        &engine,
        "CREATE POLICY p ON t FOR SELECT TO admin, senior_editor USING (true)",
    )
    .unwrap();

    // dana matches via the transitively-granted senior_editor role, even
    // though the TO clause lists "admin, senior_editor" and dana has neither
    // directly.
    let dana = principal("dana", &[]);
    let rows = exec_as_principal(&engine, &dana, "SELECT id FROM t").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "dana should match via the transitively-granted senior_editor role, got {rows:?}"
    );
}
