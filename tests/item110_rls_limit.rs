// Item 110 — RLS + LIMIT: current_user destroyed in the QuerySpec path.
//
// Bug: `SELECT … LIMIT n` parses to `LogicalPlan::Query(QuerySpec)`, whose
// RLS injection eagerly converts the policy `Expr` to `QExpr`; the
// conversion's fallback turned an unsubstituted `Expr::CurrentUser` into
// `Literal::Bool(true)`, so `owner = current_user` became `owner = TRUE` —
// a Text↔Bool coercion error (via the item-38 arms) on every RLS+LIMIT
// query by a non-superuser, and a silent policy weakening in shapes where
// Bool type-checks.
//
// Fix: `apply_rls` substitutes current_user into the policy expression AT
// INJECTION TIME (before the conversion), and the conversion fallback now
// fails CLOSED (Null → policy not-true for every row) instead of open.

use tempfile::tempdir;
use unidb::Engine;

fn setup(engine: &Engine) {
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE TABLE repro_test (id INT, owner TEXT)",
        "INSERT INTO repro_test (id, owner) VALUES (1, 'alice')",
        "INSERT INTO repro_test (id, owner) VALUES (2, 'zzz_unrelated')",
        "INSERT INTO repro_test (id, owner) VALUES (3, 'alice')",
        "CREATE USER alice",
        "GRANT SELECT ON repro_test TO alice",
        "CREATE POLICY repro_owner_only ON repro_test FOR SELECT USING (owner = current_user)",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();
}

fn count_rows_as(engine: &Engine, user: Option<&str>, sql: &str) -> usize {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql_as(user, x, sql).unwrap();
    engine.commit(x).unwrap();
    match res.into_iter().next().unwrap() {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The filed repro: non-superuser + current_user policy + LIMIT must return
/// the RLS-filtered rows — not an error, and not unfiltered rows.
#[test]
fn rls_with_limit_returns_filtered_rows() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);

    // Sanity: without LIMIT the policy already filters correctly.
    assert_eq!(
        count_rows_as(&engine, Some("alice"), "SELECT * FROM repro_test"),
        2,
        "no-LIMIT baseline must be RLS-filtered"
    );
    // The bug: with LIMIT this errored (`cannot coerce text 'alice' to
    // boolean`). Fixed: filtered rows, honoring the limit.
    assert_eq!(
        count_rows_as(&engine, Some("alice"), "SELECT * FROM repro_test LIMIT 10"),
        2,
        "RLS + LIMIT must return exactly the policy-visible rows"
    );
    // LIMIT below the visible count must clamp, still RLS-filtered — the
    // count assertion doubles as the silent-bypass guard from the filing.
    assert_eq!(
        count_rows_as(&engine, Some("alice"), "SELECT * FROM repro_test LIMIT 1"),
        1
    );
}

/// Superuser + LIMIT stays unaffected (item 103's bypass holds).
#[test]
fn superuser_limit_unaffected() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    assert_eq!(
        count_rows_as(&engine, None, "SELECT * FROM repro_test LIMIT 10"),
        3,
        "superuser must see all rows"
    );
}

/// LIMIT with no policy on the table stays unaffected.
#[test]
fn limit_without_policy_unaffected() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE TABLE plain (id INT)",
        "INSERT INTO plain (id) VALUES (1)",
        "INSERT INTO plain (id) VALUES (2)",
        "CREATE USER bob",
        "GRANT SELECT ON plain TO bob",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();
    assert_eq!(
        count_rows_as(&engine, Some("bob"), "SELECT * FROM plain LIMIT 10"),
        2
    );
}

/// Other QuerySpec-routing shapes with the same policy: ORDER BY and
/// GROUP BY also route through `LogicalPlan::Query` — the same injection
/// path — so they were equally broken. Cover them.
#[test]
fn rls_with_order_by_and_group_by() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    assert_eq!(
        count_rows_as(
            &engine,
            Some("alice"),
            "SELECT * FROM repro_test ORDER BY id LIMIT 10"
        ),
        2
    );
    assert_eq!(
        count_rows_as(
            &engine,
            Some("alice"),
            "SELECT owner, COUNT(*) FROM repro_test GROUP BY owner"
        ),
        1,
        "GROUP BY over RLS-filtered rows must see only alice's group"
    );
}

/// The per-user error text from the filing proved the substitution ran with
/// the right identity — verify two different users each see only their rows
/// through the LIMIT path (no cross-user plan-cache contamination).
#[test]
fn rls_limit_two_users_isolated() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    setup(&engine);
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE USER zeta",
        "GRANT SELECT ON repro_test TO zeta",
        "INSERT INTO repro_test (id, owner) VALUES (4, 'zeta')",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();

    let sql = "SELECT * FROM repro_test LIMIT 10";
    assert_eq!(count_rows_as(&engine, Some("alice"), sql), 2);
    assert_eq!(count_rows_as(&engine, Some("zeta"), sql), 1);
    assert_eq!(count_rows_as(&engine, Some("alice"), sql), 2);
}
