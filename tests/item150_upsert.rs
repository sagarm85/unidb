// Item 150 — `INSERT ... ON CONFLICT` (upsert), engine-level acceptance
// tests. Locked spec: `docs/backlog/150_upsert_on_conflict.md`. The DO
// UPDATE arm must route through the EXISTING update machinery
// (`apply_single_row_update`, shared verbatim with `exec_update`'s own
// per-row loop) — these tests prove the *observable* behavior that design
// implies (index maintenance, FK checks, locks, RLS), not the internal
// call graph.
//
// Test matrix (spec's "Required tests" list):
//   1. do_nothing_*            — DO NOTHING semantics, with/without target
//   2. do_update_*             — EXCLUDED binding, WHERE guard, PK + UNIQUE
//   3. returning_*             — RETURNING on both arms
//   4. null_never_conflicts_*  — NULL parity with plain INSERT
//   5. rls_*                   — INSERT WITH CHECK parity, UPDATE USING
//                                 (error not skip), post-image WITH CHECK,
//                                 column-grant denial
//   6. fk_*                    — FK validated on both arms
//   7. concurrent_*            — two threads, same new key, no dup/no hang
//   8. index_maintenance_*     — DO UPDATE maintains a secondary B-tree index
//
// (Crash test #9 lives in `tests/crash/main.rs`; REST test #10 lives in
// `tests/item150_upsert_rest.rs`.)
//
// `hot_chain_multi_hop_*` below is a bonus regression test for a
// **pre-existing** latent bug this item's own concurrency test (#7)
// uncovered empirically, not something item 150 introduced: `heap::
// get_visible_cached`/`get_visible_with_rid` followed at most ONE HOT-chain
// hop, on the documented (but false) assumption that a HOT chain is always
// length 1. `enforce_unique`'s fast path and item 150's own conflict probe
// both resolve a unique-index candidate through this same single-hop
// function; after ≥2 sequential HOT updates on the same PK/UNIQUE-indexed
// row (the implicit unique index is deliberately never repatched on the HOT
// path), the *third* resolution attempt under-resolved to "no visible
// version" — silently letting a duplicate-key `INSERT` slip past
// `enforce_unique` and create a second live row with the same key. Fixed by
// looping the chain walk (bounded, `MAX_HOT_CHAIN_HOPS`) instead of
// following exactly one hop — a pure read-path correctness fix, no WAL/page
// format change (D5 untouched).

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use unidb::error::DbError;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

// ── helpers ──────────────────────────────────────────────────────────────────

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

fn inserted_count(result: &ExecResult) -> usize {
    match result {
        ExecResult::Inserted { count } => *count,
        other => panic!("expected Inserted, got {other:?}"),
    }
}

fn int_at(row: &[Literal], i: usize) -> i64 {
    match &row[i] {
        Literal::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

fn text_at(row: &[Literal], i: usize) -> String {
    match &row[i] {
        Literal::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Fail the test if `f` does not return within `secs` — turns a hang into a
/// clean assertion failure instead of a suite that blocks forever (mirrors
/// `tests/concurrent_writers.rs`'s `with_deadline`).
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

// ── 1. DO NOTHING ────────────────────────────────────────────────────────────

#[test]
fn do_nothing_with_explicit_target_skips_duplicate_inserts_new_correct_count() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 'orig')").unwrap();

    let r = exec_super(
        &engine,
        "INSERT INTO t (id, body) VALUES (1, 'dup'), (2, 'new') \
         ON CONFLICT (id) DO NOTHING",
    )
    .unwrap();
    assert_eq!(
        inserted_count(&only_result(r)),
        1,
        "only the non-duplicate row (id=2) should be counted"
    );

    let sel = exec_super(&engine, "SELECT id, body FROM t").unwrap();
    let mut rows = rows_of(&only_result(sel));
    rows.sort_by_key(|r| int_at(r, 0));
    assert_eq!(rows.len(), 2);
    assert_eq!(
        text_at(&rows[0], 1),
        "orig",
        "duplicate row must NOT be overwritten"
    );
    assert_eq!(text_at(&rows[1], 1), "new");
}

#[test]
fn do_nothing_without_target_absorbs_any_unique_violation() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 'a@x.com')").unwrap();

    // Conflicts on `email` (a UNIQUE column), NOT the (absent) target — still
    // absorbed: "optional for DO NOTHING (then any unique violation is
    // ignored)".
    let r = exec_super(
        &engine,
        "INSERT INTO t (id, email) VALUES (2, 'a@x.com') ON CONFLICT DO NOTHING",
    )
    .unwrap();
    assert_eq!(inserted_count(&only_result(r)), 0);

    let sel = exec_super(&engine, "SELECT id FROM t").unwrap();
    assert_eq!(rows_of(&only_result(sel)).len(), 1, "no row was inserted");

    // A genuinely new row still inserts normally through the same statement shape.
    let r2 = exec_super(
        &engine,
        "INSERT INTO t (id, email) VALUES (3, 'b@x.com') ON CONFLICT DO NOTHING",
    )
    .unwrap();
    assert_eq!(inserted_count(&only_result(r2)), 1);
}

