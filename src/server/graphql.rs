//! Item 123 (Workstream C4): a schema-derived GraphQL endpoint (`POST
//! /graphql`) — the `pg_graphql` analog, except this one also exposes
//! **graph edge traversal** and **vector similarity** as first-class
//! fields, not just FK relationships. That is the whole point of building
//! it: unidb's differentiator is relational + graph + vector in one engine,
//! and GraphQL's nested-query model is the natural surface for that.
//!
//! **Schema-generation approach:** `async_graphql::dynamic` — the schema is
//! rebuilt from the live catalog on every request (`Engine::table_defs()`),
//! so it always reflects whatever tables/columns/FKs currently exist. One
//! GraphQL `Object` type per table; a scalar field per column; a root query
//! field per table with the same filter-operator matrix as `/rest/v1`
//! (`<col>`/`<col>_neq`/`_gt`/`_gte`/`_lt`/`_lte`/`_like`/`_ilike`/`_in`/
//! `_is_null`) plus `orderBy`/`limit`/`offset`; forward (many-to-one) and
//! reverse (one-to-many) FK relationship fields resolved from catalog FK
//! metadata (the same pattern [`super::rest_resource::resolve_relation`]
//! uses for C2 embedding, generalized here to enumerate *every*
//! relationship up front instead of resolving one name at a time); an
//! `edges(type, direction)` field on any table with a single `Int64`
//! primary key, returning `__edges__` rows (reuses the graph engine's
//! storage, not a new traversal path); and a root `Query.near_<table>`/
//! `near_<table>_<col>(vector, k)` field for any table with a `VECTOR`
//! column, which is just `WHERE NEAR(...)` — the exact same predicate the
//! executor already knows how to run (`sql/executor.rs::exec_select_near`).
//! `near` is deliberately a root field, not nested under an already-fetched
//! row, since a similarity search has no row to be relative to (see
//! [`near_field_names`]'s doc comment) — unlike `edges`, which *is*
//! meaningfully relative to one row's primary key.
//!
//! **The one rule that makes every field enforced identically to `/sql`:**
//! this module never runs an unenforced executor call. Every single field
//! resolver — root query, FK forward/reverse, `edges`, `near` — is built by
//! generating a parameterized SQL statement and executing it through
//! [`super::rest_resource::run_stmt`], the exact same
//! `authorize_sql_as_principal` (table/column-grant pre-check) +
//! `execute_sql_params_as_principal` (RLS + bind + execute under the
//! caller's [`crate::AuthPrincipal`]) path `POST /sql` and `/rest/v1` use.
//! `run_stmt`/`append_where`/`ParsedOp`/`Filter`/`quote_ident`/
//! `table_ident`/`validate_column`/`parse_order_value` are all reused
//! directly from `rest_resource.rs` (made `pub(super)` for this purpose) —
//! this module adds no parallel policy/enforcement engine. A row or column
//! the caller's RLS policy or table/column grants don't admit therefore
//! comes back `null`/omitted/an error exactly as it would over `/sql` or
//! `/rest/v1` — see `tests/server_graphql.rs`'s `rls_grant_parity_with_sql`
//! for the proof.
//!
//! **Graph edges specifically:** `Engine::edges_from` (used by `GET
//! /edges/from/:id`) has no RLS/grant check at all — it is a raw MVCC
//! traversal gated only by "does the caller hold *a* valid JWT," not by any
//! table grant. That is fine for that pre-existing, narrowly-scoped route,
//! but would violate this module's fail-closed requirement for a
//! general-purpose graph-traversal field. Since `__edges__` is an ordinary
//! catalog table under the hood (`graph::edges::edges_table_def`), this
//! module's `edges` field instead runs `SELECT ... FROM __edges__ WHERE
//! from_id = $1` through the same enforced `run_stmt` path as everything
//! else — a caller needs an explicit `GRANT SELECT ON __edges__` (grants
//! fail closed by default; see `authz::AuthzStore::has_privilege`) to
//! traverse edges at all, and any RLS policy an admin attaches to
//! `__edges__` applies too. This is *stronger* enforcement than the
//! existing `/edges/from/:id` route, not a regression.
//!
//! **Vector `near`:** `NEAR(column, [f...], k)`'s vector/`k` arguments must
//! be SQL literals, not bind params (`sql/parser.rs::convert_near` — the
//! grammar requires an array literal and an integer literal in those
//! positions). They are therefore formatted directly into the generated SQL
//! text here, exactly like `rest_resource.rs` already does for `LIMIT`/
//! `OFFSET` (see that module's doc comment) — safe because both come from
//! GraphQL's own typed `[Float!]!`/`Int!` argument parsing (a `Float`/`Int`
//! scalar coerces to a Rust `f32`/`i64` before it ever reaches `format!`;
//! there is no code path from a client-supplied string into this SQL text).
//! A non-finite float (`NaN`/`Infinity`) is rejected before formatting.
//!
//! **Deferred (v1 scope, noted per the C4 backlog spec):** subscriptions,
//! aggregations, cursor-based pagination, and combining `near`/`edges`
//! traversal with the root field's filter/order/limit machinery. Per-field
//! resolution can also N+1 (one enforced statement per resolved field/row) —
//! acceptable for a v1 whose correctness anchor is "every field goes through
//! the real enforced path," per this workstream's explicit scope note.
//!
//! **Item 133 — mutations:** a `Mutation` root (`insert_<t>`/`update_<t>`/
//! `delete_<t>` per eligible table) was added on top of the read-only v1
//! above, following the exact same one rule: every mutation resolver builds
//! a parameterized `INSERT`/`UPDATE`/`DELETE ... RETURNING <requested
//! sub-fields>` statement and runs it through [`run_stmt`]/[`run_stmts`] —
//! the identical `authorize_sql_as_principal` + `execute_sql_params_as_principal`
//! path the read side and `/rest/v1`/`/sql` already use. `RETURNING`'s
//! column list is authorized exactly like a `SELECT` projection
//! (`Engine::check_returning` in `lib.rs`), so an ungranted column in a
//! mutation's selection set is denied identically to requesting it via
//! `/sql`'s own `RETURNING` clause — no new enforcement logic, no new write
//! path. See [`build_insert_field`]/[`build_update_field`]/
//! [`build_delete_field`] and their doc comments.

use std::collections::{HashMap, HashSet};

use async_graphql::{
    dynamic::{
        Enum, EnumItem, Field, FieldFuture, FieldValue, InputValue, Object, Scalar, Schema,
        TypeRef, ValueAccessor,
    },
    Value as GqlValue,
};
use axum::{extract::State, Extension, Json};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    catalog::{ColumnDef, ColumnType, TableDef},
    graph::edges::EDGES_TABLE,
    sql::{executor::ExecResult, logical::Literal},
    AuthPrincipal,
};

use super::{
    dto,
    error::{map_status, ApiError},
    rest_resource::{
        append_where, build_assignments, extract_single_in, parse_order_value, quote_ident,
        run_stmt, run_stmts, strip_id_suffix, table_ident, validate_column, Filter, InFilter,
        ParsedOp, Relation,
    },
    AppState,
};

