#![cfg(feature = "server")]
//! Item 133: `POST /graphql` `Mutation` root — `insert_<t>`/`update_<t>`/
//! `delete_<t>` per eligible table, at full ACID/RLS parity with `/rest/v1`
//! and `/sql` (see `src/server/graphql.rs`'s module doc and
//! `docs/backlog/133_graphql_mutations.md`). Covers end-to-end insert/
//! update/delete returning the requested projection, RLS/`WITH CHECK`
//! rejection parity, and column-grant (RETURNING projection) denial parity.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::{json, Value};
use server_common::{token_for, valid_token, TestServer};

async fn sql(server: &TestServer, token: &str, sql: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn graphql(server: &TestServer, token: &str, query: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/graphql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "query": query }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

fn no_errors(body: &Value) -> bool {
    body["errors"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

fn errors(body: &Value) -> Vec<Value> {
    body["errors"].as_array().cloned().unwrap_or_default()
}

async fn root_user(server: &TestServer) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    token_for("root")
}

async fn setup_items(server: &TestServer) -> String {
    let root = root_user(server).await;
    assert_eq!(
        sql(
            server,
            &root,
            "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, price INT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            server,
            &root,
            "INSERT INTO items (id, name, price) VALUES (1, 'apple', 10), (2, 'banana', 20)"
        )
        .await
        .0,
        200
    );
    root
}

// ── insert/update/delete end-to-end, returning the requested projection ────

#[tokio::test]
async fn insert_mutation_returns_requested_projection() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(
        &server,
        &root,
        r#"mutation { insert_items(values: { id: 3, name: "cherry", price: 30 }) { id name price } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    assert_eq!(
        body["data"]["insert_items"],
        json!({"id": 3, "name": "cherry", "price": 30})
    );

    // A projection that only asks for a subset of columns gets back exactly
    // that subset — never a blanket `SELECT *`-shaped row.
    let (status, body) = graphql(
        &server,
        &root,
        r#"mutation { insert_items(values: { id: 4, name: "date", price: 40 }) { id } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["insert_items"], json!({"id": 4}));

    // The row is really durable: visible via a plain query field too.
    let (status, body) = graphql(&server, &root, "{ items(id: 3) { id name price } }").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"]["items"][0],
        json!({"id": 3, "name": "cherry", "price": 30})
    );

    // An unknown column in `values` is rejected the same way REST rejects it.
    let (status, body) = graphql(
        &server,
        &root,
        r#"mutation { insert_items(values: { id: 5, bogus: 1 }) { id } }"#,
    )
    .await;
    assert_eq!(status, 200, "GraphQL errors are still HTTP 200: {body}");
    assert!(!errors(&body).is_empty(), "{body}");
    let (rest_status, rest_body) = {
        let client = reqwest::Client::new();
        let resp = client
            .post(server.url("/rest/v1/items"))
            .header("Authorization", format!("Bearer {root}"))
            .json(&json!({"id": 5, "bogus": 1}))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        (status, body)
    };
    assert_eq!(
        rest_status, 404,
        "REST POST with an unknown column must also be rejected: {rest_body}"
    );
}

#[tokio::test]
async fn update_mutation_returns_updated_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(
        &server,
        &root,
        r#"mutation { update_items(id: 1, set: { price: 99 }) { id name price } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let rows = body["data"]["update_items"].as_array().unwrap();
    assert_eq!(rows, &vec![json!({"id": 1, "name": "apple", "price": 99})]);

    // A filter matching multiple rows updates and returns all of them.
    let (status, body) = graphql(
        &server,
        &root,
        r#"mutation { update_items(price_gt: 0, set: { price: 0 }) { id price } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rows = body["data"]["update_items"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    assert!(rows.iter().all(|r| r["price"] == 0), "{body}");

    // Durable: visible via a plain query afterward.
    let (status, body) = graphql(&server, &root, "{ items(id: 1) { price } }").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["items"][0]["price"], 0);
}

#[tokio::test]
async fn delete_mutation_returns_deleted_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(
        &server,
        &root,
        "mutation { delete_items(id: 2) { id name } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let rows = body["data"]["delete_items"].as_array().unwrap();
    assert_eq!(rows, &vec![json!({"id": 2, "name": "banana"})]);

    // Really gone: a plain query no longer finds it.
    let (status, body) = graphql(&server, &root, "{ items(id: 2) { id } }").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 0);

    // The other row is untouched.
    let (status, body) = graphql(&server, &root, "{ items { id } }").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
}

// ── RLS / WITH CHECK enforcement (shared path — item 133 spec §"Correctness details") ─

