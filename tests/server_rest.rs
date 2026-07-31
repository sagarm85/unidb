//! Item 123 (Workstream C1): `/rest/v1/<table>` auto REST API integration
//! tests. Covers the full filter-operator matrix, CRUD verbs, injection
//! safety (a malicious filter value must be treated as data, never SQL
//! structure), unknown-table/column rejection, and — the hard constraint —
//! RLS/grant parity with `POST /sql` under the same restricted principal.

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

async fn rest_get(server: &TestServer, token: &str, path: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn rest_post(server: &TestServer, token: &str, table: &str, body: Value) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url(&format!("/rest/v1/{table}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn rest_patch(server: &TestServer, token: &str, path: &str, body: Value) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .patch(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn rest_delete(server: &TestServer, token: &str, path: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .delete(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// `rows` field of a `{"type":"rows", "columns":[...], "rows":[[...]]}` body.
fn rows(body: &Value) -> &Vec<Value> {
    body["rows"].as_array().unwrap()
}

async fn setup_items_table(server: &TestServer) -> String {
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
            "CREATE TABLE items (id INT, name TEXT, price INT, active BOOL)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            server,
            &root,
            "INSERT INTO items (id, name, price, active) VALUES \
             (1, 'apple', 10, TRUE), \
             (2, 'banana', 20, FALSE), \
             (3, 'cherry', 30, TRUE), \
             (4, 'date', 40, NULL)"
        )
        .await
        .0,
        200
    );
    root
}

// ── (a) GET with each operator ──────────────────────────────────────────────

#[tokio::test]
async fn get_eq_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?id=eq.2").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], "banana");
}

#[tokio::test]
async fn get_neq_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?id=neq.2&select=id").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn get_gt_gte_lt_lte_filters() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_get(&server, &root, "items?price=gt.20&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 2); // 30, 40

    let (status, body) = rest_get(&server, &root, "items?price=gte.20&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 3); // 20,30,40

    let (status, body) = rest_get(&server, &root, "items?price=lt.20&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 1); // 10

    let (status, body) = rest_get(&server, &root, "items?price=lte.20&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 2); // 10,20
}

#[tokio::test]
async fn get_like_ilike_filters() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_get(&server, &root, "items?name=like.a%25&select=id").await;
    assert_eq!(status, 200, "{body}");
    // "apple" and "banana" and "date" all... only those starting with 'a':
    // apple. banana contains 'a' but doesn't start with it — `a%` requires
    // prefix 'a'. Only "apple" matches.
    assert_eq!(rows(&body).len(), 1);

    let (status, body) = rest_get(&server, &root, "items?name=ilike.A%25&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 1); // case-insensitive "apple"
}

#[tokio::test]
async fn get_in_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?id=in.(1,3)&select=id&order=id.asc").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], 1);
    assert_eq!(r[1][0], 3);
}

#[tokio::test]
async fn get_is_null_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?active=is.null&select=id").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], 4);
}

#[tokio::test]
async fn get_is_true_false_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?active=is.true&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 2);

    let (status, body) = rest_get(&server, &root, "items?active=is.false&select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 1);
}

#[tokio::test]
async fn get_select_order_limit_offset() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_get(&server, &root, "items?select=id,name&order=price.desc").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["columns"], json!(["id", "name"]));
    let r = rows(&body);
    assert_eq!(r.len(), 4);
    assert_eq!(r[0][0], 4); // price 40 first (desc)

    let (status, body) = rest_get(&server, &root, "items?order=price.asc&limit=2").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], "apple");

    let (status, body) = rest_get(&server, &root, "items?order=price.asc&limit=2&offset=2").await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], "cherry");
}

#[tokio::test]
async fn get_multiple_filters_and_together() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(
        &server,
        &root,
        "items?price=gte.20&price=lte.30&select=id&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2); // 20, 30
    assert_eq!(r[0][0], 2);
    assert_eq!(r[1][0], 3);
}

// ── (b) POST / PATCH / DELETE ────────────────────────────────────────────────