/// `POST /graphql` — mounted under the same `require_jwt` layer as every
/// other data-plane route (`router.rs`), so `Extension<AuthPrincipal>` is
/// already populated by the time this handler runs, exactly like
/// `rest_resource`'s handlers. Body is a standard GraphQL-over-HTTP request
/// (`{query, variables?, operationName?}` — `async_graphql::Request`
/// already deserializes exactly that shape); response is the standard
/// `{data, errors}` GraphQL JSON shape (`async_graphql::Response`'s own
/// `Serialize` impl) — always HTTP 200, since a GraphQL-level error (bad
/// query, denied field, ...) is reported *inside* the body per the GraphQL
/// spec, not via HTTP status.
pub async fn post_graphql(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Json(request): Json<async_graphql::Request>,
) -> Json<JsonValue> {
    let schema = match build_schema(&state, &principal).await {
        Ok(s) => s,
        Err(e) => {
            let gql_err = api_err_to_gql(e);
            let resp = async_graphql::Response::from_errors(vec![async_graphql::ServerError::new(
                gql_err.message,
                None,
            )]);
            return Json(serde_json::to_value(&resp).unwrap_or_else(|_| {
                serde_json::json!({"data": null, "errors": [{"message": "internal: failed to build GraphQL schema"}]})
            }));
        }
    };

    let response = schema.execute(request).await;
    Json(serde_json::to_value(&response).unwrap_or_else(|_| {
        serde_json::json!({"data": null, "errors": [{"message": "internal: failed to serialize GraphQL response"}]})
    }))
}

// ── schema construction (catalog-derived, rebuilt every request) ───────────

/// Names this module reserves for its own synthetic schema surface — never
/// eligible as a table's GraphQL type name (a table literally named `Query`
/// would otherwise silently clobber the root type, since
/// `SchemaBuilder::register` last-write-wins on a name collision rather
/// than erroring).
fn is_reserved_type_name(name: &str) -> bool {
    matches!(
        name,
        "Query"
            | "Mutation"
            | "Subscription"
            | "JSON"
            | "Edge"
            | "EdgeDirection"
            | "Int"
            | "Float"
            | "String"
            | "Boolean"
            | "ID"
            | "Upload"
    )
}

/// A GraphQL `Name` must match `/[_A-Za-z][_0-9A-Za-z]*/`. A table whose
/// name doesn't qualify (unusual, but the SQL layer allows more than
/// GraphQL does) is simply left out of the generated schema rather than
/// producing an invalid one.
fn is_valid_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Catalog knowledge every field resolver needs to build a *projected*
/// (not blanket `SELECT *`) enforced query — carried as schema [`Data`][1]
/// (`ctx.data::<Arc<SchemaCatalog>>()`) alongside `AppState`/`AuthPrincipal`
/// rather than threaded through every closure capture. Keyed by table name
/// so a nested resolver (FK forward/reverse, `near`) can look up *its
/// target table's* columns/relations, not just its own.
///
/// **Why projection matters for grant parity, not just efficiency:** a
/// column grant is per-column (item 112) — `GRANT SELECT (id, email) ON
/// accounts` must let `{ accounts { id email } }` succeed even though the
/// caller holds no grant on `accounts.secret`. Resolving every field via an
/// unconditional `SELECT *` would require every column to be granted
/// regardless of what the GraphQL query actually asked for, which is
/// *stricter* than `/rest/v1`/`/sql` (`SELECT id, email FROM accounts`
/// succeeds there) — a parity violation, not merely a performance loss. See
/// [`requested_projection`].
///
/// [1]: async_graphql::dynamic::SchemaBuilder::data
struct SchemaCatalog {
    defs: HashMap<String, TableDef>,
    relations: HashMap<String, Vec<(String, Relation)>>,
}

/// Build a fresh catalog-derived schema for this one request, carrying
/// `state`/`principal`/a [`SchemaCatalog`] as schema `Data` so every
/// resolver can reach [`run_stmt`] under the caller's identity via
/// `ctx.data::<AppState>()`/`ctx.data::<AuthPrincipal>()` and build a
/// correctly-projected query via `ctx.data::<Arc<SchemaCatalog>>()`.
/// Rebuilding per request means the schema always reflects the live
/// catalog (a table created/dropped mid-session is visible on the very
/// next query) at the cost of doing the (cheap, in-memory) type-assembly
/// work every time — acceptable for v1 per this module's doc comment.
async fn build_schema(state: &AppState, principal: &AuthPrincipal) -> Result<Schema, ApiError> {
    let all_defs = state.engine.table_defs().await?;
    let eligible_defs: Vec<TableDef> = all_defs
        .iter()
        .filter(|d| {
            !dto::is_internal_table(&d.name)
                && is_valid_graphql_name(&d.name)
                && !is_reserved_type_name(&d.name)
        })
        .cloned()
        .collect();
    let eligible: HashMap<String, TableDef> = eligible_defs
        .iter()
        .map(|d| (d.name.clone(), d.clone()))
        .collect();

    let mut relations_by_table: HashMap<String, Vec<(String, Relation)>> = HashMap::new();
    let mut query = Object::new("Query");
    // Item 133: a `Mutation` root is only registered when at least one table
    // is eligible — an empty `Object` (zero fields) is an invalid GraphQL
    // type, so a schema with no eligible tables stays query-only, exactly as
    // it was pre-133.
    let mut mutation = Object::new("Mutation");
    let has_mutation_fields = !eligible_defs.is_empty();
    let mut builder = Schema::build("Query", has_mutation_fields.then_some("Mutation"), None)
        .register(Scalar::new("JSON"))
        .register(build_edge_type())
        .register(build_edge_direction_enum())
        .data(state.clone())
        .data(principal.clone());

    for def in &eligible_defs {
        let relations = enumerate_relations(&all_defs, def);
        builder = builder.register(build_table_object(def, &eligible, &relations));
        query = query.field(build_query_field(def));
        for (field_name, column) in near_field_names(def) {
            query = query.field(build_near_field(&field_name, &def.name, &def.name, &column));
        }
        mutation = mutation
            .field(build_insert_field(def))
            .field(build_update_field(def))
            .field(build_delete_field(def));
        relations_by_table.insert(def.name.clone(), relations);
    }
    builder = builder.register(query);
    if has_mutation_fields {
        builder = builder.register(mutation);
    }
    builder = builder.data(std::sync::Arc::new(SchemaCatalog {
        defs: eligible,
        relations: relations_by_table,
    }));

    builder
        .finish()
        .map_err(|e| ApiError::internal("GRAPHQL_SCHEMA_ERROR", e.to_string()))
}