#[test]
fn do_nothing_target_only_absorbs_conflicts_on_the_named_target() {
    // A conflict on a UNIQUE column OTHER than the named target must still
    // raise UniqueViolation normally — ON CONFLICT (col) only silences
    // conflicts on `col`, matching Postgres (naming one constraint does not
    // silence others).
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 'a@x.com')").unwrap();

    let err = exec_super(
        &engine,
        "INSERT INTO t (id, email) VALUES (2, 'a@x.com') ON CONFLICT (id) DO NOTHING",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation (conflict on `email`, not the named target `id`); got {err:?}"
    );
}

// ── 2. DO UPDATE ─────────────────────────────────────────────────────────────

#[test]
fn do_update_excluded_values_land_on_pk_target() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, hits INT)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO users VALUES (1, 'Alice', 10)").unwrap();

    exec_super(
        &engine,
        "INSERT INTO users (id, name, hits) VALUES (1, 'Alice2', 999) \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, hits = hits + 1",
    )
    .unwrap();

    let sel = exec_super(&engine, "SELECT name, hits FROM users WHERE id = 1").unwrap();
    let rows = rows_of(&only_result(sel));
    assert_eq!(rows.len(), 1);
    assert_eq!(text_at(&rows[0], 0), "Alice2", "EXCLUDED.name must land");
    assert_eq!(
        int_at(&rows[0], 1),
        11,
        "hits = hits + 1 must read the EXISTING row (11), not EXCLUDED's 999"
    );
}

#[test]
fn do_update_excluded_values_land_on_secondary_unique_target() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO users VALUES (1, 'a@x.com', 'Alice')").unwrap();

    // Conflict target is the SECONDARY unique column (email), with a
    // DIFFERENT proposed id — must resolve to the existing row (matched on
    // email), not insert a second row nor violate the PK.
    exec_super(
        &engine,
        "INSERT INTO users (id, email, name) VALUES (999, 'a@x.com', 'Alice-Updated') \
         ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name",
    )
    .unwrap();

    let sel = exec_super(&engine, "SELECT id, name FROM users").unwrap();
    let rows = rows_of(&only_result(sel));
    assert_eq!(rows.len(), 1, "must still be exactly one row");
    assert_eq!(
        int_at(&rows[0], 0),
        1,
        "PK must be unchanged (999 never inserted)"
    );
    assert_eq!(text_at(&rows[0], 1), "Alice-Updated");
}

#[test]
fn do_update_where_guard_false_skips_row_true_applies() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, hits INT)",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO users VALUES (1, 'Alice', 10)").unwrap();

    // WHERE false -> row skipped, not an error, not counted.
    let r = exec_super(
        &engine,
        "INSERT INTO users (id, name, hits) VALUES (1, 'ShouldNotApply', 1) \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name WHERE hits > 100000",
    )
    .unwrap();
    assert_eq!(inserted_count(&only_result(r)), 0);
    let sel = exec_super(&engine, "SELECT name FROM users WHERE id = 1").unwrap();
    assert_eq!(text_at(&rows_of(&only_result(sel))[0], 0), "Alice");

    // WHERE true -> row applies normally.
    let r2 = exec_super(
        &engine,
        "INSERT INTO users (id, name, hits) VALUES (1, 'ShouldApply', 1) \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name WHERE hits >= 10",
    )
    .unwrap();
    assert_eq!(inserted_count(&only_result(r2)), 1);
    let sel2 = exec_super(&engine, "SELECT name FROM users WHERE id = 1").unwrap();
    assert_eq!(text_at(&rows_of(&only_result(sel2))[0], 0), "ShouldApply");
}

// ── 3. RETURNING on both arms ───────────────────────────────────────────────

