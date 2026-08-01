#![cfg(feature = "server")]
//! Item 123 (Workstream C4): `POST /graphql` schema-derived GraphQL API
//! integration tests. Covers scalar fields, FK forward/reverse nesting,
//! graph edge traversal, vector `near` search, introspection, injection
//! safety, and — the hard constraint — RLS/table/column-grant parity with
//! `POST /sql` under the same restricted principal.

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

/// `errors` is absent/empty on a successful GraphQL response.
fn no_errors(body: &Value) -> bool {
    body["errors"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

fn errors(body: &Value) -> Vec<Value> {
    body["errors"].as_array().cloned().unwrap_or_default()
}

async fn setup_items(server: &TestServer) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
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

async fn setup_customers_orders(server: &TestServer) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            server,
            &root,
            "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            server,
            &root,
            "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), \
             total INT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            server,
            &root,
            "INSERT INTO customers (id, name) VALUES (1, 'acme'), (2, 'globex')"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            server,
            &root,
            "INSERT INTO orders (id, customer_id, total) VALUES (100, 1, 10), (101, 2, 20), (102, 1, 30)"
        )
        .await
        .0,
        200
    );
    root
}

// ── (a) scalar fields ───────────────────────────────────────────────────────

#[tokio::test]
async fn scalar_fields_query_returns_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(
        &server,
        &root,
        "{ items(orderBy: \"id.asc\") { id name price } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], json!({"id": 1, "name": "apple", "price": 10}));
    assert_eq!(items[1], json!({"id": 2, "name": "banana", "price": 20}));

    // Filter argument (eq shorthand).
    let (status, body) = graphql(&server, &root, "{ items(id: 2) { name } }").await;
    assert_eq!(status, 200, "{body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items, &vec![json!({"name": "banana"})]);

    // Suffixed operator + limit.
    let (status, body) = graphql(
        &server,
        &root,
        "{ items(price_gt: 10, orderBy: \"id.asc\", limit: 1) { id } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items, &vec![json!({"id": 2})]);
}

// ── (b) FK forward + reverse relationship fields ────────────────────────────

#[tokio::test]
async fn fk_forward_and_reverse_fields_nest() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    // Forward (many-to-one): order -> its customer, nested as an object.
    let (status, body) = graphql(
        &server,
        &root,
        "{ orders(orderBy: \"id.asc\") { id customer { id name } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let rows = body["data"]["orders"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["customer"], json!({"id": 1, "name": "acme"}));
    assert_eq!(rows[1]["customer"], json!({"id": 2, "name": "globex"}));

    // Reverse (one-to-many): customer -> its orders, nested as an array.
    let (status, body) = graphql(
        &server,
        &root,
        "{ customers(id: 1) { id orders { id total } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let custs = body["data"]["customers"].as_array().unwrap();
    assert_eq!(custs.len(), 1);
    let child_orders = custs[0]["orders"].as_array().unwrap();
    assert_eq!(child_orders.len(), 2, "{body}");
}

// ── (c) graph edge traversal field ──────────────────────────────────────────

#[tokio::test]
async fn graph_edges_field_returns_connected_rows() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE people (id INT PRIMARY KEY, name TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO people (id, name) VALUES (1, 'alice'), (2, 'bob')"
        )
        .await
        .0,
        200
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/edges"))
        .header("Authorization", format!("Bearer {root}"))
        .json(&json!({"from_id": 1, "to_id": 2, "edge_type": "KNOWS", "props": {"since": 2020}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let (status, body) = graphql(
        &server,
        &root,
        "{ people(id: 1) { id edges { toId edgeType props } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let p = &body["data"]["people"][0];
    let edges = p["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{body}");
    assert_eq!(edges[0]["toId"], 2);
    assert_eq!(edges[0]["edgeType"], "KNOWS");
    assert_eq!(edges[0]["props"]["since"], 2020);

    // IN direction from bob's side.
    let (status, body) = graphql(
        &server,
        &root,
        "{ people(id: 2) { id edges(direction: IN) { toId edgeType } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let edges = body["data"]["people"][0]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{body}");
    assert_eq!(edges[0]["toId"], 2);

    // A non-superuser caller with no grant on `__edges__` is denied — fail
    // closed, exactly like a direct `SELECT * FROM __edges__` over `/sql`
    // would be for the same caller (see graphql.rs's module doc: `edges`
    // runs the enforced path against `__edges__`, not the unauthorized
    // `Engine::edges_from` the lower-level `/edges/from/:id` route uses).
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(
        sql(&server, &root, "GRANT SELECT ON people TO alice")
            .await
            .0,
        200
    );
    let alice = token_for("alice");
    let (status, body) = graphql(&server, &alice, "{ people(id: 1) { id edges { toId } } }").await;
    assert_eq!(status, 200, "GraphQL errors are still HTTP 200: {body}");
    let errs = errors(&body);
    assert!(!errs.is_empty(), "expected a permission error: {body}");
    assert!(
        errs[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("PERMISSION_DENIED"),
        "{body}"
    );

    // The same `SELECT * FROM __edges__` would 403 for alice directly too —
    // proving this isn't a GraphQL-specific denial.
    let (direct_status, _) = sql(&server, &alice, "SELECT * FROM __edges__").await;
    assert_eq!(direct_status, 403);
}

// ── (d) vector `near` field ──────────────────────────────────────────────────

#[tokio::test]
async fn vector_near_field_returns_nearest_rows() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE docs (id INT PRIMARY KEY, embedding VECTOR(3))"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE INDEX ON docs USING HNSW (embedding)"
        )
        .await
        .0,
        200
    );
    let ins = sql(
        &server,
        &root,
        "INSERT INTO docs (id, embedding) VALUES \
         (1, [1.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0]), (3, [0.9, 0.1, 0.0])",
    )
    .await;
    assert_eq!(ins.0, 200, "{:?}", ins.1);

    let (status, body) = graphql(
        &server,
        &root,
        "{ near_docs(vector: [1.0, 0.0, 0.0], k: 2) { id } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let rows = body["data"]["near_docs"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&1), "{ids:?}");
    assert!(
        ids.contains(&3),
        "expected the near-duplicate vector as the 2nd-nearest neighbor: {ids:?}"
    );
    assert!(
        !ids.contains(&2),
        "the orthogonal vector must not be in the top 2: {ids:?}"
    );
}

// ── (e) RLS / table+column-grant parity with /sql ───────────────────────────

/// A restricted user with a `current_user`-referencing RLS policy must see
/// EXACTLY the same rows via GraphQL as via `/sql`; an ungranted column must
/// be denied identically (fail closed, not partial data).
#[tokio::test]
async fn rls_grant_parity_with_sql() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");

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
        sql(&server, &root, "GRANT SELECT ON posts TO alice")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO posts (id, owner) VALUES (1, 'alice'), (2, 'root'), (3, 'alice')"
        )
        .await
        .0,
        200
    );
    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    let alice = token_for("alice");

    // Via /sql.
    let (sql_status, sql_body) =
        sql(&server, &alice, "SELECT id, owner FROM posts ORDER BY id").await;
    assert_eq!(sql_status, 200, "{sql_body}");
    let sql_rows = sql_body["results"][0]["rows"].as_array().unwrap().clone();

    // Via GraphQL — same table, same caller, same (implicit) predicate.
    let (gql_status, gql_body) = graphql(
        &server,
        &alice,
        "{ posts(orderBy: \"id.asc\") { id owner } }",
    )
    .await;
    assert_eq!(gql_status, 200, "{gql_body}");
    assert!(no_errors(&gql_body), "{gql_body}");
    let gql_rows = gql_body["data"]["posts"].as_array().unwrap();

    assert_eq!(
        sql_rows.len(),
        2,
        "alice's RLS policy should admit exactly her 2 rows: {sql_rows:?}"
    );
    assert_eq!(
        gql_rows.len(),
        sql_rows.len(),
        "GraphQL must see the same row count as /sql under RLS: {gql_body}"
    );
    for (sql_row, gql_row) in sql_rows.iter().zip(gql_rows.iter()) {
        assert_eq!(sql_row[0], gql_row["id"]);
        assert_eq!(sql_row[1], gql_row["owner"]);
    }

    // Column-grant parity.
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
            "GRANT SELECT (id, email) ON accounts TO support"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO accounts (id, email, secret) VALUES (1, 'a@x.com', 'shh')"
        )
        .await
        .0,
        200
    );
    let support = token_for("support");

    // Granted columns: both surfaces succeed identically.
    let (sql_status, _) = sql(&server, &support, "SELECT id, email FROM accounts").await;
    assert_eq!(sql_status, 200);
    let (gql_status, gql_body) = graphql(&server, &support, "{ accounts { id email } }").await;
    assert_eq!(gql_status, 200, "{gql_body}");
    assert!(no_errors(&gql_body), "{gql_body}");

    // Ungranted column ('secret'): both surfaces deny identically.
    let (sql_status, _) = sql(&server, &support, "SELECT secret FROM accounts").await;
    assert_eq!(sql_status, 403);
    let (gql_status, gql_body) = graphql(&server, &support, "{ accounts { id secret } }").await;
    assert_eq!(
        gql_status, 200,
        "GraphQL errors are still HTTP 200: {gql_body}"
    );
    let errs = errors(&gql_body);
    assert!(
        !errs.is_empty(),
        "ungranted column must be denied: {gql_body}"
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
        "a denied non-null field must null-propagate to the whole response, never leak partial data: {gql_body}"
    );
}