/// The columns to `SELECT` for `def` given the GraphQL sub-fields actually
/// requested (`selected`) — the analog of `/rest/v1`'s `select=col,col2`
/// (item 123 C1/C2), derived instead from the query's own selection set. A
/// requested scalar column is projected directly; a requested relationship
/// field (`relations`) or `edges` field pulls in only the join/PK column it
/// needs to resolve, not the related table's columns (those are a separate
/// enforced query, see [`build_forward_field`]/[`build_reverse_field`]/
/// [`build_edges_field`]). Falls back to `*` when nothing recognizable was
/// requested (e.g. only meta fields like `__typename`) or the selection
/// can't be inspected for some reason — matching `/rest/v1`'s own "no
/// `select=` -> `*`" default, never silently dropping data.
fn requested_projection<'a>(
    selected: impl Iterator<Item = async_graphql::SelectionField<'a>>,
    def: &TableDef,
    relations: &[(String, Relation)],
) -> String {
    let column_names: HashSet<&str> = def
        .columns
        .iter()
        .filter(|c| !c.dropped)
        .map(|c| c.name.as_str())
        .collect();
    let pk_name = single_int_pk(def).map(|c| c.name.clone());

    let mut cols: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut saw_any = false;
    let mut fallback_to_star = false;

    for field in selected {
        let name = field.name();
        saw_any = true;
        if name.starts_with("__") {
            continue; // introspection meta field (`__typename`) — no column needed.
        }
        if column_names.contains(name) {
            if seen.insert(name.to_string()) {
                cols.push(name.to_string());
            }
            continue;
        }
        if let Some((_, rel)) = relations.iter().find(|(alias, _)| alias == name) {
            let needed = match rel {
                Relation::Forward { fk_column, .. } => fk_column.clone(),
                Relation::Reverse { ref_column, .. } => ref_column.clone(),
            };
            if seen.insert(needed.clone()) {
                cols.push(needed);
            }
            continue;
        }
        if name == "edges" {
            if let Some(pk) = &pk_name {
                if seen.insert(pk.clone()) {
                    cols.push(pk.clone());
                }
            }
            continue;
        }
        // Shouldn't happen for a schema-validated query (every field name
        // was either a real column, a known relation alias, or `edges`) —
        // fail safe rather than silently omit a column this resolver
        // doesn't recognize.
        fallback_to_star = true;
    }

    if fallback_to_star || !saw_any || cols.is_empty() {
        "*".to_string()
    } else {
        cols.iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// [`requested_projection`] looked up by table name against a
/// [`SchemaCatalog`] — the shape every non-root resolver (FK forward/
/// reverse, `near`) needs, since their target table differs from the
/// resolver's own defining type. Falls back to `*` for a table the catalog
/// doesn't recognize (shouldn't happen — every target here came from the
/// catalog in the first place — but fails safe rather than erroring the
/// whole field).
fn projection_for<'a>(
    selected: impl Iterator<Item = async_graphql::SelectionField<'a>>,
    catalog: &SchemaCatalog,
    table: &str,
) -> String {
    match catalog.defs.get(table) {
        Some(def) => {
            let relations = catalog
                .relations
                .get(table)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            requested_projection(selected, def, relations)
        }
        None => "*".to_string(),
    }
}

/// One catalog-derived GraphQL `Object` type for `def`: a scalar field per
/// column, forward/reverse FK relationship fields (only when the
/// target/child table is itself eligible), and an `edges` field when `def`
/// has a single `Int64` primary key. (`near`/`near_<col>` vector-search
/// fields are *not* added here — see [`near_field_names`]'s doc comment for
/// why they are root `Query` fields instead of per-row fields.) `used`
/// tracks claimed field names so a pathological schema (an alias colliding
/// with a real column name, say) degrades by skipping the field rather than
/// panicking — `Object::field` panics on a duplicate name, which this
/// function never risks calling.
fn build_table_object(
    def: &TableDef,
    eligible: &HashMap<String, TableDef>,
    relations: &[(String, Relation)],
) -> Object {
    let mut obj = Object::new(def.name.clone());
    let mut used: HashSet<String> = HashSet::new();

    for col in def.columns.iter().filter(|c| !c.dropped) {
        if used.insert(col.name.clone()) {
            obj = obj.field(build_scalar_field(col));
        }
    }

    for (alias, rel) in relations {
        if used.contains(alias) {
            continue;
        }
        match rel {
            Relation::Forward {
                fk_column,
                ref_table,
                ref_column,
            } => {
                if eligible.contains_key(ref_table) {
                    obj = obj.field(build_forward_field(alias, fk_column, ref_table, ref_column));
                    used.insert(alias.clone());
                }
            }
            Relation::Reverse {
                child_table,
                fk_column,
                ref_column,
            } => {
                if eligible.contains_key(child_table) {
                    obj = obj.field(build_reverse_field(
                        alias,
                        child_table,
                        fk_column,
                        ref_column,
                    ));
                    used.insert(alias.clone());
                }
            }
        }
    }

    if let Some(pk) = single_int_pk(def) {
        if used.insert("edges".to_string()) {
            obj = obj.field(build_edges_field(&pk.name));
        }
    }

    obj
}

/// The `VECTOR` columns of `def` paired with the root `Query` field name
/// each gets: `near_<table>` when `def` has exactly one vector column,
/// else `near_<table>_<col>` per column (deterministic, collision-free —
/// table names are already unique in the catalog). **Root-level, not a
/// per-row field on `def`'s own object type**: unlike `edges` (which is
/// meaningfully relative to one row's primary key — "edges out of *this*
/// node"), a `NEAR` search has no row to be relative to; it is a search
/// over the whole table, exactly like the plain `<table>(...)` list field
/// it sits beside. Nesting it under an already-fetched row would force a
/// client to fetch some unrelated row first just to reach a field that
/// then ignores it — root-level is both the semantically correct shape and
/// the more usable one.
fn near_field_names(def: &TableDef) -> Vec<(String, String)> {
    let vector_cols: Vec<&ColumnDef> = def
        .columns
        .iter()
        .filter(|c| !c.dropped && matches!(c.ty, ColumnType::Vector(_)))
        .collect();
    if vector_cols.len() == 1 {
        vec![(format!("near_{}", def.name), vector_cols[0].name.clone())]
    } else {
        vector_cols
            .into_iter()
            .map(|c| (format!("near_{}_{}", def.name, c.name), c.name.clone()))
            .collect()
    }
}

// ── FK relationship enumeration (schema-time; cf. C2's resolve_relation) ───

/// Every embeddable relationship touching `base`, aliased for use as a
/// GraphQL field name. Mirrors [`super::rest_resource::resolve_relation`]'s
/// candidate-gathering (same catalog-only FK walk, same `_id`-suffix-stripped
/// aliasing via [`strip_id_suffix`]), generalized to enumerate *every*
/// relationship up front (schema construction needs the whole set, not a
/// single named lookup) with a disambiguation pass instead of C2's "ambiguous
/// -> reject" (a GraphQL schema has no per-request opportunity to 400 on an
/// ambiguous name — it must pick two distinct field names once, at schema
/// build time).
fn enumerate_relations(all_defs: &[TableDef], base: &TableDef) -> Vec<(String, Relation)> {
    let mut forward: Vec<(String, Relation)> = Vec::new();
    for col in base.columns.iter().filter(|c| !c.dropped) {
        if let Some(fk) = &col.constraints.references {
            if let Some(ref_def) = all_defs.iter().find(|d| d.name == fk.table) {
                if let Ok(ref_col) =
                    crate::sql::executor::resolve_fk_ref_col(ref_def, fk.column.as_deref())
                {
                    let alias = strip_id_suffix(&col.name)
                        .map(str::to_string)
                        .unwrap_or_else(|| col.name.clone());
                    forward.push((
                        alias,
                        Relation::Forward {
                            fk_column: col.name.clone(),
                            ref_table: fk.table.clone(),
                            ref_column: ref_col.name.clone(),
                        },
                    ));
                }
            }
        }
    }
    for fk in &base.constraints.foreign_keys {
        if fk.columns.len() != 1 {
            continue; // composite FKs are out of scope, same as C2 (item 123).
        }
        let col_name = &fk.columns[0];
        if let Some(ref_col) = fk.ref_columns.first() {
            let alias = strip_id_suffix(col_name)
                .map(str::to_string)
                .unwrap_or_else(|| col_name.clone());
            forward.push((
                alias,
                Relation::Forward {
                    fk_column: col_name.clone(),
                    ref_table: fk.ref_table.clone(),
                    ref_column: ref_col.clone(),
                },
            ));
        }
    }
    disambiguate(&mut forward, |rel, suffix| {
        if let Relation::Forward { fk_column, .. } = rel {
            format!("{suffix}_{fk_column}")
        } else {
            suffix.to_string()
        }
    });

    let mut reverse: Vec<(String, Relation)> = Vec::new();
    for child in all_defs.iter().filter(|d| !dto::is_internal_table(&d.name)) {
        for col in child.columns.iter().filter(|c| !c.dropped) {
            if let Some(fk) = &col.constraints.references {
                if fk.table == base.name {
                    if let Ok(ref_col) =
                        crate::sql::executor::resolve_fk_ref_col(base, fk.column.as_deref())
                    {
                        reverse.push((
                            child.name.clone(),
                            Relation::Reverse {
                                child_table: child.name.clone(),
                                fk_column: col.name.clone(),
                                ref_column: ref_col.name.clone(),
                            },
                        ));
                    }
                }
            }
        }
        for fk in &child.constraints.foreign_keys {
            if fk.columns.len() != 1 || fk.ref_table != base.name {
                continue;
            }
            if let Some(ref_col) = fk.ref_columns.first() {
                reverse.push((
                    child.name.clone(),
                    Relation::Reverse {
                        child_table: child.name.clone(),
                        fk_column: fk.columns[0].clone(),
                        ref_column: ref_col.clone(),
                    },
                ));
            }
        }
    }
    disambiguate(&mut reverse, |rel, suffix| {
        if let Relation::Reverse { fk_column, .. } = rel {
            let stripped = strip_id_suffix(fk_column).unwrap_or(fk_column.as_str());
            format!("{suffix}_by_{stripped}")
        } else {
            suffix.to_string()
        }
    });

    forward.into_iter().chain(reverse).collect()
}

/// Rename every alias that collides with another entry in `items`, via
/// `rename(relation, original_alias) -> new_alias`. A schema has exactly one
/// chance to name each field, so — unlike C2's per-request "ambiguous ->
/// 400" — this always produces a (fk-column-qualified) name rather than
/// dropping the relationship.
fn disambiguate(items: &mut [(String, Relation)], rename: impl Fn(&Relation, &str) -> String) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (alias, _) in items.iter() {
        *counts.entry(alias.clone()).or_insert(0) += 1;
    }
    for (alias, rel) in items.iter_mut() {
        if counts.get(alias.as_str()).copied().unwrap_or(0) > 1 {
            *alias = rename(rel, alias);
        }
    }
}