#[tokio::test]
async fn post_insert_single_and_batch() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_post(
        &server,
        &root,
        "items",
        json!({"id": 5, "name": "elderberry", "price": 50, "active": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "inserted");
    assert_eq!(body["count"], 1);

    let (status, body) = rest_post(
        &server,
        &root,
        "items",
        json!([
            {"id": 6, "name": "fig", "price": 60, "active": true},
            {"id": 7, "name": "grape", "price": 70, "active": false},
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 2);

    let (status, body) = rest_get(&server, &root, "items?select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 7);
}

#[tokio::test]
async fn patch_update_with_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_patch(&server, &root, "items?id=eq.1", json!({"price": 999})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "updated");
    assert_eq!(body["count"], 1);

    let (_, body) = rest_get(&server, &root, "items?id=eq.1&select=price").await;
    assert_eq!(rows(&body)[0][0], 999);
}

#[tokio::test]
async fn patch_update_with_in_filter_expands_and_sums() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_patch(
        &server,
        &root,
        "items?id=in.(1,2,3)",
        json!({"active": false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "updated");
    assert_eq!(body["count"], 3);

    let (_, body) = rest_get(&server, &root, "items?active=is.false&select=id").await;
    assert_eq!(rows(&body).len(), 3); // 1,2,3 now false (4 stays NULL)
}

#[tokio::test]
async fn delete_with_filter() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_delete(&server, &root, "items?id=eq.2").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "deleted");
    assert_eq!(body["count"], 1);

    let (_, body) = rest_get(&server, &root, "items?select=id").await;
    assert_eq!(rows(&body).len(), 3);
}

#[tokio::test]
async fn delete_with_in_filter_expands_and_sums() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let (status, body) = rest_delete(&server, &root, "items?id=in.(1,2)").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 2);

    let (_, body) = rest_get(&server, &root, "items?select=id").await;
    assert_eq!(rows(&body).len(), 2);
}

// ── (c) Injection safety ─────────────────────────────────────────────────────

/// A malicious filter value must be bound purely as data — never re-parsed
/// as SQL. It should either match no row (a comparable text/text case) or
/// fail as a plain type-coercion error (never as a syntax error caused by
/// injected SQL), and the table must survive intact either way.
#[tokio::test]
async fn injection_attempt_in_filter_value_is_treated_as_data() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let malicious = "1) OR (1=1";
    let path = format!("items?name=eq.{}", urlencoding_percent(malicious));
    let (status, body) = rest_get(&server, &root, &path).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        rows(&body).len(),
        0,
        "malicious value must match no row, not leak the whole table: {body}"
    );

    // A classic DROP-TABLE-shaped payload as a bind value: must not affect
    // the schema at all.
    let drop_payload = "'; DROP TABLE items; --";
    let path = format!("items?name=eq.{}", urlencoding_percent(drop_payload));
    let (status, _) = rest_get(&server, &root, &path).await;
    assert_eq!(status, 200);

    // The table must still exist and hold all 4 original rows.
    let (status, body) = rest_get(&server, &root, "items?select=id").await;
    assert_eq!(status, 200, "table must survive: {body}");
    assert_eq!(rows(&body).len(), 4);

    // Same payload via POST insert (as a TEXT value) must be stored
    // verbatim, not executed.
    let (status, body) = rest_post(
        &server,
        &root,
        "items",
        json!({"id": 99, "name": drop_payload, "price": 1, "active": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let path = format!(
        "items?name=eq.{}&select=id",
        urlencoding_percent(drop_payload)
    );
    let (status, body) = rest_get(&server, &root, &path).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        rows(&body).len(),
        1,
        "the literal malicious string must be stored and match itself: {body}"
    );
    assert_eq!(rows(&body)[0][0], 99);

    // The table still exists with the expected row count (4 original + 1
    // injected-payload row) — no DROP TABLE ever executed.
    let (status, body) = rest_get(&server, &root, "items?select=id").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(rows(&body).len(), 5);
}

/// A minimal, dependency-free percent-encoder for query values used only in
/// these tests (`reqwest` doesn't encode values embedded in a raw path
/// string we build ourselves).
fn urlencoding_percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── (d) unknown table / column ──────────────────────────────────────────────

#[tokio::test]
async fn unknown_table_is_404() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "nope").await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn unknown_column_in_filter_is_404() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?nope=eq.1").await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn unknown_column_in_select_is_404() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "items?select=id,nope").await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn internal_table_is_hidden_as_404() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let (status, body) = rest_get(&server, &root, "__events__").await;
    assert_eq!(status, 404, "{body}");
}

