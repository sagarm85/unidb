// Item 149 — Row triggers v1 (BEFORE/AFTER INSERT/UPDATE/DELETE), engine-
// level acceptance tests. Locked spec: `docs/backlog/149_row_triggers.md`.
//
// Test matrix (mirrors the backlog doc's "Required tests" list):
//   1. create_drop_*         — CREATE/DROP round-trip + validation errors;
//                               DROP TABLE cleans up; function-in-use guard.
//   2. before_veto_*         — BEFORE trigger error vetoes the statement.
//   3. after_audit_*         — AFTER INSERT audit pattern, commit + abort.
//   4. stamp_pattern_*       — AFTER UPDATE self-UPDATE via NEW.pk terminates.
//   5. binding_*             — NEW/OLD binding for INSERT/UPDATE/DELETE, incl. NULLs.
//   6. name_order_*          — deterministic multi-trigger firing order.
//   7. privilege_*           — non-superuser caller still fires the trigger
//                               (embedded-identity body); non-superuser
//                               CREATE TRIGGER is denied.
//   8. (crash test lives in tests/crash/main.rs — p149a/p149b)
//   9. upsert_do_update_fires_update_trigger — spec test 9.
//  10. concurrency_smoke_*   — two writers, no hang, counts correct.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use unidb::authz::FunctionDef;
use unidb::error::DbError;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

// ── helpers (mirrors tests/item150_upsert.rs) ───────────────────────────────

fn fresh() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run one statement as the embedded superuser (no RLS/grants).
fn exec_super(engine: &Engine, sql: &str) -> Result<Vec<ExecResult>, DbError> {
    let xid = engine.begin().unwrap();
    let r = engine.execute_sql_as(None, xid, sql);
    match r {
        Ok(rows) => {
            engine.commit(xid).unwrap();
            Ok(rows)
        }
        Err(e) => {
            let _ = engine.abort(xid);
            Err(e)
        }
    }
}

/// Run one statement, aborting instead of committing — for the AFTER-audit
/// abort direction of test 3.
fn exec_super_then_abort(engine: &Engine, sql: &str) -> Result<Vec<ExecResult>, DbError> {
    let xid = engine.begin().unwrap();
    let r = engine.execute_sql_as(None, xid, sql);
    let _ = engine.abort(xid);
    r
}

/// Run one statement as a named (RLS/grant-enforced) user.
fn exec_as(engine: &Engine, user: &str, sql: &str) -> Result<Vec<ExecResult>, DbError> {
    let xid = engine.begin().unwrap();
    let r = engine.execute_sql_as(Some(user), xid, sql);
    match r {
        Ok(rows) => {
            engine.commit(xid).unwrap();
            Ok(rows)
        }
        Err(e) => {
            let _ = engine.abort(xid);
            Err(e)
        }
    }
}

fn only_result(r: Vec<ExecResult>) -> ExecResult {
    r.into_iter().next().expect("statement produced no result")
}