/// The single `Int64`-typed primary-key column of `def`, if it has exactly
/// one (column-level `PRIMARY KEY` or a single-column table-level `PRIMARY
/// KEY (col)`) — the shape graph edges' `from_id`/`to_id` (both `i64`) can
/// key against. Multi-column PKs, non-`Int64` PKs, or no PK at all: no
/// `edges` field is generated for that type (catalog-derived opt-out, not a
/// per-table special case).
fn single_int_pk(def: &TableDef) -> Option<&ColumnDef> {
    let col_level: Vec<&ColumnDef> = def
        .columns
        .iter()
        .filter(|c| !c.dropped && c.constraints.primary_key)
        .collect();
    if col_level.len() == 1 {
        return matches!(col_level[0].ty, ColumnType::Int64).then_some(col_level[0]);
    }
    if def.constraints.primary_key.len() == 1 {
        let name = &def.constraints.primary_key[0];
        return def
            .columns
            .iter()
            .find(|c| !c.dropped && &c.name == name && matches!(c.ty, ColumnType::Int64));
    }
    None
}

// ── type mapping ─────────────────────────────────────────────────────────

/// A column's GraphQL output type. Every non-`VECTOR` scalar maps onto one
/// of GraphQL's four built-in scalars (dynamic-schema's `Int`/`Float`/
/// `Boolean`/`String` carry no compile-time range validator — see this
/// module's tests — so `Int64`'s full `i64` range and `Decimal`/`Timestamp`/
/// `Uuid`/etc.'s canonical string rendering both pass through untruncated).
/// `JSON` is a custom no-validator scalar registered once in [`build_schema`].
/// `VECTOR` maps to `[Float!]` (nullable list of non-null components) rather
/// than a scalar, so a client can request individual components without a
/// second round trip.
fn base_type_ref(ty: &ColumnType) -> TypeRef {
    match ty {
        ColumnType::Int64 => TypeRef::named(TypeRef::INT),
        ColumnType::Float => TypeRef::named(TypeRef::FLOAT),
        ColumnType::Bool => TypeRef::named(TypeRef::BOOLEAN),
        ColumnType::Json => TypeRef::named("JSON"),
        ColumnType::Vector(_) => TypeRef::named_nn_list(TypeRef::FLOAT),
        ColumnType::Text
        | ColumnType::Decimal(_, _)
        | ColumnType::Timestamp
        | ColumnType::Uuid
        | ColumnType::Bytea
        | ColumnType::Date
        | ColumnType::Time => TypeRef::named(TypeRef::STRING),
    }
}

/// Whether `ty` gets the REST-parity filter-argument matrix on the root
/// query field. `JSON`/`VECTOR` are excluded — raw-text/array equality on
/// those isn't a meaningful filter shape here (`VECTOR` has its own `near`
/// traversal field instead).
fn filterable(ty: &ColumnType) -> bool {
    !matches!(ty, ColumnType::Json | ColumnType::Vector(_))
}

/// The GraphQL scalar type name used for a filterable column's argument
/// values (distinct from [`base_type_ref`]: filter args are always a bare
/// scalar, never `[Float!]`/`JSON`).
fn filter_scalar_type_name(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Int64 => TypeRef::INT,
        ColumnType::Float => TypeRef::FLOAT,
        ColumnType::Bool => TypeRef::BOOLEAN,
        _ => TypeRef::STRING,
    }
}

/// `Literal` -> `async_graphql::Value`, reusing [`dto::literal_to_json`]'s
/// exact per-variant mapping (the same one `/rest/v1`/`/sql` use) and then
/// converting that JSON through unconditionally — every `Literal` variant
/// (including `Vector`/`Json`/`Decimal`/`Timestamp`/...) already has a
/// well-defined REST-facing JSON shape, so there is no second mapping to
/// keep in sync.
fn literal_to_gql(lit: &Literal) -> GqlValue {
    let json = dto::literal_to_json(lit);
    GqlValue::from_json(json).unwrap_or(GqlValue::Null)
}

// ── row representation (parent value for nested field resolvers) ──────────

/// One fetched row, carried between a list-returning field resolver (root
/// query, FK, `edges`, `near`) and its children's scalar/relationship
/// resolvers via [`FieldValue::owned_any`]/`ctx.parent_value.
/// try_downcast_ref`. Keyed by column name (not position) so it works
/// uniformly for a real table row, a `select *` from a related table, and a
/// synthetic `__edges__` row (`from_id`/`to_id`/`edge_type`/`props`).
struct RowData(HashMap<String, Literal>);

fn row_from(columns: &[String], row: Vec<Literal>) -> RowData {
    let mut map = HashMap::with_capacity(columns.len());
    for (name, val) in columns.iter().zip(row) {
        map.insert(name.clone(), val);
    }
    RowData(map)
}

fn rows_to_field_values(columns: &[String], rows: Vec<Vec<Literal>>) -> Vec<FieldValue<'static>> {
    rows.into_iter()
        .map(|r| FieldValue::owned_any(row_from(columns, r)))
        .collect()
}

// ── error mapping ───────────────────────────────────────────────────────

