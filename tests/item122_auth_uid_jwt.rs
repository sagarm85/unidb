// Item 122 (Workstream B1/B2) acceptance tests — auth.uid() / auth.jwt() ->> 'claim'.
//
// B1 — `auth.uid()`: resolves to the authenticated principal's subject,
//   substituted at policy-injection time exactly like `current_user`.
// B2 — `auth.jwt() ->> 'claim'`: resolves to the literal text value of the
//   verified token's claim, carried through `ExecCtx.auth_claims` (never
//   parsed out of user SQL).
//
// Both fail CLOSED (typed Null, never `Bool(true)`) when the subject/claim is
// absent — the item-110 regression this mirrors.

use std::collections::BTreeMap;

use serde_json::json;
use tempfile::tempdir;
use unidb::{sql::executor::ExecResult, sql::logical::Literal, AuthPrincipal, DbError, Engine};

// ── helpers ──────────────────────────────────────────────────────────────────

fn policy_violation<T: std::fmt::Debug>(r: &unidb::error::Result<T>) -> bool {
    matches!(r, Err(DbError::SqlPlan(ref msg)) if msg.contains("violates policy"))
}

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

/// Auth DDL (CREATE USER / GRANT / CREATE POLICY) as the embedded superuser.
fn auth_ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

/// Execute SQL as a full [`AuthPrincipal`] (subject + claims), returning the
/// raw `Literal` rows. This is the item-122 entry point — `execute_sql_as`
/// alone carries no claims.
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

fn text(lit: &Literal) -> &str {
    match lit {
        Literal::Text(s) => s.as_str(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── B1: auth.uid() SELECT policy — positive/negative ────────────────────────

/// A SELECT policy `owner = auth.uid()` isolates rows per authenticated
/// subject, exactly like `current_user` — positive case (own rows visible)
/// and negative case (other users' rows are not).
#[test]
fn auth_uid_select_policy_positive_negative() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(
        &engine,
        "CREATE TABLE posts (id INT, owner TEXT, body TEXT)",
    )
    .unwrap();
    auth_ddl(&engine, "CREATE USER alice");
    auth_ddl(&engine, "CREATE USER bob");
    auth_ddl(&engine, "GRANT SELECT ON posts TO alice");
    auth_ddl(&engine, "GRANT SELECT ON posts TO bob");

    exec(
        &engine,
        "INSERT INTO posts (id, owner, body) VALUES (1, 'alice', 'a1'), (2, 'bob', 'b1'), (3, 'alice', 'a2')",
    )
    .unwrap();

    auth_ddl(
        &engine,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = auth.uid())",
    );

    // Positive: alice sees exactly her 2 rows.
    let alice = principal("alice", &[]);
    let alice_rows = exec_as_principal(&engine, &alice, "SELECT id, owner FROM posts").unwrap();
    assert_eq!(
        alice_rows.len(),
        2,
        "alice should see 2 rows, got {alice_rows:?}"
    );
    for row in &alice_rows {
        assert_eq!(text(&row[1]), "alice");
    }

    // Negative: bob does not see alice's rows — only his own 1 row.
    let bob = principal("bob", &[]);
    let bob_rows = exec_as_principal(&engine, &bob, "SELECT id, owner FROM posts").unwrap();
    assert_eq!(bob_rows.len(), 1, "bob should see 1 row, got {bob_rows:?}");
    assert_eq!(text(&bob_rows[0][1]), "bob");
}

// ── B1: auth.uid() superuser bypass ──────────────────────────────────────────

/// The embedded/superuser path must bypass an `auth.uid()`-referencing policy
/// entirely — never evaluate it to Null and silently hide all rows (same
/// reasoning `current_user_superuser_bypass` already covers for `current_user`).
#[test]
fn auth_uid_superuser_bypass() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE docs (id INT, owner TEXT)").unwrap();
    exec(
        &engine,
        "INSERT INTO docs (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .unwrap();

    auth_ddl(
        &engine,
        "CREATE POLICY p ON docs FOR SELECT USING (owner = auth.uid())",
    );

    let rows = exec(&engine, "SELECT id FROM docs").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "superuser must bypass an auth.uid() policy and see all 2 rows, got {rows:?}"
    );
}

// ── B1: auth.uid() INSERT policy — positive/negative ─────────────────────────