/// An INSERT that violates a `WITH CHECK` policy is rejected identically via
/// GraphQL and `/sql` — same `DbError::SqlPlan("... violates policy ...")`
/// -> `SQL_PLAN_ERROR` mapping either way, since both routes end at
/// `execute_sql_params_as_principal`.
#[tokio::test]
async fn insert_mutation_rejected_by_with_check() {
    let server = TestServer::spawn().await;
    let root = root_user(&server).await;
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
    assert_eq!(
        sql(&server, &root, "GRANT INSERT, SELECT ON posts TO alice")
            .await
            .0,
        200
    );
    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON posts FOR ALL USING (owner = current_user) \
         WITH CHECK (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    let alice = token_for("alice");

    // Via `/sql`: alice cannot insert a row she doesn't own.
    let (sql_status, sql_body) = sql(
        &server,
        &alice,
        "INSERT INTO posts (id, owner) VALUES (1, 'bob') RETURNING id",
    )
    .await;
    assert_eq!(sql_status, 400, "{sql_body}");
    assert_eq!(sql_body["code"], "SQL_PLAN_ERROR", "{sql_body}");

    // Via GraphQL: the identical rejection.
    let (gql_status, gql_body) = graphql(
        &server,
        &alice,
        r#"mutation { insert_posts(values: { id: 1, owner: "bob" }) { id } }"#,
    )
    .await;
    assert_eq!(
        gql_status, 200,
        "GraphQL errors are still HTTP 200: {gql_body}"
    );
    let errs = errors(&gql_body);
    assert!(!errs.is_empty(), "expected a policy violation: {gql_body}");
    assert!(
        errs[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("SQL_PLAN_ERROR"),
        "{gql_body}"
    );

    // A row inserted with the correct owner succeeds identically on both.
    let (gql_status, gql_body) = graphql(
        &server,
        &alice,
        r#"mutation { insert_posts(values: { id: 2, owner: "alice" }) { id owner } }"#,
    )
    .await;
    assert_eq!(gql_status, 200, "{gql_body}");
    assert!(no_errors(&gql_body), "{gql_body}");
    assert_eq!(
        gql_body["data"]["insert_posts"],
        json!({"id": 2, "owner": "alice"})
    );
}

/// An UPDATE that would transfer ownership (violating `WITH CHECK`) is
/// rejected identically via GraphQL and `/sql`; an in-policy update succeeds.
#[tokio::test]
async fn update_mutation_rejected_by_with_check() {
    let server = TestServer::spawn().await;
    let root = root_user(&server).await;
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE todos (id INT PRIMARY KEY, owner TEXT, body TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(
        sql(
            &server,
            &root,
            "GRANT INSERT, SELECT, UPDATE ON todos TO alice"
        )
        .await
        .0,
        200
    );
    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON todos FOR ALL USING (owner = current_user) \
         WITH CHECK (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    let alice = token_for("alice");
    // Seeded by alice herself, not `root` — `root` is a *named* SUPERUSER
    // principal (`current_user = Some("root")`), and this engine's per-row
    // INSERT/`WITH CHECK` path only bypasses for the embedded (`None`)
    // caller or an explicit `service_role` claim (see `sql/executor.rs`'s
    // `exec_insert` doc comment) — a named superuser is still subject to a
    // table's own RLS policies, so `root` inserting a row owned by 'alice'
    // would itself violate this policy. Out of scope for item 133 to change;
    // this test only needs a row alice legitimately owns.
    assert_eq!(
        sql(
            &server,
            &alice,
            "INSERT INTO todos (id, owner, body) VALUES (1, 'alice', 'buy milk')"
        )
        .await
        .0,
        200
    );

    // Via `/sql`: alice cannot reassign her own row to bob.
    let (sql_status, sql_body) = sql(
        &server,
        &alice,
        "UPDATE todos SET owner = 'bob' WHERE id = 1 RETURNING id",
    )
    .await;
    assert_eq!(sql_status, 400, "{sql_body}");

    // Via GraphQL: the identical rejection.
    let (gql_status, gql_body) = graphql(
        &server,
        &alice,
        r#"mutation { update_todos(id: 1, set: { owner: "bob" }) { id } }"#,
    )
    .await;
    assert_eq!(gql_status, 200, "{gql_body}");
    let errs = errors(&gql_body);
    assert!(!errs.is_empty(), "expected a policy violation: {gql_body}");
    assert!(
        errs[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("SQL_PLAN_ERROR"),
        "{gql_body}"
    );

    // A within-policy update (editing a non-owner field) succeeds.
    let (gql_status, gql_body) = graphql(
        &server,
        &alice,
        r#"mutation { update_todos(id: 1, set: { body: "buy oat milk" }) { id body } }"#,
    )
    .await;
    assert_eq!(gql_status, 200, "{gql_body}");
    assert!(no_errors(&gql_body), "{gql_body}");
    assert_eq!(
        gql_body["data"]["update_todos"],
        json!([{"id": 1, "body": "buy oat milk"}])
    );
}

// ── column-grant (RETURNING projection) denial parity ──────────────────────

/// A caller who can `INSERT`/`UPDATE` a table but lacks a `SELECT` grant on
/// one of its columns is denied when a mutation's selection set asks for
/// that column back — `RETURNING`'s column list is authorized exactly like a
/// `SELECT` projection (`Engine::check_returning`), so this must match `/sql`
/// exactly. Granted columns succeed identically.
#[tokio::test]
async fn mutation_projection_over_ungranted_column_denied() {
    let server = TestServer::spawn().await;
    let root = root_user(&server).await;
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE accounts (id INT PRIMARY KEY, email TEXT, secret TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER support").await.0, 200);
    assert_eq!(
        sql(
            &server,
            &root,
            "GRANT INSERT, UPDATE ON accounts TO support"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "GRANT SELECT (id, email) ON accounts TO support"
        )
        .await
        .0,
        200
    );
    let support = token_for("support");

    // Parity check via `/sql` first: RETURNING an ungranted column is 403.
    let (sql_status, sql_body) = sql(
        &server,
        &support,
        "INSERT INTO accounts (id, email, secret) VALUES (1, 'a@x.com', 'shh') \
         RETURNING id, secret",
    )
    .await;
    assert_eq!(sql_status, 403, "{sql_body}");

    // Via GraphQL: the identical denial — projecting the ungranted `secret`
    // column back is rejected, and no row was left half-inserted (the whole
    // statement is one enforced, pre-checked call).
    let (gql_status, gql_body) = graphql(
        &server,
        &support,
        r#"mutation { insert_accounts(values: { id: 2, email: "b@x.com", secret: "zzz" }) { id secret } }"#,
    )
    .await;
    assert_eq!(
        gql_status, 200,
        "GraphQL errors are still HTTP 200: {gql_body}"
    );
    let errs = errors(&gql_body);
    assert!(
        !errs.is_empty(),
        "ungranted RETURNING column must be denied: {gql_body}"
    );
    assert!(
        errs[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("PERMISSION_DENIED"),
        "{gql_body}"
    );
    assert!(
        gql_body["data"].is_null(),
        "a denied non-null field must null-propagate, never leak partial data: {gql_body}"
    );

    // The row must not exist afterward — the whole statement (including the
    // rejected RETURNING) failed as one unit, same as the authorization
    // pre-check REST/`/sql` do (`authorize_sql_as_principal` runs before
    // `begin`, so nothing was ever written).
    let (root_status, root_body) =
        sql(&server, &root, "SELECT id FROM accounts WHERE id = 2").await;
    assert_eq!(root_status, 200, "{root_body}");
    assert_eq!(
        root_body["results"][0]["rows"].as_array().unwrap().len(),
        0,
        "no partial insert should have happened: {root_body}"
    );

    // A projection over only granted columns succeeds on both surfaces.
    let (sql_status, sql_body) = sql(
        &server,
        &support,
        "INSERT INTO accounts (id, email, secret) VALUES (3, 'c@x.com', 'shh') \
         RETURNING id, email",
    )
    .await;
    assert_eq!(sql_status, 200, "{sql_body}");

    let (gql_status, gql_body) = graphql(
        &server,
        &support,
        r#"mutation { insert_accounts(values: { id: 4, email: "d@x.com", secret: "shh" }) { id email } }"#,
    )
    .await;
    assert_eq!(gql_status, 200, "{gql_body}");
    assert!(no_errors(&gql_body), "{gql_body}");
    assert_eq!(
        gql_body["data"]["insert_accounts"],
        json!({"id": 4, "email": "d@x.com"})
    );
}

// ── pre-existing behavior unaffected ────────────────────────────────────────

#[tokio::test]
async fn mutation_root_present_once_a_table_exists() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(&server, &root, "{ __schema { mutationType { name } } }").await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    assert_eq!(body["data"]["__schema"]["mutationType"]["name"], "Mutation");

    let (status, body) = graphql(
        &server,
        &root,
        r#"{ __type(name: "Mutation") { fields { name } } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<String> = body["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"insert_items".to_string()), "{names:?}");
    assert!(names.contains(&"update_items".to_string()), "{names:?}");
    assert!(names.contains(&"delete_items".to_string()), "{names:?}");
}