// ── (e) RLS / column-grant parity with /sql ─────────────────────────────────

/// A restricted user with a `current_user`-referencing RLS policy must see
/// EXACTLY the same rows via `/rest/v1` as via `/sql`.
#[tokio::test]
async fn rls_parity_with_sql() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");

    sql(&server, &root, "CREATE TABLE posts (id INT, owner TEXT)").await;
    sql(&server, &root, "CREATE USER alice").await;
    sql(&server, &root, "GRANT SELECT ON posts TO alice").await;
    sql(
        &server,
        &root,
        "INSERT INTO posts (id, owner) VALUES (1, 'alice'), (2, 'root'), (3, 'alice')",
    )
    .await;
    let policy_resp = sql(
        &server,
        &root,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = current_user)",
    )
    .await;
    assert_eq!(policy_resp.0, 200, "{:?}", policy_resp.1);

    let alice = token_for("alice");

    // Via /sql.
    let (sql_status, sql_body) =
        sql(&server, &alice, "SELECT id, owner FROM posts ORDER BY id").await;
    assert_eq!(sql_status, 200, "{sql_body}");
    let sql_rows = sql_body["results"][0]["rows"].as_array().unwrap().clone();

    // Via /rest/v1 — same table, same caller, same (implicit) predicate.
    let (rest_status, rest_body) =
        rest_get(&server, &alice, "posts?select=id,owner&order=id.asc").await;
    assert_eq!(rest_status, 200, "{rest_body}");
    let rest_rows = rows(&rest_body).clone();

    assert_eq!(
        sql_rows.len(),
        2,
        "alice's RLS policy should admit exactly her 2 rows: {sql_rows:?}"
    );
    assert_eq!(
        sql_rows, rest_rows,
        "GET /rest/v1 must see exactly the same rows as POST /sql under RLS"
    );
}

/// A column-restricted user must get the identical `PERMISSION_DENIED`
/// outcome (and identical admitted row set) via `/rest/v1` as via `/sql`.
#[tokio::test]
async fn column_grant_parity_with_sql() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");

    sql(
        &server,
        &root,
        "CREATE TABLE accounts (id INT, email TEXT, secret TEXT)",
    )
    .await;
    sql(&server, &root, "CREATE USER support").await;
    sql(
        &server,
        &root,
        "GRANT SELECT (id, email) ON accounts TO support",
    )
    .await;
    sql(
        &server,
        &root,
        "INSERT INTO accounts (id, email, secret) VALUES (1, 'a@x.com', 'shh')",
    )
    .await;

    let support = token_for("support");

    // Granted columns: both surfaces succeed identically.
    let (sql_status, _) = sql(&server, &support, "SELECT id, email FROM accounts").await;
    assert_eq!(sql_status, 200);
    let (rest_status, rest_body) = rest_get(&server, &support, "accounts?select=id,email").await;
    assert_eq!(rest_status, 200, "{rest_body}");

    // Ungranted column ('secret'): both surfaces deny identically (403).
    let (sql_status, _) = sql(&server, &support, "SELECT secret FROM accounts").await;
    assert_eq!(sql_status, 403);
    let (rest_status, rest_body) = rest_get(&server, &support, "accounts?select=secret").await;
    assert_eq!(rest_status, 403, "{rest_body}");

    // SELECT * requires every column granted: both deny identically.
    let (sql_status, _) = sql(&server, &support, "SELECT * FROM accounts").await;
    assert_eq!(sql_status, 403);
    let (rest_status, rest_body) = rest_get(&server, &support, "accounts").await;
    assert_eq!(rest_status, 403, "{rest_body}");
}

/// A user with no privileges on the table at all gets 403 through
/// `/rest/v1` exactly like `/sql` — table-grant enforcement, not just
/// column-grant, is inherited.
#[tokio::test]
async fn no_table_privilege_is_403_on_rest_too() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");
    sql(&server, &root, "CREATE TABLE t (id INT)").await;
    sql(&server, &root, "CREATE USER bob").await;
    let bob = token_for("bob");

    let (status, body) = rest_get(&server, &bob, "t").await;
    assert_eq!(status, 403, "{body}");

    let (status, body) = rest_post(&server, &bob, "t", json!({"id": 1})).await;
    assert_eq!(status, 403, "{body}");
}

