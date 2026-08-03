#![cfg(feature = "server")]
//! Item 147 — Stored SQL functions (v1) + RPC (`POST /rest/v1/rpc/<fn>`).
//!
//! Follows the `tests/item144_cron.rs` harness pattern closely — same shape
//! of problem (a superuser-registered admin object, control-plane
//! persisted, that's a pure downstream caller of `execute_sql_params_
//! as_principal`). The one deliberate divergence from cron: RPC itself is
//! **invoker**-default (`run_as: None` => the calling principal, not the
//! embedded superuser — see `src/authz/mod.rs::FunctionDef`'s doc comment
//! for the full security-model rationale), so the RLS-parity test (4) is
//! this file's critical test, mirroring `item144_cron.rs`'s own `run_as`
//! parity test but for the *default* case instead of the opt-in one.
//!
//! Test matrix (mirrors the backlog doc's required-tests list):
//!   1. register/list/delete round-trip; delete idempotent; non-superuser
//!      gets 403 on all three admin routes.
//!   2. registration validation: bad name, dup params, empty body, `$3`
//!      with 2 params -> 400 INVALID_FUNCTION_DEF.
//!   3. RPC named args + positional args produce identical results.
//!   4. invoker RLS parity: alice's RPC call returns exactly what alice's
//!      direct `/sql` SELECT returns (not bob's, not the unfiltered set); a
//!      `WITH CHECK`-violating INSERT through RPC is rejected exactly as
//!      via `/sql`.
//!   5. `run_as` definer-analog: runs as the declared role regardless of
//!      caller; caller must still be authenticated.
//!   6. atomicity: a 2-statement body whose second statement fails leaves
//!      the first statement's insert invisible (rolled back).
//!   7. unknown fn -> 404; wrong arg names/count -> 400.
//!   8. params flow through coercion: INT + TEXT args round-trip.

use serde_json::{json, Value};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::{token_for, valid_token, TestServer};

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn sql(server: &TestServer, token: &str, sql_text: &str) -> (u16, Value) {
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql_text }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// Bootstraps a `root` superuser — the standard cast for the admin-route
/// tests below (mirrors `item144_cron.rs::setup_root`).
async fn setup_root(server: &TestServer) -> String {
    let bootstrap = valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    token_for("root")
}

async fn register_function(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url("/functions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = if status == 204 {
        Value::Null
    } else {
        resp.json().await.unwrap_or(Value::Null)
    };
    (status, body)
}