fn rows_of(result: &ExecResult) -> Vec<Vec<Literal>> {
    match result {
        ExecResult::Rows { rows, .. } => rows.clone(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn int_at(row: &[Literal], i: usize) -> i64 {
    match &row[i] {
        Literal::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

fn zero_param_fn(name: &str, body: &[&str]) -> FunctionDef {
    FunctionDef {
        name: name.to_string(),
        params: vec![],
        body: body.iter().map(|s| s.to_string()).collect(),
        run_as: None,
    }
}

/// Fail the test if `f` does not return within `secs` (mirrors
/// `tests/item150_upsert.rs::with_deadline`).
fn with_deadline<F: FnOnce() + Send + 'static>(secs: u64, f: F) {
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let d2 = Arc::clone(&done);
    let h = thread::spawn(move || {
        f();
        d2.store(1, std::sync::atomic::Ordering::SeqCst);
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if done.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            h.join().unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test did not finish within {secs}s — likely a deadlock/livelock hang");
}

// ── 1. CREATE/DROP round-trip + validation ──────────────────────────────────

#[test]
fn create_drop_trigger_round_trip() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    engine
        .upsert_function(zero_param_fn("fn1", &["SELECT 1"]))
        .unwrap();

    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig1 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION fn1",
    )
    .unwrap();
    assert!(matches!(only_result(r), ExecResult::CreatedTrigger));

    let r = exec_super(&engine, "DROP TRIGGER trig1 ON t").unwrap();
    assert!(matches!(only_result(r), ExecResult::DroppedTrigger));

    // Dropped: firing it again (re-create then re-drop) must work — proves
    // the name was actually freed.
    exec_super(
        &engine,
        "CREATE TRIGGER trig1 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION fn1",
    )
    .unwrap();
}

#[test]
fn create_trigger_unknown_function_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION nope",
    );
    assert!(
        matches!(r, Err(DbError::InvalidTriggerDef(_))),
        "expected InvalidTriggerDef, got {r:?}"
    );
}

#[test]
fn create_trigger_function_with_params_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(FunctionDef {
            name: "with_params".to_string(),
            params: vec!["x".to_string()],
            body: vec!["SELECT $1".to_string()],
            run_as: None,
        })
        .unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION with_params",
    );
    assert!(
        matches!(r, Err(DbError::InvalidTriggerDef(_))),
        "expected InvalidTriggerDef (nonzero params), got {r:?}"
    );
}

#[test]
fn create_trigger_bad_column_ref_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn(
            "bad_col_fn",
            &["INSERT INTO t (id) VALUES (NEW.nonexistent)"],
        ))
        .unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION bad_col_fn",
    );
    assert!(
        matches!(r, Err(DbError::InvalidTriggerDef(_))),
        "expected InvalidTriggerDef (unknown column), got {r:?}"
    );
}

#[test]
fn create_trigger_old_on_insert_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn(
            "old_on_insert_fn",
            &["INSERT INTO t (id) VALUES (OLD.id)"],
        ))
        .unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION old_on_insert_fn",
    );
    assert!(
        matches!(r, Err(DbError::InvalidTriggerDef(_))),
        "expected InvalidTriggerDef (OLD unavailable on INSERT), got {r:?}"
    );
}

#[test]
fn create_trigger_new_on_delete_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn(
            "new_on_delete_fn",
            &["INSERT INTO t (id) VALUES (NEW.id)"],
        ))
        .unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER trig AFTER DELETE ON t FOR EACH ROW EXECUTE FUNCTION new_on_delete_fn",
    );
    assert!(
        matches!(r, Err(DbError::InvalidTriggerDef(_))),
        "expected InvalidTriggerDef (NEW unavailable on DELETE), got {r:?}"
    );
}

#[test]
fn create_trigger_duplicate_name_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn("dupfn", &["SELECT 1"]))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER dup_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION dupfn",
    )
    .unwrap();
    let r = exec_super(
        &engine,
        "CREATE TRIGGER dup_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION dupfn",
    );
    assert!(
        matches!(r, Err(DbError::TriggerAlreadyExists(_))),
        "expected TriggerAlreadyExists, got {r:?}"
    );
}

#[test]
fn drop_table_cleans_up_its_triggers() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn("cleanup_fn", &["SELECT 1"]))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER cleanup_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION cleanup_fn",
    )
    .unwrap();

    exec_super(&engine, "DROP TABLE t").unwrap();
    // Recreate a table under the SAME name — the old trigger must NOT
    // reappear (it was purged by DROP TABLE, not merely orphaned).
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    let r = exec_super(&engine, "DROP TRIGGER cleanup_trig ON t");
    assert!(
        matches!(r, Err(DbError::UnknownTrigger { .. })),
        "DROP TABLE must have purged the trigger; got {r:?}"
    );
}