// ── (f) OpenAPI stub (C3) ────────────────────────────────────────────────────

#[tokio::test]
async fn openapi_document_lists_tables() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/rest/v1"))
        .header("Authorization", format!("Bearer {root}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["openapi"], "3.0.3");
    assert!(body["paths"]["/rest/v1/items"].is_object());
    assert!(body["components"]["schemas"]["items"].is_object());
}

// ── (g) C2: embedded resource expansion ─────────────────────────────────────
//
// `orders.customer_id -> customers.id` is the acceptance example from item
// 123 C2: forward (many-to-one) embedding via `customer(...)`, and reverse
// (one-to-many) embedding of `orders` off `customers`.

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
            "INSERT INTO orders (id, customer_id, total) VALUES \
             (100, 1, 10), (101, 1, 20), (102, 2, 30), (103, NULL, 40)"
        )
        .await
        .0,
        200
    );
    root
}

/// (a) forward embed: each order gets its `customer` nested object, resolved
/// via the `orders.customer_id -> customers.id` FK — the exact scenario from
/// item 123 C2's acceptance example. A `NULL` FK value embeds as `null`,
/// never an error or a spurious match.
#[tokio::test]
async fn forward_embed_returns_nested_parent_object() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "orders?select=id,total,customer(id,name)&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["columns"], json!(["id", "total", "customer"]));
    let r = rows(&body);
    assert_eq!(r.len(), 4, "{body}");
    assert_eq!(r[0], json!([100, 10, {"id": 1, "name": "acme"}]));
    assert_eq!(r[1], json!([101, 20, {"id": 1, "name": "acme"}]));
    assert_eq!(r[2], json!([102, 30, {"id": 2, "name": "globex"}]));
    assert_eq!(r[3], json!([103, 40, null]));
}

/// (b) a filter on the base table still applies with an embed present — the
/// embed's hidden join-key column doesn't disturb `WHERE`/`ORDER BY` on the
/// base query, and filtered-out base rows never trigger an embed fetch for
/// them.
#[tokio::test]
async fn filter_on_base_table_works_with_embed() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "orders?select=id,total,customer(id,name)&total=gt.15&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 3, "{body}");
    assert_eq!(r[0], json!([101, 20, {"id": 1, "name": "acme"}]));
    assert_eq!(r[1], json!([102, 30, {"id": 2, "name": "globex"}]));
    // Order 103 (total 40, NULL customer_id) passes the base filter too —
    // its embed is `null`, not an error, and it isn't dropped by the embed
    // machinery.
    assert_eq!(r[2], json!([103, 40, null]));
}

/// Reverse (one-to-many) embed: a customer's `orders` come back as a nested
/// array (empty when it has none).
#[tokio::test]
async fn reverse_embed_returns_nested_array() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "customers?select=id,name,orders(id,total)&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2, "{body}");
    assert_eq!(
        r[0],
        json!([1, "acme", [{"id": 100, "total": 10}, {"id": 101, "total": 20}]])
    );
    assert_eq!(r[1], json!([2, "globex", [{"id": 102, "total": 30}]]));
}

/// (d) an embed name that matches no FK relationship is a 400, not a silent
/// no-op, a 404, or a 500.
#[tokio::test]
async fn unknown_relationship_is_400() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(&server, &root, "orders?select=id,nope(id)").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "UNKNOWN_RELATIONSHIP");
}

/// (d) an embed name that matches more than one FK relationship must be
/// rejected rather than silently picking one — two FK columns on the same
/// table both targeting `customers` make the bare table name ambiguous; the
/// individually-disambiguating `_id`-stripped aliases still work.
#[tokio::test]
async fn ambiguous_relationship_is_400() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE orders (id INT PRIMARY KEY, buyer_id INT REFERENCES customers(id), \
             seller_id INT REFERENCES customers(id))"
        )
        .await
        .0,
        200
    );

    let (status, body) = rest_get(&server, &root, "orders?select=id,customers(id)").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "AMBIGUOUS_RELATIONSHIP");

    // The unambiguous `_id`-stripped aliases still resolve individually.
    let (status, body) = rest_get(&server, &root, "orders?select=id,buyer(id)").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = rest_get(&server, &root, "orders?select=id,seller(id)").await;
    assert_eq!(status, 200, "{body}");
}