// ── (f) introspection ────────────────────────────────────────────────────────

#[tokio::test]
async fn introspection_returns_schema() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    let (status, body) = graphql(
        &server,
        &root,
        "{ __schema { queryType { name } types { name } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    assert_eq!(body["data"]["__schema"]["queryType"]["name"], "Query");

    let type_names: Vec<String> = body["data"]["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(type_names.contains(&"items".to_string()), "{type_names:?}");
    assert!(type_names.contains(&"Edge".to_string()), "{type_names:?}");
    assert!(
        type_names.contains(&"EdgeDirection".to_string()),
        "{type_names:?}"
    );
    assert!(type_names.contains(&"JSON".to_string()), "{type_names:?}");

    // A field-level introspection query also works (tooling like G4 walks
    // this to build a query builder / API-docs panel).
    let (status, body) = graphql(
        &server,
        &root,
        "{ __type(name: \"items\") { fields { name } } }",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let field_names: Vec<String> = body["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert!(field_names.contains(&"id".to_string()), "{field_names:?}");
    assert!(field_names.contains(&"name".to_string()), "{field_names:?}");
    assert!(
        field_names.contains(&"price".to_string()),
        "{field_names:?}"
    );
}

// ── (g) injection safety ─────────────────────────────────────────────────────

#[tokio::test]
async fn injection_attempt_in_filter_value_is_treated_as_data() {
    let server = TestServer::spawn().await;
    let root = setup_items(&server).await;

    // A malicious filter *value* is always a bind parameter — it can only
    // ever be compared as data, never change the query's structure.
    let query = r#"{ items(name: "z' OR '1'='1") { id } }"#;
    let (status, body) = graphql(&server, &root, query).await;
    assert_eq!(status, 200, "{body}");
    assert!(no_errors(&body), "{body}");
    let rows = body["data"]["items"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        0,
        "malicious filter value must match no row, not leak the table: {body}"
    );

    // A malicious `orderBy` value is validated against the catalog
    // (`validate_column`), not built into SQL text — an unknown "column"
    // is rejected as a GraphQL field error, never executed.
    let (status, body) = graphql(
        &server,
        &root,
        r#"{ items(orderBy: "id; DROP TABLE items--") { id } }"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let errs = errors(&body);
    assert!(
        !errs.is_empty(),
        "malicious orderBy column must be rejected: {body}"
    );

    // The table must still exist and hold its original rows — no DROP
    // TABLE ever executed.
    let (status, body) = graphql(&server, &root, "{ items { id } }").await;
    assert_eq!(status, 200, "items must survive: {body}");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 2);
}