#[test]
fn drop_function_in_use_by_trigger_is_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    engine
        .upsert_function(zero_param_fn("in_use_fn", &["SELECT 1"]))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER use_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION in_use_fn",
    )
    .unwrap();

    let r = engine.remove_function("in_use_fn");
    assert!(
        matches!(r, Err(DbError::FunctionInUseByTrigger { .. })),
        "expected FunctionInUseByTrigger, got {r:?}"
    );

    // Once the trigger is dropped, removal succeeds.
    exec_super(&engine, "DROP TRIGGER use_trig ON t").unwrap();
    engine.remove_function("in_use_fn").unwrap();
}

// ── 2. BEFORE veto ───────────────────────────────────────────────────────────

#[test]
fn before_trigger_error_vetoes_the_statement_no_side_effects() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    exec_super(&engine, "CREATE TABLE guard (id INT PRIMARY KEY)").unwrap();
    // Pre-seed a conflicting row so the trigger's own INSERT always fails.
    exec_super(&engine, "INSERT INTO guard VALUES (1)").unwrap();

    engine
        .upsert_function(zero_param_fn(
            "veto_fn",
            &["INSERT INTO guard (id) VALUES (NEW.id)"],
        ))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER veto_trig BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION veto_fn",
    )
    .unwrap();

    let r = exec_super(&engine, "INSERT INTO t VALUES (1, 'x')");
    assert!(
        matches!(r, Err(DbError::UniqueViolation { .. })),
        "expected the trigger body's UniqueViolation to propagate, got {r:?}"
    );

    // Neither the triggering row nor a second guard row exist.
    let t_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM t").unwrap(),
    ));
    assert!(t_rows.is_empty(), "no row must persist after BEFORE veto");
    let guard_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM guard").unwrap(),
    ));
    assert_eq!(
        guard_rows.len(),
        1,
        "the trigger's own attempted write must not persist either"
    );
}

// ── 3. AFTER audit pattern ──────────────────────────────────────────────────

fn setup_audit_pattern(engine: &Engine) {
    exec_super(engine, "CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    exec_super(engine, "CREATE TABLE audit (id INT, note TEXT)").unwrap();
    engine
        .upsert_function(zero_param_fn(
            "audit_fn",
            &["INSERT INTO audit (id, note) VALUES (NEW.id, 'inserted')"],
        ))
        .unwrap();
    exec_super(
        engine,
        "CREATE TRIGGER audit_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION audit_fn",
    )
    .unwrap();
}

#[test]
fn after_insert_audit_row_visible_after_commit() {
    let (engine, _dir) = fresh();
    setup_audit_pattern(&engine);

    exec_super(&engine, "INSERT INTO t VALUES (1, 'hello')").unwrap();

    let t_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM t WHERE id = 1").unwrap(),
    ));
    assert_eq!(t_rows.len(), 1);
    let audit_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM audit WHERE id = 1").unwrap(),
    ));
    assert_eq!(
        audit_rows.len(),
        1,
        "AFTER INSERT trigger must have written the audit row, same transaction"
    );
}

#[test]
fn after_insert_audit_row_invisible_after_abort() {
    let (engine, _dir) = fresh();
    setup_audit_pattern(&engine);

    let r = exec_super_then_abort(&engine, "INSERT INTO t VALUES (2, 'bye')");
    assert!(r.is_ok(), "the INSERT itself succeeds pre-abort: {r:?}");

    let t_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM t WHERE id = 2").unwrap(),
    ));
    assert!(
        t_rows.is_empty(),
        "triggering row must not survive an abort"
    );
    let audit_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM audit WHERE id = 2").unwrap(),
    ));
    assert!(
        audit_rows.is_empty(),
        "the AFTER trigger's audit row must not survive an abort either"
    );
}

// ── 4. Stamp pattern (AFTER UPDATE self-update, no recursion) ──────────────