/// (c) RLS + column-grant parity on the EMBEDDED table: a restricted caller
/// only sees embedded parent rows they could `SELECT` directly (RLS-hidden
/// rows nest as `null`, never leak), and requesting an embedded column
/// they aren't granted is denied exactly like a direct `/rest/v1` request
/// for that column would be — fail closed, not partial data.
#[tokio::test]
async fn embed_honors_rls_and_column_grants_on_embedded_table() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");

    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, owner TEXT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
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
            &server,
            &root,
            "INSERT INTO customers (id, name, owner) VALUES \
             (1, 'acme', 'alice'), (2, 'globex', 'root')"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO orders (id, customer_id, total) VALUES (100, 1, 10), (101, 2, 20)"
        )
        .await
        .0,
        200
    );

    sql(&server, &root, "CREATE USER alice").await;
    sql(&server, &root, "GRANT SELECT ON orders TO alice").await;
    sql(
        &server,
        &root,
        "GRANT SELECT (id, name) ON customers TO alice",
    )
    .await;
    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON customers FOR SELECT USING (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    let alice = token_for("alice");

    // RLS: alice can see order 100's customer (hers) but not order 101's
    // (root's) — the latter must come back `null`, never leak the row or
    // error the whole request.
    let (status, body) = rest_get(
        &server,
        &alice,
        "orders?select=id,customer(id,name)&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2, "{body}");
    assert_eq!(r[0], json!([100, {"id": 1, "name": "acme"}]));
    assert_eq!(
        r[1],
        json!([101, null]),
        "alice's RLS policy must hide the other customer's row, even nested: {body}"
    );

    // Column grant: alice is not granted `owner` on customers, so an embed
    // asking for it is denied exactly like a top-level `/rest/v1` request
    // for that column would be.
    let (status, body) = rest_get(&server, &alice, "orders?select=id,customer(id,owner)").await;
    assert_eq!(status, 403, "{body}");

    // Same comparison, requested directly on `/rest/v1/customers`: identical
    // denial.
    let (direct_status, direct_body) = rest_get(&server, &alice, "customers?select=id,owner").await;
    assert_eq!(direct_status, status, "{direct_body}");
}

/// (e) a malicious value combined with a `select=...,name(cols)` embed is
/// still bound purely as data, and a malicious *relationship name* is only
/// ever matched against catalog FK metadata — never built into SQL text —
/// so it just fails to resolve rather than reaching the database.
#[tokio::test]
async fn injection_attempt_with_embed_present_is_treated_as_data() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    // TEXT-column filter value (comparable against any text, unlike an INT
    // column where a non-numeric malicious string would fail type coercion
    // rather than testing the injection-safety property).
    let malicious = "1) OR (1=1";
    let path = format!(
        "customers?select=id,name,orders(id,total)&name=eq.{}",
        urlencoding_percent(malicious)
    );
    let (status, body) = rest_get(&server, &root, &path).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        rows(&body).len(),
        0,
        "malicious filter value must match no row, not leak the table: {body}"
    );

    // A DROP-TABLE-shaped string as the relationship *name* itself: matched
    // against catalog FK metadata only, so it's simply unresolved (400) —
    // never string-built into SQL.
    let malicious_name = "customers'; DROP TABLE customers; --";
    let path = format!(
        "orders?select=id,{}(id)",
        urlencoding_percent(malicious_name)
    );
    let (status, body) = rest_get(&server, &root, &path).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "UNKNOWN_RELATIONSHIP");

    // Both tables must still exist and hold their original rows — no DROP
    // TABLE ever executed.
    let (status, body) = rest_get(&server, &root, "orders?select=id").await;
    assert_eq!(status, 200, "orders must survive: {body}");
    assert_eq!(rows(&body).len(), 4);
    let (status, body) = rest_get(&server, &root, "customers?select=id").await;
    assert_eq!(status, 200, "customers must survive: {body}");
    assert_eq!(rows(&body).len(), 2);
}