/// `ApiError` -> `async_graphql::Error`, carrying the same machine-readable
/// code `/rest/v1`/`/sql` would report (`PERMISSION_DENIED`,
/// `TABLE_NOT_FOUND`, ...) as a `"<CODE>: <message>"` prefix in the GraphQL
/// error's `message` — reuses [`map_status`] (the exact `DbError` -> code
/// mapping `error.rs`'s `IntoResponse for ApiError` uses for `/sql`/
/// `/rest/v1`) rather than re-deriving a second mapping.
fn api_err_to_gql(e: ApiError) -> async_graphql::Error {
    match e {
        ApiError::Db(db) => {
            let (_, code) = map_status(&db);
            async_graphql::Error::new(format!("{code}: {db}"))
        }
        ApiError::Api { code, message, .. } => {
            async_graphql::Error::new(format!("{code}: {message}"))
        }
    }
}

// ── filter-argument construction / parsing (root query field) ──────────────

/// A `ParsedOp` tuple-variant constructor, used as a plain function pointer
/// below — factored into a named alias only to satisfy clippy's
/// `type_complexity` lint on the [`FILTER_SUFFIXES`] table.
type ParsedOpCtor = fn(String) -> ParsedOp;

/// Suffixed comparison operators added per filterable column, alongside the
/// bare `<col>` (equality) argument — the same operator set `/rest/v1`
/// exposes as `<op>.<value>` (item 123 C1), just spelled as distinct typed
/// GraphQL arguments instead of one string-encoded query param.
const FILTER_SUFFIXES: [(&str, ParsedOpCtor); 7] = [
    ("_neq", ParsedOp::Neq),
    ("_gt", ParsedOp::Gt),
    ("_gte", ParsedOp::Gte),
    ("_lt", ParsedOp::Lt),
    ("_lte", ParsedOp::Lte),
    ("_like", ParsedOp::Like),
    ("_ilike", ParsedOp::Ilike),
];

/// The per-column filter-argument matrix (`<col>`/`_neq`/`_gt`/.../`_in`/
/// `_is_null`) only — factored out of [`add_filter_arguments`] so item 133's
/// `update_<t>`/`delete_<t>` mutation fields can take the same filter
/// arguments as the root query field *without* also inheriting `orderBy`/
/// `limit`/`offset`, which this engine's `UPDATE`/`DELETE` grammar has no
/// concept of (see `rest_resource.rs`'s module doc on `UPDATE`/`DELETE`'s
/// "simple WHERE" grammar) — accepting them here would silently be a no-op
/// rather than the 400 an unrecognized argument should be.
fn add_column_filter_arguments(mut field: Field, def: &TableDef) -> Field {
    for col in def.columns.iter().filter(|c| !c.dropped) {
        if !filterable(&col.ty) {
            continue;
        }
        let sty = filter_scalar_type_name(&col.ty);
        field = field.argument(InputValue::new(col.name.clone(), TypeRef::named(sty)));
        for (suffix, _) in FILTER_SUFFIXES {
            field = field.argument(InputValue::new(
                format!("{}{suffix}", col.name),
                TypeRef::named(sty),
            ));
        }
        field = field.argument(InputValue::new(
            format!("{}_in", col.name),
            TypeRef::named_nn_list(sty),
        ));
        field = field.argument(InputValue::new(
            format!("{}_is_null", col.name),
            TypeRef::named(TypeRef::BOOLEAN),
        ));
    }
    field
}

fn add_filter_arguments(field: Field, def: &TableDef) -> Field {
    add_column_filter_arguments(field, def)
        .argument(InputValue::new("orderBy", TypeRef::named(TypeRef::STRING)))
        .argument(InputValue::new("limit", TypeRef::named(TypeRef::INT)))
        .argument(InputValue::new("offset", TypeRef::named(TypeRef::INT)))
}

/// A GraphQL scalar argument value -> the raw text [`ParsedOp`]/
/// [`render_filter`][super::rest_resource] binds as a `Literal::Text` —
/// reusing the engine's own implicit text<->column-type coercion
/// (`sql/executor.rs::compare`), exactly the same trick `/rest/v1` relies on
/// for its (always-string) query-param values (see `rest_resource.rs`'s
/// module doc). Typed here only far enough to read the right
/// `ValueAccessor` method for the declared arg type.
fn gql_value_to_filter_string(v: &ValueAccessor, ty: &ColumnType) -> async_graphql::Result<String> {
    match ty {
        ColumnType::Bool => Ok(v.boolean()?.to_string()),
        ColumnType::Int64 => Ok(v.i64()?.to_string()),
        ColumnType::Float => Ok(v.f64()?.to_string()),
        _ => Ok(v.string()?.to_string()),
    }
}

fn collect_filters(
    def: &TableDef,
    args: &async_graphql::dynamic::ObjectAccessor,
) -> async_graphql::Result<Vec<Filter>> {
    let mut filters = Vec::new();
    for col in def.columns.iter().filter(|c| !c.dropped) {
        if !filterable(&col.ty) {
            continue;
        }
        if let Some(v) = args.get(&col.name) {
            filters.push(Filter {
                column: col.name.clone(),
                op: ParsedOp::Eq(gql_value_to_filter_string(&v, &col.ty)?),
            });
        }
        for (suffix, ctor) in FILTER_SUFFIXES {
            if let Some(v) = args.get(&format!("{}{suffix}", col.name)) {
                filters.push(Filter {
                    column: col.name.clone(),
                    op: ctor(gql_value_to_filter_string(&v, &col.ty)?),
                });
            }
        }
        if let Some(v) = args.get(&format!("{}_in", col.name)) {
            let list = v.list()?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list.iter() {
                vals.push(gql_value_to_filter_string(&item, &col.ty)?);
            }
            filters.push(Filter {
                column: col.name.clone(),
                op: ParsedOp::In(vals),
            });
        }
        // `<col>_is_null: false` is not "IS NOT NULL" (no such `ParsedOp`
        // variant, and REST has no equivalent op either) — only `true` adds
        // a filter; `false`/absent means "no constraint from this arg."
        if let Some(v) = args.get(&format!("{}_is_null", col.name)) {
            if v.boolean()? {
                filters.push(Filter {
                    column: col.name.clone(),
                    op: ParsedOp::IsNull,
                });
            }
        }
    }
    Ok(filters)
}

// ── field builders ─────────────────────────────────────────────────────────

fn build_scalar_field(col: &ColumnDef) -> Field {
    let key = col.name.clone();
    Field::new(col.name.clone(), base_type_ref(&col.ty), move |ctx| {
        let key = key.clone();
        FieldFuture::new(async move {
            let row = ctx.parent_value.try_downcast_ref::<RowData>()?;
            let lit = row.0.get(&key).cloned().unwrap_or(Literal::Null);
            Ok(Some(FieldValue::value(literal_to_gql(&lit))))
        })
    })
}