#[test]
fn after_update_self_update_stamp_terminates_no_recursion() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, body TEXT, touch_count INT)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 'orig', 0)").unwrap();

    engine
        .upsert_function(zero_param_fn(
            "stamp_fn",
            &["UPDATE t SET touch_count = touch_count + 1 WHERE id = NEW.id"],
        ))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER stamp_trig AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION stamp_fn",
    )
    .unwrap();

    exec_super(&engine, "UPDATE t SET body = 'changed' WHERE id = 1").unwrap();

    let rows = rows_of(&only_result(
        exec_super(&engine, "SELECT touch_count FROM t WHERE id = 1").unwrap(),
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        int_at(&rows[0], 0),
        1,
        "the self-UPDATE must land exactly once — recursion would make this diverge \
         (or a non-cascading bug would make it stay 0)"
    );
}

// ── 5. NEW/OLD binding correctness (values + NULLs), all three events ──────

fn setup_binding_log(engine: &Engine) {
    exec_super(engine, "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT)").unwrap();
    exec_super(
        engine,
        "CREATE TABLE log (event TEXT, old_tag TEXT, new_tag TEXT)",
    )
    .unwrap();
    engine
        .upsert_function(zero_param_fn(
            "log_insert_fn",
            &["INSERT INTO log (event, old_tag, new_tag) VALUES ('insert', NULL, NEW.tag)"],
        ))
        .unwrap();
    engine
        .upsert_function(zero_param_fn(
            "log_update_fn",
            &["INSERT INTO log (event, old_tag, new_tag) VALUES ('update', OLD.tag, NEW.tag)"],
        ))
        .unwrap();
    engine
        .upsert_function(zero_param_fn(
            "log_delete_fn",
            &["INSERT INTO log (event, old_tag, new_tag) VALUES ('delete', OLD.tag, NULL)"],
        ))
        .unwrap();
    exec_super(
        engine,
        "CREATE TRIGGER log_ins AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_insert_fn",
    )
    .unwrap();
    exec_super(
        engine,
        "CREATE TRIGGER log_upd AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION log_update_fn",
    )
    .unwrap();
    exec_super(
        engine,
        "CREATE TRIGGER log_del AFTER DELETE ON t FOR EACH ROW EXECUTE FUNCTION log_delete_fn",
    )
    .unwrap();
}

fn text_or_null(row: &[Literal], i: usize) -> Option<String> {
    match &row[i] {
        Literal::Text(s) => Some(s.clone()),
        Literal::Null => None,
        other => panic!("expected Text or Null, got {other:?}"),
    }
}

#[test]
fn new_old_binding_values_across_insert_update_delete() {
    let (engine, _dir) = fresh();
    setup_binding_log(&engine);

    exec_super(&engine, "INSERT INTO t VALUES (1, 'a')").unwrap();
    exec_super(&engine, "UPDATE t SET tag = 'b' WHERE id = 1").unwrap();
    exec_super(&engine, "DELETE FROM t WHERE id = 1").unwrap();

    let rows = rows_of(&only_result(
        exec_super(&engine, "SELECT event, old_tag, new_tag FROM log").unwrap(),
    ));
    assert_eq!(rows.len(), 3, "one log row per event: {rows:?}");

    let by_event = |ev: &str| -> Vec<Literal> {
        rows.iter()
            .find(|r| matches!(&r[0], Literal::Text(s) if s == ev))
            .unwrap_or_else(|| panic!("no log row for event '{ev}': {rows:?}"))
            .clone()
    };

    let ins = by_event("insert");
    assert_eq!(text_or_null(&ins, 1), None, "INSERT has no OLD image");
    assert_eq!(text_or_null(&ins, 2), Some("a".to_string()));

    let upd = by_event("update");
    assert_eq!(text_or_null(&upd, 1), Some("a".to_string()));
    assert_eq!(text_or_null(&upd, 2), Some("b".to_string()));

    let del = by_event("delete");
    assert_eq!(text_or_null(&del, 1), Some("b".to_string()));
    assert_eq!(text_or_null(&del, 2), None, "DELETE has no NEW image");
}

