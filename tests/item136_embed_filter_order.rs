#![cfg(feature = "server")]
//! Item 136: `/rest/v1` embedded-resource filtering + ordering — dotted
//! `<embed>.<col>=<op>.<val>` / `<embed>.order=...` / `<embed>.limit=...` /
//! `<embed>.offset=...` query params, threaded into the embed's second
//! query ([`unidb::server::rest_resource::fetch_embedded`] internally) and
//! sliced **per-parent** (lateral semantics) in Rust after stitching.
//!
//! Builds on item 123 C2's embedding (`tests/server_rest.rs`'s `orders.
//! customer_id -> customers.id` fixture) without changing any of its
//! existing behavior — verified unchanged in that file separately.

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

/// `rows` field of a `{"type":"rows", "columns":[...], "rows":[[...]]}` body.
fn rows(body: &Value) -> &Vec<Value> {
    body["rows"].as_array().unwrap()
}

/// `customers(id, name)` / `orders(id, customer_id -> customers.id, total)`,
/// several orders per customer so per-parent filter/order/limit is
/// meaningfully exercised (item 123 C2's fixture, extended with more rows).
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
             (100, 1, 50), (101, 1, 120), (102, 1, 300), (103, 1, 150), (104, 1, 200), \
             (105, 2, 400), (106, 2, 90), (107, 2, 500)"
        )
        .await
        .0,
        200
    );
    root
}

/// The item 136 acceptance example: per customer, only orders with
/// `total > 100`, ordered by `total desc`, at most 3.
///
/// Customer 1's orders > 100: 120, 300, 150, 200 -> desc: 300, 200, 150, 120
/// -> limit 3: 300, 200, 150.
/// Customer 2's orders > 100: 400, 500 -> desc: 500, 400 (no truncation
/// needed, but still must respect the per-customer cap independently of
/// customer 1's count).
#[tokio::test]
async fn embed_filter_order_limit_applied_per_parent() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "customers?select=id,orders(id,total)&orders.total=gt.100&orders.order=total.desc&\
         orders.limit=3&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2, "{body}");

    let c1_orders = r[0][1].as_array().unwrap();
    assert_eq!(
        c1_orders,
        &vec![
            json!({"id": 102, "total": 300}),
            json!({"id": 104, "total": 200}),
            json!({"id": 103, "total": 150}),
        ],
        "customer 1 must get only its own top-3 orders >100, desc by total: {body}"
    );

    let c2_orders = r[1][1].as_array().unwrap();
    assert_eq!(
        c2_orders,
        &vec![
            json!({"id": 107, "total": 500}),
            json!({"id": 105, "total": 400}),
        ],
        "customer 2 must independently see its own (unclipped, only 2) orders >100, desc: {body}"
    );
}

/// `<embed>.offset` + `<embed>.limit` together slice each parent's group
/// `offset..offset+limit` in the requested order — proving this is a real
/// per-parent lateral slice, not a single combined SQL `LIMIT`/`OFFSET` on
/// the underlying `IN (...)` query (which would wrongly cap/skip across all
/// parents' rows combined rather than resetting per parent).
#[tokio::test]
async fn embed_offset_and_limit_are_per_parent_not_global() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "customers?select=id,orders(id,total)&orders.order=total.desc&orders.limit=1&\
         orders.offset=1&order=id.asc",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2, "{body}");

    // Customer 1, all orders desc by total: 300,200,150,120,50 -> offset 1,
    // limit 1 -> [200]. If the cap were global (applied once to the
    // combined IN-query instead of per parent), customer 1's second-highest
    // order would be lost or customer 2 would see nothing at all.
    assert_eq!(r[0][1], json!([{"id": 104, "total": 200}]), "{body}");
    // Customer 2, desc by total: 500,400,90 -> offset 1, limit 1 -> [400].
    assert_eq!(r[1][1], json!([{"id": 105, "total": 400}]), "{body}");
}