/// Root `<table>(filters..., orderBy, limit, offset): [Type!]!` field — the
/// GraphQL analog of `GET /rest/v1/<table>?...`. Builds and runs exactly one
/// enforced `SELECT` per call, same shape as `rest_resource::get_collection`'s
/// base query (minus C2 embedding, which this module's FK fields cover via
/// nested resolution instead).
fn build_query_field(def: &TableDef) -> Field {
    let closure_def = def.clone();
    let field = Field::new(
        def.name.clone(),
        TypeRef::named_nn_list_nn(def.name.clone()),
        move |ctx| {
            let def = closure_def.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
                let relations = catalog
                    .relations
                    .get(&def.name)
                    .cloned()
                    .unwrap_or_default();

                let filters = collect_filters(&def, &ctx.args)?;
                let mut binds = Vec::new();
                let where_sql = append_where(&filters, &mut binds);
                let projection =
                    requested_projection(ctx.field().selection_set(), &def, &relations);
                let mut sql = format!("SELECT {projection} FROM {}", table_ident(&def.name));
                if let Some(w) = where_sql {
                    sql.push_str(" WHERE ");
                    sql.push_str(&w);
                }
                if let Some(v) = ctx.args.get("orderBy") {
                    let order_keys = parse_order_value(v.string()?);
                    for (c, _) in &order_keys {
                        validate_column(&def, c).map_err(api_err_to_gql)?;
                    }
                    if !order_keys.is_empty() {
                        let order_sql = order_keys
                            .iter()
                            .map(|(c, desc)| {
                                format!("{} {}", quote_ident(c), if *desc { "DESC" } else { "ASC" })
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        sql.push_str(" ORDER BY ");
                        sql.push_str(&order_sql);
                    }
                }
                if let Some(v) = ctx.args.get("limit") {
                    let n = v.i64()?;
                    if n < 0 {
                        return Err(async_graphql::Error::new("limit must be non-negative"));
                    }
                    sql.push_str(&format!(" LIMIT {n}"));
                }
                if let Some(v) = ctx.args.get("offset") {
                    let n = v.i64()?;
                    if n < 0 {
                        return Err(async_graphql::Error::new("offset must be non-negative"));
                    }
                    sql.push_str(&format!(" OFFSET {n}"));
                }

                let result = run_stmt(state, principal, sql, binds)
                    .await
                    .map_err(api_err_to_gql)?;
                let items = match result {
                    ExecResult::Rows { columns, rows } => rows_to_field_values(&columns, rows),
                    _ => Vec::new(),
                };
                Ok(Some(FieldValue::list(items)))
            })
        },
    );
    add_filter_arguments(field, def)
}

// ── mutation field builders (item 133) ──────────────────────────────────────

/// A `values`/`set` `JSON!` argument -> its JSON object body. Reuses
/// `async_graphql::Value::into_json` to convert the already-parsed GraphQL
/// argument (never re-parses text, unlike REST's [`super::rest_resource`]
/// handlers which parse a raw request body) — the
/// resulting `serde_json::Value` then flows through exactly the same
/// [`dto::json_to_literal`] mapping REST's JSON bodies use, so a string,
/// number, bool, object, or array value binds identically either way. An
/// argument that isn't a JSON object (e.g. `values: 5` or `values: [1,2]`) is
/// rejected the same way REST rejects a non-object `PATCH`/`POST` body.
fn json_object_arg(
    args: &async_graphql::dynamic::ObjectAccessor,
    name: &str,
) -> async_graphql::Result<JsonMap<String, JsonValue>> {
    let json = args
        .try_get(name)?
        .as_value()
        .clone()
        .into_json()
        .map_err(|e| async_graphql::Error::new(format!("invalid JSON for `{name}`: {e}")))?;
    match json {
        JsonValue::Object(m) => Ok(m),
        other => Err(async_graphql::Error::new(format!(
            "`{name}` must be a JSON object, got {other}"
        ))),
    }
}

/// Concatenate the `Rows` results of a (possibly multi-statement, see
/// [`build_update_stmts`]/[`build_delete_stmts`]) `run_stmts` call into one
/// flat list of resolver-ready [`FieldValue`]s — the `update_<t>`/
/// `delete_<t>` mutations return one logical list, exactly like
/// `rest_resource::merge_counts` sums one logical count, just concatenating
/// rows instead of summing counts (a mutation's caller wants back *what*
/// changed, where `PATCH`/`DELETE`'s count-based REST response only needs
/// *how many*). A non-`Rows` result (shouldn't happen — every statement built
/// here always carries `RETURNING`) is skipped rather than erroring the
/// whole mutation.
fn merge_returned_rows(results: Vec<ExecResult>) -> Vec<FieldValue<'static>> {
    let mut out = Vec::new();
    for r in results {
        if let ExecResult::Rows { columns, rows } = r {
            out.extend(rows_to_field_values(&columns, rows));
        }
    }
    out
}

/// `insert_<t>(values: JSON!): <T>` — the GraphQL analog of `POST
/// /rest/v1/<t>`. Builds and runs exactly one `INSERT ... RETURNING
/// <requested sub-fields>` through the same enforced [`run_stmt`] path as
/// every other field in this module: `values`' JSON keys are catalog-
/// validated + quoted column names (via [`validate_column`]/[`table_ident`],
/// an unknown column is the same `COLUMN_NOT_FOUND` REST returns), its
/// values are `$n` binds (never interpolated), and the RETURNING projection
/// is exactly the mutation's requested sub-fields (never a blanket `*`) so a
/// caller only needs a `SELECT` grant on what it reads back — this is
/// `Engine::check_returning` (`lib.rs`), the identical machinery `/sql`'s own
/// `RETURNING` clause is authorized through. Single-row only in v1 (a bulk
/// `[JSON!]` variant is a documented non-goal, see the backlog spec) —
/// unlike REST's `POST`, which also accepts a JSON array.
fn build_insert_field(def: &TableDef) -> Field {
    let closure_def = def.clone();
    Field::new(
        format!("insert_{}", def.name),
        TypeRef::named(def.name.clone()),
        move |ctx| {
            let def = closure_def.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
                let relations = catalog
                    .relations
                    .get(&def.name)
                    .cloned()
                    .unwrap_or_default();

                let values = json_object_arg(&ctx.args, "values")?;
                if values.is_empty() {
                    return Err(async_graphql::Error::new(
                        "`values` must have at least one column",
                    ));
                }
                let columns: Vec<String> = values.keys().cloned().collect();
                for c in &columns {
                    validate_column(&def, c).map_err(api_err_to_gql)?;
                }
                let mut binds: Vec<Literal> = Vec::with_capacity(columns.len());
                let placeholders: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        binds.push(dto::json_to_literal(&values[c]));
                        format!("${}", binds.len())
                    })
                    .collect();

                let projection =
                    requested_projection(ctx.field().selection_set(), &def, &relations);
                // Column list deliberately unquoted — same `sql/parser.rs`
                // stringification quirk `rest_resource::build_insert`
                // documents (`convert_insert` re-includes quote characters
                // instead of stripping them); `columns` is already
                // catalog-validated above, so this stays injection-safe.
                let sql = format!(
                    "INSERT INTO {} ({}) VALUES ({}) RETURNING {projection}",
                    table_ident(&def.name),
                    columns.join(", "),
                    placeholders.join(", "),
                );

                let result = run_stmt(state, principal, sql, binds)
                    .await
                    .map_err(api_err_to_gql)?;
                let row = match result {
                    ExecResult::Rows { columns, rows } => rows
                        .into_iter()
                        .next()
                        .map(|r| FieldValue::owned_any(row_from(&columns, r))),
                    _ => None,
                };
                Ok(row)
            })
        },
    )
    .argument(InputValue::new("values", TypeRef::named_nn("JSON")))
}