#[test]
fn new_old_binding_propagates_actual_null_column_values() {
    let (engine, _dir) = fresh();
    setup_binding_log(&engine);

    // The inserted row's `tag` is itself NULL — NEW.tag must bind to NULL,
    // not error or silently coerce to empty text.
    exec_super(&engine, "INSERT INTO t (id, tag) VALUES (2, NULL)").unwrap();

    let rows = rows_of(&only_result(
        exec_super(&engine, "SELECT new_tag FROM log WHERE event = 'insert'").unwrap(),
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        text_or_null(&rows[0], 0),
        None,
        "NEW.tag must bind to NULL when the inserted column value is NULL"
    );
}

// ── 6. Name-order firing determinism ────────────────────────────────────────

#[test]
fn multiple_triggers_fire_in_name_order() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    // `seq_id` is a SERIAL column: the Nth row inserted (in wall-clock
    // insertion order, regardless of which trigger inserted it) gets the
    // Nth counter value — a purely VALUES-based (no INSERT...SELECT, which
    // this engine's v1 INSERT grammar doesn't support) way to observe
    // firing order.
    exec_super(
        &engine,
        "CREATE TABLE order_log (seq_id SERIAL PRIMARY KEY, tag TEXT)",
    )
    .unwrap();

    engine
        .upsert_function(zero_param_fn(
            "a_trig_fn",
            &["INSERT INTO order_log (tag) VALUES ('a')"],
        ))
        .unwrap();
    engine
        .upsert_function(zero_param_fn(
            "b_trig_fn",
            &["INSERT INTO order_log (tag) VALUES ('b')"],
        ))
        .unwrap();
    // Deliberately register "b_trig" before "a_trig" so a bug that fired in
    // registration order (instead of name order) would still be caught.
    exec_super(
        &engine,
        "CREATE TRIGGER b_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION b_trig_fn",
    )
    .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER a_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION a_trig_fn",
    )
    .unwrap();

    exec_super(&engine, "INSERT INTO t VALUES (1)").unwrap();

    let rows = rows_of(&only_result(
        exec_super(&engine, "SELECT tag FROM order_log ORDER BY seq_id").unwrap(),
    ));
    assert_eq!(rows.len(), 2);
    match (&rows[0][0], &rows[1][0]) {
        (Literal::Text(first), Literal::Text(second)) => {
            assert_eq!(first, "a", "a_trig (alphabetically first) must fire first");
            assert_eq!(second, "b", "b_trig must fire second");
        }
        other => panic!("expected two Text rows, got {other:?}"),
    }
}

// ── 7. Privilege model ──────────────────────────────────────────────────────

#[test]
fn non_superuser_statement_fires_trigger_against_ungranted_table() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    exec_super(&engine, "CREATE TABLE audit (id INT)").unwrap();
    exec_super(&engine, "CREATE USER alice").unwrap();
    // alice can INSERT into t, but has NO grant at all on `audit` — the
    // trigger body must still succeed (embedded/unrestricted identity).
    exec_super(&engine, "GRANT INSERT, SELECT ON t TO alice").unwrap();

    engine
        .upsert_function(zero_param_fn(
            "priv_audit_fn",
            &["INSERT INTO audit (id) VALUES (NEW.id)"],
        ))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER priv_trig AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION priv_audit_fn",
    )
    .unwrap();

    exec_as(&engine, "alice", "INSERT INTO t VALUES (1, 'x')")
        .expect("alice's statement (which she IS granted) must succeed");

    let audit_rows = rows_of(&only_result(
        exec_super(&engine, "SELECT id FROM audit").unwrap(),
    ));
    assert_eq!(
        audit_rows.len(),
        1,
        "the trigger body must have written to `audit` despite alice having no grant there"
    );
}