#[test]
fn returning_works_on_insert_arm_and_do_update_arm() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 100)").unwrap();

    // One statement, one row inserts (id=2, no conflict), one row updates
    // (id=1, conflict) — RETURNING must reflect both, in statement order.
    let r = exec_super(
        &engine,
        "INSERT INTO t (id, val) VALUES (1, 999), (2, 5) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val \
         RETURNING id, val",
    )
    .unwrap();
    let mut rows = rows_of(&only_result(r));
    rows.sort_by_key(|r| int_at(r, 0));
    assert_eq!(
        rows.len(),
        2,
        "RETURNING must include both the updated and inserted row"
    );
    assert_eq!((int_at(&rows[0], 0), int_at(&rows[0], 1)), (1, 999));
    assert_eq!((int_at(&rows[1], 0), int_at(&rows[1], 1)), (2, 5));
}

// ── 4. NULL never conflicts ─────────────────────────────────────────────────

#[test]
fn null_never_conflicts_parity_with_plain_insert() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT, email TEXT UNIQUE)").unwrap();

    // Plain INSERT: two NULL emails do not conflict (today's behavior).
    exec_super(&engine, "INSERT INTO t (id, email) VALUES (1, NULL)").unwrap();
    exec_super(&engine, "INSERT INTO t (id, email) VALUES (2, NULL)").unwrap();

    // Upsert path: a NULL proposed value on the conflict target must behave
    // identically — never treated as a conflict, always a plain insert.
    let r = exec_super(
        &engine,
        "INSERT INTO t (id, email) VALUES (3, NULL) \
         ON CONFLICT (email) DO UPDATE SET id = EXCLUDED.id",
    )
    .unwrap();
    assert_eq!(inserted_count(&only_result(r)), 1);

    let sel = exec_super(&engine, "SELECT id FROM t").unwrap();
    assert_eq!(
        rows_of(&only_result(sel)).len(),
        3,
        "all three NULL-email rows must coexist (NULL never conflicts)"
    );
}

// ── 5. RLS ───────────────────────────────────────────────────────────────────

fn setup_rls_docs(engine: &Engine) {
    exec_super(
        engine,
        "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, val INT)",
    )
    .unwrap();
    exec_super(engine, "CREATE USER alice").unwrap();
    exec_super(engine, "CREATE USER bob").unwrap();
    exec_super(engine, "GRANT SELECT, INSERT, UPDATE ON docs TO alice").unwrap();
    exec_super(engine, "GRANT SELECT, INSERT, UPDATE ON docs TO bob").unwrap();
    exec_super(
        engine,
        "CREATE POLICY ins_own ON docs FOR INSERT USING (owner = current_user)",
    )
    .unwrap();
    exec_super(
        engine,
        "CREATE POLICY upd_own ON docs FOR UPDATE USING (owner = current_user) \
         WITH CHECK (val >= 0)",
    )
    .unwrap();
}

#[test]
fn rls_insert_arm_with_check_parity_with_plain_insert() {
    let (engine, _dir) = fresh();
    setup_rls_docs(&engine);

    // Plain INSERT violating the INSERT policy is rejected.
    let plain_err = exec_as(&engine, "alice", "INSERT INTO docs VALUES (1, 'bob', 1)").unwrap_err();

    // The SAME violation, reached through the (no-conflict) insert arm of an
    // ON CONFLICT statement, must be rejected identically — no bypass.
    let upsert_err = exec_as(
        &engine,
        "alice",
        "INSERT INTO docs VALUES (2, 'bob', 1) ON CONFLICT (id) DO NOTHING",
    )
    .unwrap_err();

    assert!(matches!(plain_err, DbError::SqlPlan(_)));
    assert!(matches!(upsert_err, DbError::SqlPlan(_)));
}

#[test]
fn rls_update_arm_using_mismatch_is_an_error_not_a_silent_skip() {
    let (engine, _dir) = fresh();
    setup_rls_docs(&engine);
    // bob owns row id=1.
    exec_as(&engine, "bob", "INSERT INTO docs VALUES (1, 'bob', 10)").unwrap();

    // alice targets the SAME id — her insert's own proposed row (owner=
    // 'alice') would pass the INSERT policy, but the row already exists
    // (owned by bob), so this routes into the UPDATE arm, whose USING
    // (owner = current_user) does NOT match the EXISTING row (owner=bob).
    // Spec: this must be a hard ERROR, not a silent skip (silently skipping
    // would leak the row's existence to a caller who can't otherwise see it).
    let err = exec_as(
        &engine,
        "alice",
        "INSERT INTO docs (id, owner, val) VALUES (1, 'alice', 5) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::SqlPlan(_)),
        "expected a hard error on USING mismatch; got {err:?}"
    );

    // bob's own row must be untouched.
    let sel = exec_as(&engine, "bob", "SELECT val FROM docs WHERE id = 1").unwrap();
    assert_eq!(int_at(&rows_of(&only_result(sel))[0], 0), 10);
}