/// Build the `UPDATE ... RETURNING <projection>` statement(s) for
/// `update_<t>`, mirroring `rest_resource::patch_collection`'s `IN`-list
/// expansion (this engine's `UPDATE` grammar has no native `IN` support —
/// see `rest_resource.rs`'s module doc) but adding a `RETURNING` clause to
/// every generated statement so each expanded statement contributes its
/// updated rows, not just a count.
fn build_update_stmts(
    def: &TableDef,
    assignments: &JsonMap<String, JsonValue>,
    scalar_filters: &[Filter],
    in_filter: Option<InFilter>,
    projection: &str,
) -> Result<Vec<(String, Vec<Literal>)>, ApiError> {
    let one_stmt = |filters: &[Filter]| -> Result<(String, Vec<Literal>), ApiError> {
        let mut binds = Vec::new();
        let assign_sql = build_assignments(def, assignments, &mut binds)?;
        let where_sql = append_where(filters, &mut binds);
        let mut sql = format!("UPDATE {} SET {assign_sql}", table_ident(&def.name));
        if let Some(w) = where_sql {
            sql.push_str(" WHERE ");
            sql.push_str(&w);
        }
        sql.push_str(" RETURNING ");
        sql.push_str(projection);
        Ok((sql, binds))
    };
    match in_filter {
        None => Ok(vec![one_stmt(scalar_filters)?]),
        Some((col, values)) => values
            .into_iter()
            .map(|v| {
                let mut filters = scalar_filters.to_vec();
                filters.push(Filter {
                    column: col.clone(),
                    op: ParsedOp::Eq(v),
                });
                one_stmt(&filters)
            })
            .collect(),
    }
}

/// `update_<t>(<filter args>, set: JSON!): [<T>!]` — the GraphQL analog of
/// `PATCH /rest/v1/<t>?<filters>`. Filter arguments are the exact same typed
/// matrix [`build_query_field`] exposes ([`collect_filters`]/
/// [`add_column_filter_arguments`]); `set`'s JSON keys/values follow
/// [`build_insert_field`]'s column-validated / bind-parameterized rule
/// exactly. Returns every updated row's requested sub-fields via
/// `RETURNING` — see [`build_update_stmts`] for the `IN`-filter multi-
/// statement expansion, run as one atomic call to [`run_stmts`] (one
/// transaction, same as `PATCH`).
fn build_update_field(def: &TableDef) -> Field {
    let closure_def = def.clone();
    let field = Field::new(
        format!("update_{}", def.name),
        TypeRef::named_nn_list(def.name.clone()),
        move |ctx| {
            let def = closure_def.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
                let relations = catalog
                    .relations
                    .get(&def.name)
                    .cloned()
                    .unwrap_or_default();

                let assignments = json_object_arg(&ctx.args, "set")?;
                if assignments.is_empty() {
                    return Err(async_graphql::Error::new(
                        "`set` must contain at least one column assignment",
                    ));
                }

                let filters = collect_filters(&def, &ctx.args)?;
                let (scalar_filters, in_filter) =
                    extract_single_in(filters).map_err(api_err_to_gql)?;
                let projection =
                    requested_projection(ctx.field().selection_set(), &def, &relations);
                let stmts =
                    build_update_stmts(&def, &assignments, &scalar_filters, in_filter, &projection)
                        .map_err(api_err_to_gql)?;

                let results = run_stmts(state, principal, stmts)
                    .await
                    .map_err(api_err_to_gql)?;
                Ok(Some(FieldValue::list(merge_returned_rows(results))))
            })
        },
    );
    add_column_filter_arguments(field, def)
        .argument(InputValue::new("set", TypeRef::named_nn("JSON")))
}

/// [`build_update_stmts`]'s `DELETE` analog — same `IN`-expansion shape,
/// with `RETURNING <projection>` in place of `SET <assignments>`.
fn build_delete_stmts(
    def: &TableDef,
    scalar_filters: &[Filter],
    in_filter: Option<InFilter>,
    projection: &str,
) -> Vec<(String, Vec<Literal>)> {
    let one_stmt = |filters: &[Filter]| -> (String, Vec<Literal>) {
        let mut binds = Vec::new();
        let where_sql = append_where(filters, &mut binds);
        let mut sql = format!("DELETE FROM {}", table_ident(&def.name));
        if let Some(w) = where_sql {
            sql.push_str(" WHERE ");
            sql.push_str(&w);
        }
        sql.push_str(" RETURNING ");
        sql.push_str(projection);
        (sql, binds)
    };
    match in_filter {
        None => vec![one_stmt(scalar_filters)],
        Some((col, values)) => values
            .into_iter()
            .map(|v| {
                let mut filters = scalar_filters.to_vec();
                filters.push(Filter {
                    column: col.clone(),
                    op: ParsedOp::Eq(v),
                });
                one_stmt(&filters)
            })
            .collect(),
    }
}

/// `delete_<t>(<filter args>): [<T>!]` — the GraphQL analog of `DELETE
/// /rest/v1/<t>?<filters>`. Same filter-argument matrix and `RETURNING`-
/// projection rule as [`build_update_field`], minus the `set` argument.
fn build_delete_field(def: &TableDef) -> Field {
    let closure_def = def.clone();
    let field = Field::new(
        format!("delete_{}", def.name),
        TypeRef::named_nn_list(def.name.clone()),
        move |ctx| {
            let def = closure_def.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
                let relations = catalog
                    .relations
                    .get(&def.name)
                    .cloned()
                    .unwrap_or_default();

                let filters = collect_filters(&def, &ctx.args)?;
                let (scalar_filters, in_filter) =
                    extract_single_in(filters).map_err(api_err_to_gql)?;
                let projection =
                    requested_projection(ctx.field().selection_set(), &def, &relations);
                let stmts = build_delete_stmts(&def, &scalar_filters, in_filter, &projection);

                let results = run_stmts(state, principal, stmts)
                    .await
                    .map_err(api_err_to_gql)?;
                Ok(Some(FieldValue::list(merge_returned_rows(results))))
            })
        },
    );
    add_column_filter_arguments(field, def)
}

/// Many-to-one FK field: `alias: RefType` (nullable — `NULL` FK value, or an
/// RLS/grant-hidden parent row, both resolve to `null`, never an error or a
/// leaked row).
fn build_forward_field(alias: &str, fk_column: &str, ref_table: &str, ref_column: &str) -> Field {
    let fk_column = fk_column.to_string();
    let ref_table = ref_table.to_string();
    let ref_column = ref_column.to_string();
    Field::new(alias, TypeRef::named(ref_table.clone()), move |ctx| {
        let fk_column = fk_column.clone();
        let ref_table = ref_table.clone();
        let ref_column = ref_column.clone();
        FieldFuture::new(async move {
            let state = ctx.data::<AppState>()?;
            let principal = ctx.data::<AuthPrincipal>()?;
            let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
            let row = ctx.parent_value.try_downcast_ref::<RowData>()?;
            let fk_val = row.0.get(&fk_column).cloned().unwrap_or(Literal::Null);
            if matches!(fk_val, Literal::Null) {
                return Ok(None);
            }
            let projection = projection_for(ctx.field().selection_set(), catalog, &ref_table);
            let sql = format!(
                "SELECT {projection} FROM {} WHERE {} = $1 LIMIT 1",
                table_ident(&ref_table),
                quote_ident(&ref_column)
            );
            let result = run_stmt(state, principal, sql, vec![fk_val])
                .await
                .map_err(api_err_to_gql)?;
            match result {
                ExecResult::Rows { columns, rows } => Ok(rows
                    .into_iter()
                    .next()
                    .map(|r| FieldValue::owned_any(row_from(&columns, r)))),
                _ => Ok(None),
            }
        })
    })
}