#[test]
fn non_superuser_cannot_create_trigger() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    exec_super(&engine, "CREATE USER alice").unwrap();
    exec_super(&engine, "GRANT SELECT, INSERT ON t TO alice").unwrap();
    engine
        .upsert_function(zero_param_fn("noop_fn", &["SELECT 1"]))
        .unwrap();

    let r = exec_as(
        &engine,
        "alice",
        "CREATE TRIGGER not_allowed AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION noop_fn",
    );
    assert!(
        matches!(r, Err(DbError::PermissionDenied(_))),
        "non-superuser CREATE TRIGGER must be denied, got {r:?}"
    );
}

// ── 9. Upsert DO UPDATE fires UPDATE triggers ───────────────────────────────

#[test]
fn upsert_do_update_arm_fires_update_trigger() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, val INT, touch_count INT)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 1, 0)").unwrap();

    engine
        .upsert_function(zero_param_fn(
            "upsert_stamp_fn",
            &["UPDATE t SET touch_count = touch_count + 1 WHERE id = NEW.id"],
        ))
        .unwrap();
    exec_super(
        &engine,
        "CREATE TRIGGER upsert_stamp_trig AFTER UPDATE ON t FOR EACH ROW \
         EXECUTE FUNCTION upsert_stamp_fn",
    )
    .unwrap();

    exec_super(
        &engine,
        "INSERT INTO t (id, val, touch_count) VALUES (1, 99, 0) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap();

    let rows = rows_of(&only_result(
        exec_super(&engine, "SELECT val, touch_count FROM t WHERE id = 1").unwrap(),
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        int_at(&rows[0], 0),
        99,
        "DO UPDATE must have applied the SET"
    );
    assert_eq!(
        int_at(&rows[0], 1),
        1,
        "the DO UPDATE arm must route through apply_single_row_update and fire the \
         UPDATE trigger exactly like a plain UPDATE would"
    );
}

// ── 10. Concurrency smoke ────────────────────────────────────────────────────

#[test]
fn concurrency_smoke_two_writers_on_triggered_table_no_hang_correct_counts() {
    with_deadline(30, || {
        let dir = tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
        exec_super(&engine, "CREATE TABLE audit (id INT)").unwrap();
        engine
            .upsert_function(zero_param_fn(
                "concur_audit_fn",
                &["INSERT INTO audit (id) VALUES (NEW.id)"],
            ))
            .unwrap();
        exec_super(
            &engine,
            "CREATE TRIGGER concur_trig AFTER INSERT ON t FOR EACH ROW \
             EXECUTE FUNCTION concur_audit_fn",
        )
        .unwrap();

        let threads = 2usize;
        let per_thread = 25i64;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for t_idx in 0..threads {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..per_thread {
                    // Disjoint id ranges per thread — no conflicts expected.
                    let id = (t_idx as i64) * 1_000_000 + i;
                    let sql = format!("INSERT INTO t VALUES ({id}, {i})");
                    for attempt in 0..20 {
                        let xid = engine.begin().unwrap();
                        match engine.execute_sql(xid, &sql) {
                            Ok(_) => {
                                engine.commit(xid).unwrap();
                                break;
                            }
                            Err(_) => {
                                let _ = engine.abort(xid);
                                if attempt == 19 {
                                    panic!("thread {t_idx} row {i} never converged");
                                }
                                thread::sleep(Duration::from_millis(5));
                            }
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let expected = threads as i64 * per_thread;
        let t_rows = rows_of(&only_result(
            exec_super(&engine, "SELECT id FROM t").unwrap(),
        ));
        assert_eq!(t_rows.len() as i64, expected, "row count in t must match");
        let audit_rows = rows_of(&only_result(
            exec_super(&engine, "SELECT id FROM audit").unwrap(),
        ));
        assert_eq!(
            audit_rows.len() as i64,
            expected,
            "every triggering row must have exactly one audit row"
        );
    });
}