async fn list_functions(server: &TestServer, token: &str) -> (u16, Value) {
    let resp = client()
        .get(server.url("/functions"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn delete_function(server: &TestServer, token: &str, name: &str) -> u16 {
    client()
        .delete(server.url(&format!("/functions/{name}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn rpc(server: &TestServer, token: &str, fn_name: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url(&format!("/rest/v1/rpc/{fn_name}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// An RPC call with no `Authorization` header at all — used by test 5's
/// "caller must still be authenticated" assertion.
async fn rpc_unauthenticated(server: &TestServer, fn_name: &str, body: Value) -> u16 {
    client()
        .post(server.url(&format!("/rest/v1/rpc/{fn_name}")))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn find_function<'a>(functions: &'a Value, name: &str) -> Option<&'a Value> {
    functions.as_array()?.iter().find(|f| f["name"] == name)
}

// ── 1. register/list/delete round-trip; delete idempotent; 403 gating ──────

#[tokio::test]
async fn register_list_delete_round_trip_and_admin_routes_are_superuser_only() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    let alice = token_for("alice");

    // Round-trip as root.
    let (status, _) = register_function(
        &server,
        &root,
        json!({
            "name": "echo1",
            "params": ["x"],
            "body": ["SELECT $1::INT AS x"],
        }),
    )
    .await;
    assert_eq!(status, 204);

    let (status, functions) = list_functions(&server, &root).await;
    assert_eq!(status, 200);
    let f = find_function(&functions, "echo1").expect("registered function present");
    assert_eq!(f["params"], json!(["x"]));
    assert_eq!(f["body"], json!(["SELECT $1::INT AS x"]));
    assert!(f["run_as"].is_null());

    assert_eq!(delete_function(&server, &root, "echo1").await, 204);
    let (_, functions) = list_functions(&server, &root).await;
    assert!(find_function(&functions, "echo1").is_none());

    // Idempotent delete.
    assert_eq!(delete_function(&server, &root, "echo1").await, 204);
    assert_eq!(delete_function(&server, &root, "never-existed").await, 204);

    // A repeated `POST /functions` with the same name upserts (replaces),
    // not duplicates.
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({"name": "up", "params": [], "body": ["SELECT 1"]}),
        )
        .await
        .0,
        204
    );
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({"name": "up", "params": [], "body": ["SELECT 2"]}),
        )
        .await
        .0,
        204
    );
    let (_, functions) = list_functions(&server, &root).await;
    let matching: Vec<_> = functions
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["name"] == "up")
        .collect();
    assert_eq!(matching.len(), 1, "upsert must replace, not duplicate");
    assert_eq!(matching[0]["body"], json!(["SELECT 2"]));

    // Non-superuser gets 403 on all three admin routes.
    let (status, _) = register_function(
        &server,
        &alice,
        json!({"name": "x", "params": [], "body": ["SELECT 1"]}),
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(list_functions(&server, &alice).await.0, 403);
    assert_eq!(delete_function(&server, &alice, "up").await, 403);
}

// ── 2. registration validation -> 400 INVALID_FUNCTION_DEF ─────────────────

#[tokio::test]
async fn registration_validation_rejects_malformed_definitions() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    // Bad name (starts with a digit).
    let (status, body) = register_function(
        &server,
        &root,
        json!({"name": "9bad", "params": [], "body": ["SELECT 1"]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_DEF", "{body}");

    // Duplicate params.
    let (status, body) = register_function(
        &server,
        &root,
        json!({"name": "dupparams", "params": ["a", "a"], "body": ["SELECT $1"]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_DEF", "{body}");

    // Empty body.
    let (status, body) = register_function(
        &server,
        &root,
        json!({"name": "emptybody", "params": [], "body": []}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_DEF", "{body}");

    // A blank statement inside a non-empty body.
    let (status, body) = register_function(
        &server,
        &root,
        json!({"name": "blankstmt", "params": [], "body": ["SELECT 1", "   "]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_DEF", "{body}");

    // `$3` with only 2 declared params.
    let (status, body) = register_function(
        &server,
        &root,
        json!({
            "name": "outofrange",
            "params": ["a", "b"],
            "body": ["SELECT $1, $2, $3"],
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_DEF", "{body}");

    // A well-formed registration after the rejected attempts still works —
    // the failed validations must not have left any partial state behind.
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({"name": "good", "params": ["a", "b"], "body": ["SELECT $1, $2"]}),
        )
        .await
        .0,
        204
    );
}

// ── 3. RPC named args + positional args produce identical results ──────────

#[tokio::test]
async fn named_and_positional_args_produce_identical_results() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    assert_eq!(
        sql(&server, &root, "CREATE TABLE widgets (id INT, label TEXT)")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO widgets (id, label) VALUES (1, 'a'), (2, 'b')"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "widget_by_id",
                "params": ["wid"],
                "body": ["SELECT id, label FROM widgets WHERE id = $1"],
            }),
        )
        .await
        .0,
        204
    );

    let (status_named, body_named) = rpc(&server, &root, "widget_by_id", json!({"wid": 2})).await;
    assert_eq!(status_named, 200, "{body_named}");

    let (status_pos, body_pos) = rpc(&server, &root, "widget_by_id", json!([2])).await;
    assert_eq!(status_pos, 200, "{body_pos}");

    assert_eq!(body_named, body_pos, "named vs positional must agree");
    assert_eq!(body_named["type"], "rows");
    assert_eq!(body_named["rows"], json!([[2, "b"]]));
}

// ── 4. invoker RLS parity (the critical test) ───────────────────────────────

#[tokio::test]
async fn invoker_default_gives_rls_parity_with_direct_sql() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE posts (id INT PRIMARY KEY, owner TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(sql(&server, &root, "CREATE USER bob").await.0, 200);
    assert_eq!(
        sql(&server, &root, "GRANT SELECT, INSERT ON posts TO alice")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(&server, &root, "GRANT SELECT, INSERT ON posts TO bob")
            .await
            .0,
        200
    );

    // Seed rows as root **before** the policy exists — item-24 Z1's INSERT
    // `WITH CHECK` enforcement is a per-row executor check keyed on
    // `ctx.current_user.is_none()` (the embedded/`None` identity), not on
    // `is_effective_superuser` (see `sql/executor.rs`'s Z1 insert-policy
    // comment) — a *named* superuser like `root` does not bypass an
    // identity-dependent `WITH CHECK` policy once one is registered. Seeding
    // ahead of `CREATE POLICY` sidesteps that (orthogonal, pre-existing,
    // out-of-scope-for-this-item) engine behavior entirely.
    let (seed_status, seed_body) = sql(
        &server,
        &root,
        "INSERT INTO posts (id, owner) VALUES (1, 'alice'), (2, 'bob')",
    )
    .await;
    assert_eq!(seed_status, 200, "{seed_body}");

    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON posts FOR ALL USING (owner = current_user) \
         WITH CHECK (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "my_posts",
                "params": [],
                "body": ["SELECT id, owner FROM posts ORDER BY id"],
                // run_as omitted -> invoker (the default under test).
            }),
        )
        .await
        .0,
        204
    );

    let alice = token_for("alice");
    let bob = token_for("bob");

    // Parity, not hardcoded expectations: RPC as alice must equal alice's
    // own direct `/sql` SELECT — not the unfiltered two-row set, not bob's.
    let (direct_status, direct_body) =
        sql(&server, &alice, "SELECT id, owner FROM posts ORDER BY id").await;
    assert_eq!(direct_status, 200);
    let (rpc_status, rpc_body) = rpc(&server, &alice, "my_posts", json!({})).await;
    assert_eq!(rpc_status, 200, "{rpc_body}");
    assert_eq!(rpc_body, direct_body["results"][0]);
    assert_eq!(rpc_body["rows"], json!([[1, "alice"]]));

    let (direct_status, direct_body) =
        sql(&server, &bob, "SELECT id, owner FROM posts ORDER BY id").await;
    assert_eq!(direct_status, 200);
    let (rpc_status, rpc_body) = rpc(&server, &bob, "my_posts", json!({})).await;
    assert_eq!(rpc_status, 200, "{rpc_body}");
    assert_eq!(rpc_body, direct_body["results"][0]);
    assert_eq!(rpc_body["rows"], json!([[2, "bob"]]));

    // A `WITH CHECK`-violating INSERT through RPC is rejected exactly as
    // via `/sql`.
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "insert_post",
                "params": ["id", "owner"],
                "body": ["INSERT INTO posts (id, owner) VALUES ($1, $2) RETURNING id"],
            }),
        )
        .await
        .0,
        204
    );

    let (direct_status, direct_body) = sql(
        &server,
        &alice,
        "INSERT INTO posts (id, owner) VALUES (99, 'mallory') RETURNING id",
    )
    .await;
    assert_eq!(direct_status, 400, "{direct_body}");
    assert_eq!(direct_body["code"], "SQL_PLAN_ERROR");

    let (rpc_status, rpc_body) = rpc(
        &server,
        &alice,
        "insert_post",
        json!({"id": 99, "owner": "mallory"}),
    )
    .await;
    assert_eq!(rpc_status, direct_status, "{rpc_body}");
    assert_eq!(rpc_body["code"], "SQL_PLAN_ERROR", "{rpc_body}");

    // Confirm the rejected insert really didn't land.
    let (_, body) = sql(&server, &root, "SELECT COUNT(*) FROM posts WHERE id = 99").await;
    assert_eq!(body["results"][0]["rows"][0][0], 0);
}

// ── 5. `run_as` definer-analog ──────────────────────────────────────────────

#[tokio::test]
async fn run_as_definer_runs_as_the_declared_role_regardless_of_caller() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE secrets (id INT, payload TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO secrets (id, payload) VALUES (1, 'classified')"
        )
        .await
        .0,
        200
    );
    // A scoped role granted SELECT on `secrets` (but nothing else); no RLS
    // policy is needed for this test — the grant boundary alone is enough
    // to prove `run_as` is really substituting a fixed principal.
    assert_eq!(sql(&server, &root, "CREATE USER reader_role").await.0, 200);
    assert_eq!(
        sql(&server, &root, "GRANT SELECT ON secrets TO reader_role")
            .await
            .0,
        200
    );
    // The caller (alice) is granted nothing on `secrets` directly — any
    // access she gets must come from `run_as`, not her own grants.
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(sql(&server, &root, "CREATE USER carol").await.0, 200);

    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "read_secrets",
                "params": [],
                "body": ["SELECT id, payload FROM secrets ORDER BY id"],
                "run_as": "reader_role",
            }),
        )
        .await
        .0,
        204
    );

    // Direct `/sql` as alice (no `run_as`, her own — empty — grants) is
    // rejected: establishes that alice's own principal has no access.
    let alice = token_for("alice");
    let (direct_status, _) = sql(
        &server,
        &alice,
        "SELECT id, payload FROM secrets ORDER BY id",
    )
    .await;
    assert_ne!(
        direct_status, 200,
        "alice's own grants must not include SELECT on secrets"
    );

    // But calling the `run_as: reader_role` function via RPC succeeds,
    // regardless of which authenticated caller invokes it.
    let (status, body) = rpc(&server, &alice, "read_secrets", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"], json!([[1, "classified"]]));

    let carol = token_for("carol");
    let (status, body) = rpc(&server, &carol, "read_secrets", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"], json!([[1, "classified"]]));

    // The caller must still be authenticated — no JWT at all is rejected
    // before `run_as` (or invoker resolution) ever runs.
    let unauth_status = rpc_unauthenticated(&server, "read_secrets", json!({})).await;
    assert_eq!(unauth_status, 401);
}

// ── 6. atomicity: a failing second statement rolls back the first ──────────

#[tokio::test]
async fn a_failing_statement_rolls_back_earlier_statements_in_the_same_call() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT UNIQUE)")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(&server, &root, "INSERT INTO markers (n) VALUES (1)")
            .await
            .0,
        200
    );

    // Statement 1 inserts a fresh row; statement 2 violates the UNIQUE
    // constraint on the pre-existing row — the whole call must roll back,
    // including statement 1's insert.
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "two_step",
                "params": [],
                "body": [
                    "INSERT INTO markers (n) VALUES (2)",
                    "INSERT INTO markers (n) VALUES (1)",
                ],
            }),
        )
        .await
        .0,
        204
    );

    let (status, body) = rpc(&server, &root, "two_step", json!({})).await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(body["code"], "UNIQUE_VIOLATION", "{body}");

    let (_, body) = sql(&server, &root, "SELECT n FROM markers ORDER BY n").await;
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rows,
        &vec![json!([1])],
        "statement 1's insert must not be visible after statement 2 failed"
    );
}