/// A `FOR INSERT` policy `owner = auth.uid()` rejects rows whose owner
/// doesn't match the authenticated subject.
#[test]
fn auth_uid_insert_policy_positive_negative() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE items (id INT, owner TEXT)").unwrap();
    auth_ddl(&engine, "CREATE USER alice");
    auth_ddl(&engine, "GRANT INSERT ON items TO alice");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON items FOR INSERT USING (owner = auth.uid())",
    );

    let alice = principal("alice", &[]);

    let x = engine.begin().unwrap();
    let ok = engine.execute_sql_as_principal(
        &alice,
        x,
        "INSERT INTO items (id, owner) VALUES (1, 'alice')",
    );
    engine.commit(x).unwrap();
    assert!(
        ok.is_ok(),
        "alice inserting her own row must succeed: {ok:?}"
    );

    let x = engine.begin().unwrap();
    let bad = engine.execute_sql_as_principal(
        &alice,
        x,
        "INSERT INTO items (id, owner) VALUES (2, 'bob')",
    );
    engine.abort(x).unwrap();
    assert!(
        policy_violation(&bad),
        "alice inserting bob's row must violate the auth.uid() policy, got {bad:?}"
    );
}

// ── B2: auth.jwt() ->> 'tenant' — tenant isolation under two tokens ──────────

/// The item-122 headline acceptance test: `USING (tenant_id = (auth.jwt() ->>
/// 'tenant')))` isolates two tenants' rows under two different tokens
/// (principals), count-asserted. Note the required parens around the ->>
/// extraction — `->>` binds looser than `=` under sqlparser's precedence
/// table (the same pre-existing caveat as the JSON `->>` operator), so
/// `tenant_id = auth.jwt() ->> 'tenant'` (no inner parens) groups wrong.
#[test]
fn auth_jwt_tenant_isolation_two_tokens() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(
        &engine,
        "CREATE TABLE rows_t (id INT, tenant_id TEXT, val TEXT)",
    )
    .unwrap();
    auth_ddl(&engine, "CREATE USER svc");
    auth_ddl(&engine, "GRANT SELECT ON rows_t TO svc");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON rows_t FOR SELECT USING (tenant_id = (auth.jwt() ->> 'tenant'))",
    );

    exec(
        &engine,
        "INSERT INTO rows_t (id, tenant_id, val) VALUES \
         (1, 'acme', 'a1'), (2, 'acme', 'a2'), (3, 'globex', 'g1')",
    )
    .unwrap();

    // Token A: tenant = acme.
    let tok_acme = principal("svc", &[("tenant", json!("acme"))]);
    let acme_rows =
        exec_as_principal(&engine, &tok_acme, "SELECT id, tenant_id FROM rows_t").unwrap();
    assert_eq!(
        acme_rows.len(),
        2,
        "acme token should see 2 rows, got {acme_rows:?}"
    );
    for row in &acme_rows {
        assert_eq!(text(&row[1]), "acme");
    }

    // Token B: tenant = globex — must NOT see acme's rows.
    let tok_globex = principal("svc", &[("tenant", json!("globex"))]);
    let globex_rows =
        exec_as_principal(&engine, &tok_globex, "SELECT id, tenant_id FROM rows_t").unwrap();
    assert_eq!(
        globex_rows.len(),
        1,
        "globex token should see 1 row, got {globex_rows:?}"
    );
    assert_eq!(text(&globex_rows[0][1]), "globex");
}

// ── B2: absent claim fails closed (no rows, not all rows) ────────────────────

/// A principal with a subject but no `tenant` claim must see ZERO rows under
/// the tenant policy — never all rows. This is the fail-closed contract: a
/// missing claim substitutes `Literal::Null`, and `tenant_id = NULL` is never
/// true for any row (predicate-Null == false), so the row is filtered out —
/// it must never fall back to `Bool(true)` (the item-110 regression).
#[test]
fn auth_jwt_missing_claim_fails_closed() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(
        &engine,
        "CREATE TABLE rows_t (id INT, tenant_id TEXT, val TEXT)",
    )
    .unwrap();
    auth_ddl(&engine, "CREATE USER svc");
    auth_ddl(&engine, "GRANT SELECT ON rows_t TO svc");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON rows_t FOR SELECT USING (tenant_id = (auth.jwt() ->> 'tenant'))",
    );
    exec(
        &engine,
        "INSERT INTO rows_t (id, tenant_id, val) VALUES (1, 'acme', 'a1'), (2, 'globex', 'g1')",
    )
    .unwrap();

    // svc's token carries no 'tenant' claim at all.
    let tok_no_claim = principal("svc", &[]);
    let rows = exec_as_principal(&engine, &tok_no_claim, "SELECT id FROM rows_t").unwrap();
    assert_eq!(
        rows.len(),
        0,
        "missing claim must fail CLOSED (0 rows), not open (all rows): got {rows:?}"
    );
}