#[test]
fn rls_update_arm_post_image_with_check_enforced() {
    let (engine, _dir) = fresh();
    setup_rls_docs(&engine);
    exec_as(&engine, "bob", "INSERT INTO docs VALUES (1, 'bob', 10)").unwrap();

    // bob's own row (USING passes), but the post-image violates WITH CHECK
    // (val >= 0) — must be rejected.
    let err = exec_as(
        &engine,
        "bob",
        "INSERT INTO docs (id, owner, val) VALUES (1, 'bob', -1) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::SqlPlan(_)));

    let sel = exec_as(&engine, "bob", "SELECT val FROM docs WHERE id = 1").unwrap();
    assert_eq!(
        int_at(&rows_of(&only_result(sel))[0], 0),
        10,
        "the rejected write must not have applied"
    );
}

#[test]
fn rls_update_arm_column_grant_denial_on_set_column() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, val INT)",
    )
    .unwrap();
    exec_super(&engine, "CREATE USER carol").unwrap();
    exec_super(&engine, "GRANT SELECT, INSERT ON docs TO carol").unwrap();
    // Column-scoped UPDATE grant on `owner` only — NOT `val`.
    exec_super(&engine, "GRANT UPDATE (owner) ON docs TO carol").unwrap();
    exec_super(&engine, "INSERT INTO docs VALUES (1, 'carol', 10)").unwrap();

    let err = exec_as(
        &engine,
        "carol",
        "INSERT INTO docs (id, owner, val) VALUES (1, 'carol', 20) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::PermissionDenied(_)),
        "SET val without an UPDATE grant on `val` must be denied; got {err:?}"
    );

    // The same SET column she DOES hold a grant on succeeds.
    exec_as(
        &engine,
        "carol",
        "INSERT INTO docs (id, owner, val) VALUES (1, 'carol', 20) \
         ON CONFLICT (id) DO UPDATE SET owner = EXCLUDED.owner",
    )
    .unwrap();
}

// ── 6. FK interplay ──────────────────────────────────────────────────────────

#[test]
fn fk_validated_on_insert_arm_and_update_arm() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE parents (id INT PRIMARY KEY)").unwrap();
    exec_super(
        &engine,
        "CREATE TABLE children (id INT PRIMARY KEY, parent_id INT REFERENCES parents(id))",
    )
    .unwrap();
    exec_super(&engine, "INSERT INTO parents VALUES (1)").unwrap();

    // Insert arm: valid parent succeeds.
    exec_super(
        &engine,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) \
         ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id",
    )
    .unwrap();

    // Insert arm: invalid parent (no conflict — id=2 is new) must still be
    // FK-checked.
    let err = exec_super(
        &engine,
        "INSERT INTO children (id, parent_id) VALUES (2, 999) \
         ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::ForeignKeyViolation { .. }));

    // Update arm: conflict on id=1, SET parent_id to an invalid parent must
    // be FK-checked too (parent_id is in SET, so has_fk_refs_in_set gates it
    // on).
    let err2 = exec_super(
        &engine,
        "INSERT INTO children (id, parent_id) VALUES (1, 999) \
         ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id",
    )
    .unwrap_err();
    assert!(matches!(err2, DbError::ForeignKeyViolation { .. }));

    // The row must be unaffected by the rejected update-arm attempt.
    let sel = exec_super(&engine, "SELECT parent_id FROM children WHERE id = 1").unwrap();
    assert_eq!(int_at(&rows_of(&only_result(sel))[0], 0), 1);
}

// ── 7. Concurrency ───────────────────────────────────────────────────────────