/// A per-embed filter on a column the caller isn't granted on the embedded
/// table is denied identically to a direct `GET` on that table filtering by
/// the same column — the embedded query runs through the exact same
/// enforced `authorize_sql_as_principal` path, so this is inherited
/// enforcement, not a new path. Same for `<embed>.order`.
#[tokio::test]
async fn embed_filter_and_order_on_ungranted_column_denied_like_direct_get() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    sql(&server, &admin, "CREATE USER root SUPERUSER").await;
    let root = token_for("root");

    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, secret TEXT)"
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
            "INSERT INTO customers (id, name, secret) VALUES (1, 'acme', 'shh')"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO orders (id, customer_id, total) VALUES (100, 1, 10)"
        )
        .await
        .0,
        200
    );

    sql(&server, &root, "CREATE USER alice").await;
    sql(&server, &root, "GRANT SELECT ON orders TO alice").await;
    // alice is granted `id`/`name` on customers, but NOT `secret`.
    sql(
        &server,
        &root,
        "GRANT SELECT (id, name) ON customers TO alice",
    )
    .await;
    let alice = token_for("alice");

    // Sanity: alice can otherwise embed customers fine.
    let (status, body) = rest_get(&server, &alice, "orders?select=id,customer(id,name)").await;
    assert_eq!(status, 200, "{body}");

    // --- filter parity ---
    let (embed_filter_status, embed_filter_body) = rest_get(
        &server,
        &alice,
        "orders?select=id,customer(id,name)&customer.secret=eq.shh",
    )
    .await;
    let (direct_filter_status, direct_filter_body) =
        rest_get(&server, &alice, "customers?secret=eq.shh").await;
    assert_eq!(
        embed_filter_status, direct_filter_status,
        "embed filter: {embed_filter_body}, direct: {direct_filter_body}"
    );
    assert_eq!(
        embed_filter_status, 403,
        "an ungranted-column filter must be denied, not silently applied/ignored: {embed_filter_body}"
    );

    // --- order parity ---
    let (embed_order_status, embed_order_body) = rest_get(
        &server,
        &alice,
        "orders?select=id,customer(id,name)&customer.order=secret.desc",
    )
    .await;
    let (direct_order_status, direct_order_body) =
        rest_get(&server, &alice, "customers?order=secret.desc").await;
    assert_eq!(
        embed_order_status, direct_order_status,
        "embed order: {embed_order_body}, direct: {direct_order_body}"
    );
    assert_eq!(
        embed_order_status, 403,
        "an ungranted-column order must be denied, not silently applied/ignored: {embed_order_body}"
    );

    // Root (superuser, full grants) can still filter/order the embed by
    // `secret` — proves the denial above is grant-driven, not a blanket
    // rejection of the `secret` column name.
    let (status, body) = rest_get(
        &server,
        &root,
        "orders?select=id,customer(id,name)&customer.secret=eq.shh",
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// A dotted param whose prefix does not name an embedded relation actually
/// present in `select=` is a clear 400 (`UNKNOWN_EMBED_PARAM`), never a
/// silently ignored no-op — whether the prefix names no relation at all, or
/// names a real FK relation that simply wasn't embedded on this request.
#[tokio::test]
async fn unknown_embed_prefix_is_400() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    // "bogus" doesn't correspond to any FK relationship at all.
    let (status, body) = rest_get(
        &server,
        &root,
        "customers?select=id,orders(id,total)&bogus.foo=eq.1",
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "UNKNOWN_EMBED_PARAM");

    // "orders" IS a real relationship, but this request's `select=` doesn't
    // embed it — the dotted param must still be rejected, not silently
    // dropped.
    let (status, body) = rest_get(&server, &root, "customers?select=id&orders.total=gt.100").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "UNKNOWN_EMBED_PARAM");

    // Baseline: with the embed actually present, the same param is valid.
    let (status, body) = rest_get(
        &server,
        &root,
        "customers?select=id,orders(id,total)&orders.total=gt.100",
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// Every pre-existing (non-dotted-param) embed request keeps working
/// unchanged: forward embed with a base-table filter present, no dotted
/// params at all.
#[tokio::test]
async fn plain_embed_without_dotted_params_still_works() {
    let server = TestServer::spawn().await;
    let root = setup_customers_orders(&server).await;

    let (status, body) = rest_get(
        &server,
        &root,
        "orders?select=id,total,customer(id,name)&order=id.asc&limit=2",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r = rows(&body);
    assert_eq!(r.len(), 2, "{body}");
    assert_eq!(r[0], json!([100, 50, {"id": 1, "name": "acme"}]));
    assert_eq!(r[1], json!([101, 120, {"id": 1, "name": "acme"}]));
}