/// Same fail-closed contract for `auth.uid()`, at the integration level: a
/// named, non-superuser identity whose subject doesn't match any `owner`
/// value must see zero rows, never all rows.
///
/// Note on scope: this crate treats a token with **no** `sub` claim at all
/// (`AuthPrincipal::user(None)`) as the implicit superuser (see
/// `src/server/auth.rs`'s `Claims::sub` doc and `execute_sql_inner_as_principal`'s
/// `skip_rls` — `None` always bypasses RLS). That is a pre-existing,
/// out-of-scope-for-B1/B2 design point (an `anon`-vs-superuser distinction is
/// Workstream B3's `anon` role, not this slice) — so "no subject at all"
/// cannot be exercised as a non-bypassed case through this entry point today.
/// The true `subject: None` fail-closed substitution (Null, never
/// `Bool(true)`) is instead unit-tested directly against
/// `substitute_auth_context_in_expr` in `src/sql/logical.rs`.
#[test]
fn auth_uid_unmatched_subject_returns_zero_rows() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE docs2 (id INT, owner TEXT)").unwrap();
    auth_ddl(&engine, "CREATE USER root SUPERUSER");
    auth_ddl(&engine, "CREATE USER nobody");
    auth_ddl(&engine, "GRANT SELECT ON docs2 TO nobody");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON docs2 FOR SELECT USING (owner = auth.uid())",
    );
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(
            Some("root"),
            x,
            "INSERT INTO docs2 (id, owner) VALUES (1, 'alice'), (2, 'bob')",
        )
        .unwrap();
    engine.commit(x).unwrap();

    // A named, non-superuser identity with a policy that needs auth.uid() —
    // subject IS present here ("nobody"), so this exercises the ordinary
    // (non-bypass) path with a real-but-unmatched subject: zero rows.
    let named_no_match = AuthPrincipal::user(Some("nobody".to_string()));
    let rows = exec_as_principal(&engine, &named_no_match, "SELECT id FROM docs2").unwrap();
    assert_eq!(
        rows.len(),
        0,
        "a subject with no matching owner rows must see 0 rows, got {rows:?}"
    );
}

// ── B2: auth.jwt() through the QuerySpec/QExpr path (SELECT ... LIMIT) ───────

/// `SELECT ... LIMIT n` routes through `LogicalPlan::Query`/`QuerySpec`, whose
/// RLS injection eagerly converts the policy `Expr` to `QExpr` via
/// `qualify_policy` (the exact path item 110 broke for `current_user`).
/// Confirms `auth.jwt()` substitution happens BEFORE that conversion too, so
/// `qualify_policy`'s fail-closed fallback (`Expr::AuthClaim` arm) is never
/// actually hit on the happy path — the claim resolves correctly with LIMIT.
#[test]
fn auth_jwt_with_limit_uses_qexpr_path() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    exec(&engine, "CREATE TABLE rows_lim (id INT, tenant_id TEXT)").unwrap();
    auth_ddl(&engine, "CREATE USER svc");
    auth_ddl(&engine, "GRANT SELECT ON rows_lim TO svc");
    auth_ddl(
        &engine,
        "CREATE POLICY p ON rows_lim FOR SELECT USING (tenant_id = (auth.jwt() ->> 'tenant'))",
    );
    exec(
        &engine,
        "INSERT INTO rows_lim (id, tenant_id) VALUES (1, 'acme'), (2, 'acme'), (3, 'globex')",
    )
    .unwrap();

    let tok_acme = principal("svc", &[("tenant", json!("acme"))]);
    let rows = exec_as_principal(&engine, &tok_acme, "SELECT id FROM rows_lim LIMIT 10").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "RLS + LIMIT with auth.jwt() must return the filtered rows, got {rows:?}"
    );
}