// ── 7. unknown fn -> 404; wrong arg names/count -> 400 ─────────────────────

#[tokio::test]
async fn unknown_function_is_404_and_bad_args_are_400() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    let (status, body) = rpc(&server, &root, "does_not_exist", json!({})).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "FUNCTION_NOT_FOUND", "{body}");

    assert_eq!(
        register_function(
            &server,
            &root,
            json!({"name": "needs_two", "params": ["a", "b"], "body": ["SELECT $1, $2"]}),
        )
        .await
        .0,
        204
    );

    // Wrong named-arg names (extra + missing).
    let (status, body) = rpc(
        &server,
        &root,
        "needs_two",
        json!({"a": 1, "wrong_name": 2}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_ARGS", "{body}");

    // Missing an arg entirely.
    let (status, body) = rpc(&server, &root, "needs_two", json!({"a": 1})).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_ARGS", "{body}");

    // Wrong positional-array length (too few / too many).
    let (status, body) = rpc(&server, &root, "needs_two", json!([1])).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_ARGS", "{body}");

    let (status, body) = rpc(&server, &root, "needs_two", json!([1, 2, 3])).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "INVALID_FUNCTION_ARGS", "{body}");

    // The correct shape succeeds.
    let (status, body) = rpc(&server, &root, "needs_two", json!({"a": 1, "b": 2})).await;
    assert_eq!(status, 200, "{body}");
}

// ── 8. params flow through coercion: INT + TEXT round-trip ─────────────────

#[tokio::test]
async fn int_and_text_args_round_trip_through_coercion() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE gadgets (id INT, name TEXT)")
            .await
            .0,
        200
    );
    assert_eq!(
        register_function(
            &server,
            &root,
            json!({
                "name": "add_gadget",
                "params": ["gid", "gname"],
                "body": [
                    "INSERT INTO gadgets (id, name) VALUES ($1, $2)",
                    "SELECT id, name FROM gadgets WHERE id = $1",
                ],
            }),
        )
        .await
        .0,
        204
    );

    let (status, body) = rpc(
        &server,
        &root,
        "add_gadget",
        json!({"gid": 42, "gname": "sprocket"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "rows");
    assert_eq!(body["rows"], json!([[42, "sprocket"]]));

    // Confirm durability + the exact types stored (not just echoed back
    // in-transaction).
    let (_, body) = sql(&server, &root, "SELECT id, name FROM gadgets WHERE id = 42").await;
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows, &vec![json!([42, "sprocket"])]);
}