/// One-to-many FK field: `alias: [ChildType!]!` (empty list, never `null`,
/// when the base key is `NULL` or no visible children exist).
fn build_reverse_field(alias: &str, child_table: &str, fk_column: &str, ref_column: &str) -> Field {
    let child_table = child_table.to_string();
    let fk_column = fk_column.to_string();
    let ref_column = ref_column.to_string();
    Field::new(
        alias,
        TypeRef::named_nn_list_nn(child_table.clone()),
        move |ctx| {
            let child_table = child_table.clone();
            let fk_column = fk_column.clone();
            let ref_column = ref_column.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;
                let row = ctx.parent_value.try_downcast_ref::<RowData>()?;
                let base_val = row.0.get(&ref_column).cloned().unwrap_or(Literal::Null);
                if matches!(base_val, Literal::Null) {
                    return Ok(Some(FieldValue::list(Vec::<FieldValue>::new())));
                }
                let projection = projection_for(ctx.field().selection_set(), catalog, &child_table);
                let sql = format!(
                    "SELECT {projection} FROM {} WHERE {} = $1",
                    table_ident(&child_table),
                    quote_ident(&fk_column)
                );
                let result = run_stmt(state, principal, sql, vec![base_val])
                    .await
                    .map_err(api_err_to_gql)?;
                let items = match result {
                    ExecResult::Rows { columns, rows } => rows_to_field_values(&columns, rows),
                    _ => Vec::new(),
                };
                Ok(Some(FieldValue::list(items)))
            })
        },
    )
}

/// The `Edge` object type: `fromId`/`toId`/`edgeType`/`props`, sourced from
/// `__edges__`'s own columns (`from_id`/`to_id`/`edge_type`/`props`).
fn build_edge_type() -> Object {
    Object::new("Edge")
        .field(edge_scalar_field(
            "fromId",
            "from_id",
            TypeRef::named(TypeRef::INT),
        ))
        .field(edge_scalar_field(
            "toId",
            "to_id",
            TypeRef::named(TypeRef::INT),
        ))
        .field(edge_scalar_field(
            "edgeType",
            "edge_type",
            TypeRef::named(TypeRef::STRING),
        ))
        .field(edge_scalar_field("props", "props", TypeRef::named("JSON")))
}

fn edge_scalar_field(name: &str, key: &str, ty: TypeRef) -> Field {
    let key = key.to_string();
    Field::new(name, ty, move |ctx| {
        let key = key.clone();
        FieldFuture::new(async move {
            let row = ctx.parent_value.try_downcast_ref::<RowData>()?;
            let lit = row.0.get(&key).cloned().unwrap_or(Literal::Null);
            Ok(Some(FieldValue::value(literal_to_gql(&lit))))
        })
    })
}

fn build_edge_direction_enum() -> Enum {
    Enum::new("EdgeDirection")
        .item(EnumItem::new("OUT").description("edges where this row is `from_id` (default)"))
        .item(EnumItem::new("IN").description("edges where this row is `to_id`"))
}

/// `edges(type: String, direction: EdgeDirection = OUT): [Edge!]!` — graph
/// traversal as a first-class GraphQL field, the differentiator this
/// workstream exists for. See this module's doc comment for why it runs a
/// `SELECT ... FROM __edges__` through the enforced [`run_stmt`] path
/// (real, fail-closed grant + RLS enforcement) instead of the unauthorized
/// `Engine::edges_from` the lower-level `GET /edges/from/:id` route uses.
fn build_edges_field(pk_column: &str) -> Field {
    let pk_column = pk_column.to_string();
    Field::new("edges", TypeRef::named_nn_list_nn("Edge"), move |ctx| {
        let pk_column = pk_column.clone();
        FieldFuture::new(async move {
            let state = ctx.data::<AppState>()?;
            let principal = ctx.data::<AuthPrincipal>()?;
            let row = ctx.parent_value.try_downcast_ref::<RowData>()?;
            let pk_val = row.0.get(&pk_column).cloned().unwrap_or(Literal::Null);
            let Literal::Int(id) = pk_val else {
                return Ok(Some(FieldValue::list(Vec::<FieldValue>::new())));
            };
            let direction_in = ctx
                .args
                .get("direction")
                .map(|v| v.enum_name().map(|s| s == "IN").unwrap_or(false))
                .unwrap_or(false);
            let id_col = if direction_in { "to_id" } else { "from_id" };
            let mut binds = vec![Literal::Int(id)];
            let mut sql = format!(
                "SELECT from_id, to_id, edge_type, props FROM {EDGES_TABLE} WHERE {id_col} = $1"
            );
            if let Some(v) = ctx.args.get("type") {
                binds.push(Literal::Text(v.string()?.to_string()));
                sql.push_str(&format!(" AND edge_type = ${}", binds.len()));
            }
            let result = run_stmt(state, principal, sql, binds)
                .await
                .map_err(api_err_to_gql)?;
            let items = match result {
                ExecResult::Rows { columns, rows } => rows_to_field_values(&columns, rows),
                _ => Vec::new(),
            };
            Ok(Some(FieldValue::list(items)))
        })
    })
    .argument(InputValue::new("type", TypeRef::named(TypeRef::STRING)))
    .argument(InputValue::new(
        "direction",
        TypeRef::named("EdgeDirection"),
    ))
}

/// `near`/`near_<col>(vector: [Float!]!, k: Int!): [Type!]!` — vector
/// similarity as a first-class GraphQL field, reusing the exact `WHERE
/// NEAR(...)` predicate `sql/executor.rs::exec_select_near` already knows
/// how to run (see this module's doc comment for why `vector`/`k` are
/// formatted as SQL literals rather than bind params).
fn build_near_field(field_name: &str, table_name: &str, target_type: &str, column: &str) -> Field {
    let table_name = table_name.to_string();
    let column = column.to_string();
    Field::new(
        field_name,
        TypeRef::named_nn_list_nn(target_type),
        move |ctx| {
            let table_name = table_name.clone();
            let column = column.clone();
            FieldFuture::new(async move {
                let state = ctx.data::<AppState>()?;
                let principal = ctx.data::<AuthPrincipal>()?;
                let catalog = ctx.data::<std::sync::Arc<SchemaCatalog>>()?;

                let list = ctx.args.try_get("vector")?.list()?;
                let mut floats: Vec<f32> = Vec::with_capacity(list.len());
                for item in list.iter() {
                    let f = item.f32()?;
                    if !f.is_finite() {
                        return Err(async_graphql::Error::new(
                            "vector components must be finite numbers",
                        ));
                    }
                    floats.push(f);
                }
                if floats.is_empty() {
                    return Err(async_graphql::Error::new("vector must not be empty"));
                }
                let k = ctx.args.try_get("k")?.i64()?;
                if k < 0 {
                    return Err(async_graphql::Error::new("k must be non-negative"));
                }

                let vec_sql = floats
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let projection = projection_for(ctx.field().selection_set(), catalog, &table_name);
                let sql = format!(
                    "SELECT {projection} FROM {} WHERE NEAR({}, [{}], {})",
                    table_ident(&table_name),
                    quote_ident(&column),
                    vec_sql,
                    k
                );
                let result = run_stmt(state, principal, sql, Vec::new())
                    .await
                    .map_err(api_err_to_gql)?;
                let items = match result {
                    ExecResult::Rows { columns, rows } => rows_to_field_values(&columns, rows),
                    _ => Vec::new(),
                };
                Ok(Some(FieldValue::list(items)))
            })
        },
    )
    .argument(InputValue::new(
        "vector",
        TypeRef::named_nn_list_nn(TypeRef::FLOAT),
    ))
    .argument(InputValue::new("k", TypeRef::named_nn(TypeRef::INT)))
}