#[test]
fn concurrent_upsert_same_new_key_exactly_one_row_no_duplicate_no_hang() {
    with_deadline(30, || {
        let dir = tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();

        let threads = 4;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for i in 0..threads {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait(); // maximize overlap on the SAME key
                let sql = format!(
                    "INSERT INTO t (id, val) VALUES (1, {i}) \
                     ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val"
                );
                // The phantom-lock-before-snapshot design (item 35, reused
                // verbatim by the conflict probe) serializes same-key
                // upserts without a write-write conflict on this row, but a
                // small bounded retry keeps the test robust to any residual
                // SI abort rather than asserting zero retries are needed.
                for attempt in 0..20 {
                    let xid = engine.begin().unwrap();
                    match engine.execute_sql(xid, &sql) {
                        Ok(_) => {
                            engine.commit(xid).unwrap();
                            return;
                        }
                        Err(_) => {
                            let _ = engine.abort(xid);
                            if attempt == 19 {
                                panic!("thread {i} could not converge after 20 attempts");
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let xid = engine.begin().unwrap();
        let r = engine.execute_sql(xid, "SELECT id FROM t").unwrap();
        engine.commit(xid).unwrap();
        let rows = rows_of(&only_result(r));
        assert_eq!(
            rows.len(),
            1,
            "exactly one live row must exist after all threads converge — no duplicate"
        );
        assert_eq!(int_at(&rows[0], 0), 1);
    });
}

// ── 8. Index maintenance ────────────────────────────────────────────────────

#[test]
fn do_update_maintains_secondary_btree_index() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT)").unwrap();
    exec_super(&engine, "CREATE INDEX idx_tag ON t USING BTREE (tag)").unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 'old')").unwrap();

    exec_super(
        &engine,
        "INSERT INTO t (id, tag) VALUES (1, 'new') \
         ON CONFLICT (id) DO UPDATE SET tag = EXCLUDED.tag",
    )
    .unwrap();

    // A WHERE on the indexed column must find the row under its NEW value
    // (proves the B-tree entry was updated, not left stale) and must NOT
    // find it under the old value.
    let sel_new = exec_super(&engine, "SELECT id FROM t WHERE tag = 'new'").unwrap();
    assert_eq!(rows_of(&only_result(sel_new)).len(), 1);
    let sel_old = exec_super(&engine, "SELECT id FROM t WHERE tag = 'old'").unwrap();
    assert_eq!(
        rows_of(&only_result(sel_old)).len(),
        0,
        "stale B-tree entry under the old tag value must not resolve to a live row"
    );
}

// ── bonus: multi-hop HOT chain resolution regression (see file header) ─────

#[test]
fn hot_chain_multi_hop_resolution_rejects_duplicate_key_after_several_hot_updates() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    exec_super(&engine, "INSERT INTO t VALUES (1, 0)").unwrap();

    // Several sequential HOT-eligible updates (each its own committed
    // statement) extend the row's HOT chain past the old single-hop bound.
    for i in 1..6 {
        exec_super(&engine, &format!("UPDATE t SET val = {i} WHERE id = 1")).unwrap();
    }
    let sel = exec_super(&engine, "SELECT val FROM t WHERE id = 1").unwrap();
    assert_eq!(int_at(&rows_of(&only_result(sel))[0], 0), 5);

    // A duplicate-key INSERT must still be rejected — `enforce_unique`'s
    // fast path must resolve the chain all the way to the current live
    // version, not silently miss it and let a second live row through.
    let err = exec_super(&engine, "INSERT INTO t VALUES (1, 99)").unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation after a long HOT chain; got {err:?}"
    );

    let sel2 = exec_super(&engine, "SELECT id FROM t").unwrap();
    assert_eq!(
        rows_of(&only_result(sel2)).len(),
        1,
        "must still be exactly one live row for id=1"
    );
}

// ── grammar non-goals (documented rejections) ───────────────────────────────

#[test]
fn composite_conflict_target_is_rejected() {
    let (engine, _dir) = fresh();
    exec_super(
        &engine,
        "CREATE TABLE t (a INT, b INT, val INT, PRIMARY KEY (a, b))",
    )
    .unwrap();
    let err = exec_super(
        &engine,
        "INSERT INTO t (a, b, val) VALUES (1, 1, 1) \
         ON CONFLICT (a, b) DO NOTHING",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::SqlUnsupported(_)), "got {err:?}");
}

#[test]
fn on_constraint_target_is_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    let err = exec_super(
        &engine,
        "INSERT INTO t (id) VALUES (1) ON CONFLICT ON CONSTRAINT t_pkey DO NOTHING",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::SqlUnsupported(_)), "got {err:?}");
}

#[test]
fn do_update_without_explicit_target_is_rejected() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();
    let err = exec_super(
        &engine,
        "INSERT INTO t (id, val) VALUES (1, 1) \
         ON CONFLICT DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::SqlUnsupported(_)), "got {err:?}");
}

#[test]
fn conflict_target_must_be_unique_or_pk() {
    let (engine, _dir) = fresh();
    exec_super(&engine, "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT)").unwrap();
    let err = exec_super(
        &engine,
        "INSERT INTO t (id, tag) VALUES (1, 'x') \
         ON CONFLICT (tag) DO UPDATE SET tag = EXCLUDED.tag",
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidConflictTarget { .. }),
        "got {err:?}"
    );
}
